//! Per-connection MCP initialization, request routing, and validation boundaries.

use super::*;

const UNAVAILABLE_TOOL: &str = "Tool is unavailable";

/// Route a decoded message without performing I/O or executing a tool.
///
/// Configuration is checked even for notifications and hidden endpoints. The
/// two initialization transitions commit only after their action is complete.
pub fn route(
    session: &mut Session,
    message: &McpEnvelope,
    server: &ServerInfo,
    services: &[ServiceSpec],
    roles: &[String],
    limits: EnvelopeLimits,
) -> McpResult<McpAction> {
    let id = message.id.as_ref();
    validate_server(server)
        .and_then(|()| validate_catalog(services, roles))
        .and_then(|()| validate_limits(limits))
        .map_err(|_| failure(id, INTERNAL_ERROR, "Invalid server configuration"))?;

    if session.phase == SessionPhase::Closed && id.is_some() {
        return Err(failure(id, SERVER_NOT_INITIALIZED, "Session is closed"));
    }

    if message.method == "notifications/initialized" {
        if id.is_some() || session.phase != SessionPhase::AwaitingInitialized {
            return Err(failure(id, INVALID_REQUEST, "Unexpected initialized notification"));
        }
        validate_empty_params(message.params.as_ref(), limits)
            .map_err(|error| parameter_failure(id, error, INVALID_REQUEST))?;
        session.phase = SessionPhase::Ready;
        return Ok(McpAction::Initialized);
    }

    // Request-only names sent as notifications cannot advance initialization,
    // reveal catalog entries, or cause argument validation or execution.
    let Some(id) = id else {
        return Ok(McpAction::IgnoreNotification);
    };

    match message.method.as_str() {
        "initialize" => {
            if session.phase != SessionPhase::New {
                return Err(failure(Some(id), INVALID_REQUEST, "Session already initialized"));
            }
            let (offered, client) = initialize_params(message.params.as_ref(), limits)
                .map_err(|error| parameter_failure(Some(id), error, INVALID_PARAMS))?;
            let selected = server.protocol_versions.iter().find(|version| **version == offered)
                .unwrap_or(&server.protocol_versions[0]);
            let stored_version = copy_string(selected)
                .map_err(|error| parameter_failure(Some(id), error, INVALID_PARAMS))?;
            let action_version = copy_string(selected)
                .map_err(|error| parameter_failure(Some(id), error, INVALID_PARAMS))?;
            let action = McpAction::Initialize {
                id: id.clone(),
                protocol_version: action_version,
            };
            session.protocol_version = Some(stored_version);
            session.client = Some(client);
            session.phase = SessionPhase::AwaitingInitialized;
            Ok(action)
        }
        "ping" => {
            validate_empty_params(message.params.as_ref(), limits)
                .map_err(|error| parameter_failure(Some(id), error, INVALID_PARAMS))?;
            Ok(McpAction::Ping { id: id.clone() })
        }
        "tools/list" | "tools/call" => {
            if session.phase != SessionPhase::Ready {
                return Err(failure(Some(id), SERVER_NOT_INITIALIZED, "Server is not initialized"));
            }
            if !server.capabilities.tools {
                return Err(failure(Some(id), METHOD_NOT_FOUND, "Method not found"));
            }
            if message.method == "tools/list" {
                validate_empty_params(message.params.as_ref(), limits)
                    .map_err(|error| parameter_failure(Some(id), error, INVALID_PARAMS))?;
                return Ok(McpAction::ListTools { id: id.clone() });
            }

            let (name, arguments) = call_params(message.params.as_ref(), limits)
                .map_err(|error| parameter_failure(Some(id), error, INVALID_PARAMS))?;
            for endpoint in services.iter().flat_map(|service| service.endpoints.iter()) {
                let candidate = tool_name(&endpoint.service, &endpoint.name)
                    .map_err(|_| failure(Some(id), INTERNAL_ERROR, "Invalid tool catalog"))?;
                if candidate == name {
                    if !role_allowed(endpoint, roles) {
                        return Err(failure(Some(id), INVALID_PARAMS, UNAVAILABLE_TOOL));
                    }
                    return Ok(McpAction::CallTool {
                        id: id.clone(), endpoint_code: endpoint.code, arguments,
                    });
                }
            }
            Err(failure(Some(id), INVALID_PARAMS, UNAVAILABLE_TOOL))
        }
        _ => Err(failure(Some(id), METHOD_NOT_FOUND, "Method not found")),
    }
}

/// Reauthorize a routed call and bind the validator's output to its input bytes.
///
/// The validator owns schema selection and proof issuance. This boundary checks
/// storage consistency without guessing an endpoint schema fingerprint.
pub fn validate_call(
    action: &McpAction,
    services: &[ServiceSpec],
    roles: &[String],
    validator: &dyn ToolArgumentsValidator,
) -> McpResult<ValidatedSlots> {
    let (id, code, arguments) = match action {
        McpAction::CallTool { id, endpoint_code, arguments } => (id, *endpoint_code, arguments),
        _ => return Err(failure(action_id(action), INVALID_REQUEST, "Expected a tool call")),
    };
    validate_catalog(services, roles)
        .map_err(|_| failure(Some(id), INTERNAL_ERROR, "Invalid tool catalog"))?;
    let endpoint = services.iter().flat_map(|service| service.endpoints.iter())
        .find(|endpoint| endpoint.code == code)
        .filter(|endpoint| role_allowed(endpoint, roles))
        .ok_or_else(|| failure(Some(id), INVALID_PARAMS, UNAVAILABLE_TOOL))?;

    let validated = validator.validate_arguments(endpoint, arguments.clone())
        .map_err(|_| failure(Some(id), INVALID_PARAMS, "Invalid tool arguments"))?;
    if validated.payload.as_slice() != arguments.as_slice()
        || usize::try_from(validated.proof.payload_len()).ok() != Some(validated.payload.len())
        || usize::try_from(validated.proof.decoded_len()).ok() != Some(validated.decoded.len())
    {
        return Err(failure(Some(id), INTERNAL_ERROR, "Invalid argument validation ownership"));
    }
    Ok(validated)
}

fn action_id(action: &McpAction) -> Option<&McpId> {
    match action {
        McpAction::Initialize { id, .. } | McpAction::Ping { id }
        | McpAction::ListTools { id } | McpAction::CallTool { id, .. } => Some(id),
        McpAction::Initialized | McpAction::IgnoreNotification => None,
    }
}

fn local_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Wire, 0, detail)
}

fn parameter_failure(id: Option<&McpId>, error: Error, invalid_code: i32) -> McpFailure {
    if matches!(error.code, ErrorCode::BudgetExhausted | ErrorCode::PayloadTooLarge
        | ErrorCode::DecodedArenaOverflow | ErrorCode::DepthLimitExceeded | ErrorCode::InternalError)
    {
        failure(id, INTERNAL_ERROR, "Request resource limit exceeded")
    } else {
        failure(id, invalid_code, "Invalid method parameters")
    }
}

fn copy_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len())
        .map_err(|_| local_error(ErrorCode::BudgetExhausted, "string allocation failed"))?;
    owned.push_str(value);
    Ok(owned)
}

fn validate_limits(limits: EnvelopeLimits) -> Result<()> {
    if limits.max_payload_bytes == 0 || limits.max_decoded_bytes == 0
        || limits.max_depth == 0 || limits.max_fields == 0
    {
        return Err(local_error(ErrorCode::ConstraintViolation, "JSON limits must be nonzero"));
    }
    Ok(())
}

fn validate_server(server: &ServerInfo) -> Result<()> {
    if server.implementation.name.is_empty() || server.implementation.version.is_empty()
        || server.protocol_versions.is_empty()
        || (server.capabilities.tools_list_changed && !server.capabilities.tools)
    {
        return Err(local_error(ErrorCode::ConstraintViolation, "invalid server configuration"));
    }
    for (index, version) in server.protocol_versions.iter().enumerate() {
        if version.is_empty() {
            return Err(local_error(ErrorCode::ConstraintViolation, "empty protocol version"));
        }
        if server.protocol_versions[..index].contains(version) {
            return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate protocol version"));
        }
    }
    Ok(())
}

fn validate_roles(roles: &[String]) -> Result<()> {
    for (index, role) in roles.iter().enumerate() {
        if role.is_empty() {
            return Err(local_error(ErrorCode::ConstraintViolation, "empty role"));
        }
        if roles[..index].contains(role) {
            return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate role"));
        }
    }
    Ok(())
}

fn validate_catalog(services: &[ServiceSpec], roles: &[String]) -> Result<()> {
    validate_roles(roles)?;
    for (service_index, service) in services.iter().enumerate() {
        // Validate service identifiers even when their endpoint lists are empty.
        tool_name(&service.name, "_")?;
        if services[..service_index].iter().any(|previous| {
            previous.id == service.id || previous.name == service.name
        }) {
            return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate service identity"));
        }
        for (endpoint_index, endpoint) in service.endpoints.iter().enumerate() {
            if endpoint.service != service.name {
                return Err(local_error(ErrorCode::ConstraintViolation, "endpoint service mismatch"));
            }
            tool_name(&endpoint.service, &endpoint.name)?;
            validate_roles(&endpoint.roles)?;
            if service.endpoints[..endpoint_index].iter().any(|previous| {
                previous.name == endpoint.name || previous.code == endpoint.code
            }) || services[..service_index].iter().any(|previous| {
                previous.endpoints.iter().any(|other| other.code == endpoint.code)
            }) {
                return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate endpoint identity"));
            }
        }
    }
    // Unique service names and endpoint names, with dot-free identifiers,
    // establish uniqueness of every root tool_name projection as well.
    Ok(())
}

fn require_object(payload: &Payload, limits: EnvelopeLimits) -> Result<()> {
    envelope::validate_json(payload, limits)?;
    if payload.as_slice().iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(local_error(ErrorCode::TypeMismatch, "expected parameter object"));
    }
    Ok(())
}

/// Check a closed field set using the existing parser for decoded key matching.
/// After complete JSON validation, each colon at root object depth counts one
/// field. Colons inside strings and nested containers cannot affect the count.
/// Duplicate decoded keys have already been rejected, so matching every field
/// to a distinct allowed name proves that no unknown fields remain.
fn closed_fields<const N: usize>(
    payload: &Payload, names: [&str; N], limits: EnvelopeLimits,
) -> Result<[Option<Payload>; N]> {
    require_object(payload, limits)?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut count = 0usize;
    for &byte in payload.as_slice() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth -= 1,
                b':' if depth == 1 => count += 1,
                _ => {}
            }
        }
    }
    let mut fields: [Option<Payload>; N] = core::array::from_fn(|_| None);
    let mut matched = 0;
    for (index, name) in names.iter().enumerate() {
        fields[index] = envelope::object_field(payload, name, limits)?;
        if fields[index].is_some() {
            matched += 1;
        }
    }
    if count != matched {
        return Err(local_error(ErrorCode::UnknownObjectKey, "unknown method parameter"));
    }
    Ok(fields)
}

fn required(field: Option<Payload>) -> Result<Payload> {
    field.ok_or_else(|| local_error(ErrorCode::MissingRequiredField, "missing method parameter"))
}

fn metadata(field: Option<Payload>, limits: EnvelopeLimits) -> Result<()> {
    if let Some(field) = field {
        require_object(&field, limits)?;
    }
    Ok(())
}

fn validate_empty_params(params: Option<&Payload>, limits: EnvelopeLimits) -> Result<()> {
    if let Some(params) = params {
        let [meta] = closed_fields(params, ["_meta"], limits)?;
        metadata(meta, limits)?;
    }
    Ok(())
}

fn initialize_params(
    params: Option<&Payload>, limits: EnvelopeLimits,
) -> Result<(String, ImplementationInfo)> {
    let params = params.ok_or_else(|| {
        local_error(ErrorCode::MissingRequiredField, "missing initialization parameters")
    })?;
    let [version, capabilities, client, meta] = closed_fields(
        params, ["protocolVersion", "capabilities", "clientInfo", "_meta"], limits,
    )?;
    let version = envelope::decode_string(&required(version)?, limits)?;
    require_object(&required(capabilities)?, limits)?;
    let [name, client_version] = closed_fields(&required(client)?, ["name", "version"], limits)?;
    let name = envelope::decode_string(&required(name)?, limits)?;
    let client_version = envelope::decode_string(&required(client_version)?, limits)?;
    if name.is_empty() || client_version.is_empty() {
        return Err(local_error(ErrorCode::ConstraintViolation, "empty client identity"));
    }
    metadata(meta, limits)?;
    Ok((version, ImplementationInfo { name, version: client_version }))
}

fn call_params(params: Option<&Payload>, limits: EnvelopeLimits) -> Result<(String, Payload)> {
    let params = params.ok_or_else(|| {
        local_error(ErrorCode::MissingRequiredField, "missing tool parameters")
    })?;
    let [name, arguments, meta] = closed_fields(params, ["name", "arguments", "_meta"], limits)?;
    let name = envelope::decode_string(&required(name)?, limits)?;
    metadata(meta, limits)?;
    let arguments = match arguments {
        Some(arguments) => arguments,
        None => {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(2)
                .map_err(|_| local_error(ErrorCode::BudgetExhausted, "argument allocation failed"))?;
            bytes.extend_from_slice(b"{}");
            Payload::from_vec(bytes)
        }
    };
    require_object(&arguments, limits)?;
    Ok((name, arguments))
}
