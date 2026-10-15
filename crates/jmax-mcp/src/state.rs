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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec};
    use core::cell::Cell;
    use jmax_abi::buffer::ValidationProof;

    fn server() -> ServerInfo {
        ServerInfo {
            implementation: ImplementationInfo {
                name: String::from("test-server"), version: String::from("1"),
            },
            protocol_versions: vec![String::from("preferred"), String::from(DEFAULT_PROTOCOL_VERSION)],
            capabilities: ServerCapabilities { tools: true, tools_list_changed: true },
            instructions: None,
        }
    }

    fn endpoint(service: &str, name: &str, code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, service: String::from(service), name: String::from(name),
            roles: roles.iter().map(|role| String::from(*role)).collect(),
            params: Vec::new(), response: Vec::new(), stream_response: None,
            description: String::from("test endpoint"),
        }
    }

    fn catalog() -> Vec<ServiceSpec> {
        vec![ServiceSpec {
            id: 0, name: String::from("svc"),
            endpoints: vec![
                endpoint("svc", "public", 0, &[]),
                endpoint("svc", "private", u32::MAX, &["admin", "reader"]),
            ],
        }]
    }

    fn message(method: &str, params: Option<&str>) -> McpEnvelope {
        McpEnvelope {
            id: Some(McpId::Number(7)), method: String::from(method),
            params: params.map(|raw| Payload::from_slice(raw.as_bytes())),
        }
    }

    fn notification(method: &str, params: Option<&str>) -> McpEnvelope {
        let mut message = message(method, params);
        message.id = None;
        message
    }

    fn init(version: &str) -> McpEnvelope {
        message("initialize", Some(&alloc::format!(
            r#"{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"client","version":"2"}}}}"#,
        )))
    }

    fn session(phase: SessionPhase) -> Session {
        Session {
            phase, protocol_version: Some(String::from("preferred")),
            client: Some(ImplementationInfo { name: String::from("client"), version: String::from("2") }),
        }
    }

    fn dispatch(session: &mut Session, message: &McpEnvelope) -> McpResult<McpAction> {
        route(session, message, &server(), &catalog(), &[], EnvelopeLimits::default())
    }

    fn rejected(session: &mut Session, message: &McpEnvelope, code: i32) -> McpFailure {
        let before = session.clone();
        let error = dispatch(session, message).unwrap_err();
        assert_eq!(error.code, code, "method: {}, params: {:?}", message.method, message.params);
        assert_eq!(error.id, message.id.clone().unwrap_or(McpId::Null));
        assert_eq!(*session, before);
        assert!(error.message.len() <= 64);
        error
    }

    #[test]
    fn initialize_negotiates_explicit_versions_and_falls_back_in_preference_order() {
        for (offered, expected) in [
            (DEFAULT_PROTOCOL_VERSION, DEFAULT_PROTOCOL_VERSION),
            ("preferred", "preferred"), ("future-version", "preferred"),
            ("", "preferred"), ("PREFERRED", "preferred"),
        ] {
            let mut state = Session::default();
            let action = dispatch(&mut state, &init(offered)).unwrap();
            assert!(matches!(action, McpAction::Initialize {
                id: McpId::Number(7), protocol_version,
            } if protocol_version == expected));
            assert_eq!(state.phase(), SessionPhase::AwaitingInitialized);
            assert_eq!(state.protocol_version(), Some(expected));
            assert_eq!(state.client().unwrap().name, "client");
            assert_eq!(state.client().unwrap().version, "2");
            assert!(matches!(dispatch(&mut state, &notification("notifications/initialized", None)).unwrap(), McpAction::Initialized));
            assert_eq!(state.phase(), SessionPhase::Ready);
        }
    }

    #[test]
    fn initialize_decodes_fields_and_owns_client_information() {
        let mut state = Session::default();
        let request = message("initialize", Some(
            r#"{"protocol\u0056ersion":"preferred","capabilities":{"future":{"x":[1,true,null]}},"clientInfo":{"na\u006de":"客户端🚀","version":"\u0032"},"_meta":{"a:b":"\"{}[]:"}}"#,
        ));
        let action = dispatch(&mut state, &request).unwrap();
        drop(request);
        assert!(matches!(action, McpAction::Initialize { .. }));
        assert_eq!(state.client().unwrap().name, "客户端🚀");
        assert_eq!(state.client().unwrap().version, "2");
    }

    #[test]
    fn malformed_initialize_parameters_never_mutate_the_session() {
        for params in [
            None, Some("{}"), Some("[]"), Some("null"),
            Some(r#"{"protocolVersion":1,"capabilities":{},"clientInfo":{"name":"c","version":"v"}}"#),
            Some(r#"{"protocolVersion":"v","clientInfo":{"name":"c","version":"v"}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":[],"clientInfo":{"name":"c","version":"v"}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":[]}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":"","version":"v"}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":"c","version":""}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":"c"}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":true,"version":"v"}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":"c","version":"v","extra":0}}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":"c","version":"v"},"extra":0}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{},"clientInfo":{"name":"c","version":"v"},"_meta":null}"#),
            Some(r#"{"protocolVersion":"v","capabilities":{"x":0,"x":1},"clientInfo":{"name":"c","version":"v"}}"#),
            Some(r#"{"protocolVersion":"v","protocol\u0056ersion":"v","capabilities":{},"clientInfo":{"name":"c","version":"v"}}"#),
        ] {
            rejected(&mut Session::default(), &message("initialize", params), INVALID_PARAMS);
        }
    }

    #[test]
    fn initialized_requires_the_correct_phase_notification_shape_and_metadata() {
        for phase in [SessionPhase::New, SessionPhase::Ready, SessionPhase::Closed] {
            rejected(&mut session(phase), &notification("notifications/initialized", None), INVALID_REQUEST);
        }
        rejected(&mut session(SessionPhase::AwaitingInitialized), &message("notifications/initialized", None), INVALID_REQUEST);
        for params in [Some("[]"), Some("null"), Some(r#"{"cursor":"x"}"#), Some(r#"{"_meta":[]}"#), Some(r#"{"_meta":{"x":0,"x":1}}"#)] {
            rejected(&mut session(SessionPhase::AwaitingInitialized), &notification("notifications/initialized", params), INVALID_REQUEST);
        }
        for params in [None, Some("{}"), Some(r#"{"_meta":{"nested":[{},"x"]}}"#)] {
            let mut state = session(SessionPhase::AwaitingInitialized);
            let before = state.clone();
            assert!(matches!(dispatch(&mut state, &notification("notifications/initialized", params)).unwrap(), McpAction::Initialized));
            assert_eq!(state.phase(), SessionPhase::Ready);
            assert_eq!(state.client, before.client);
            assert_eq!(state.protocol_version, before.protocol_version);
            rejected(&mut state, &notification("notifications/initialized", None), INVALID_REQUEST);
        }
    }

    #[test]
    fn repeated_initialize_and_closed_requests_preserve_state() {
        for phase in [SessionPhase::AwaitingInitialized, SessionPhase::Ready] {
            rejected(&mut session(phase), &init("preferred"), INVALID_REQUEST);
        }
        for method in ["initialize", "ping", "tools/list", "tools/call", "notifications/initialized", "unknown"] {
            rejected(&mut session(SessionPhase::Closed), &message(method, None), SERVER_NOT_INITIALIZED);
        }
        let mut state = session(SessionPhase::Ready);
        state.close();
        rejected(&mut state, &message("ping", None), SERVER_NOT_INITIALIZED);
    }

    #[test]
    fn ping_accepts_all_nonclosed_phases_without_initializing() {
        for phase in [SessionPhase::New, SessionPhase::AwaitingInitialized, SessionPhase::Ready] {
            for params in [None, Some("{}"), Some(" \t{\"_meta\":{\"x\":[1]}}\n")] {
                let mut state = session(phase);
                let before = state.clone();
                assert!(matches!(dispatch(&mut state, &message("ping", params)).unwrap(), McpAction::Ping { id: McpId::Number(7) }));
                assert_eq!(state, before);
            }
        }
        for params in ["[]", "null", r#"{"cursor":"next"}"#, r#"{"_meta":0}"#, r#"{"extra":{}}"#] {
            rejected(&mut Session::default(), &message("ping", Some(params)), INVALID_PARAMS);
        }
    }

    #[test]
    fn request_ids_preserve_null_unicode_and_integer_boundaries() {
        for id in [McpId::Null, McpId::Number(i64::MIN), McpId::Number(i64::MAX), McpId::String(String::from("\0é🚀"))] {
            let mut request = init("preferred");
            request.id = Some(id.clone());
            let mut state = Session::default();
            match dispatch(&mut state, &request).unwrap() {
                McpAction::Initialize { id: returned, .. } => assert_eq!(returned, id),
                other => panic!("unexpected action: {other:?}"),
            }
            request.method = String::from("ping");
            request.params = None;
            assert!(matches!(dispatch(&mut state, &request).unwrap(), McpAction::Ping { id: returned } if returned == id));
            request.method = String::from("unknown");
            assert_eq!(rejected(&mut state, &request, METHOD_NOT_FOUND).id, id);
        }
    }

    #[test]
    fn request_only_notifications_are_ignored_including_on_closed_sessions() {
        for phase in [SessionPhase::New, SessionPhase::AwaitingInitialized, SessionPhase::Ready, SessionPhase::Closed] {
            for method in ["initialize", "ping", "tools/list", "tools/call", "unknown", "Notifications/initialized"] {
                let mut state = session(phase);
                let before = state.clone();
                assert!(matches!(dispatch(&mut state, &notification(method, Some("[]"))).unwrap(), McpAction::IgnoreNotification));
                assert_eq!(state, before);
            }
        }
    }

    #[test]
    fn tool_methods_require_readiness_and_enabled_capability() {
        for method in ["tools/list", "tools/call"] {
            for phase in [SessionPhase::New, SessionPhase::AwaitingInitialized] {
                rejected(&mut session(phase), &message(method, None), SERVER_NOT_INITIALIZED);
            }
            let mut disabled = server();
            disabled.capabilities = ServerCapabilities::default();
            let mut state = session(SessionPhase::Ready);
            let before = state.clone();
            let error = route(&mut state, &message(method, None), &disabled, &catalog(), &[], EnvelopeLimits::default()).unwrap_err();
            assert_eq!(error.code, METHOD_NOT_FOUND);
            assert_eq!(state, before);
        }
        for phase in [SessionPhase::New, SessionPhase::AwaitingInitialized, SessionPhase::Ready] {
            rejected(&mut session(phase), &message("Tools/list", None), METHOD_NOT_FOUND);
        }
    }

    #[test]
    fn list_tools_has_no_pagination_and_accepts_only_metadata() {
        for params in [None, Some("{}"), Some(r#"{"_meta":{"trace":"x"}}"#)] {
            assert!(matches!(dispatch(&mut session(SessionPhase::Ready), &message("tools/list", params)).unwrap(), McpAction::ListTools { id: McpId::Number(7) }));
        }
        for params in [r#"{"cursor":null}"#, r#"{"cursor":""}"#, "[]", r#"{"_meta":null}"#] {
            rejected(&mut session(SessionPhase::Ready), &message("tools/list", Some(params)), INVALID_PARAMS);
        }
    }

    #[test]
    fn calls_own_exact_argument_bytes_and_default_to_an_object() {
        for (params, expected) in [
            (r#"{"name":"svc.public"}"#, "{}"),
            (r#"{"name":"svc.public","arguments": { "a" : [1e2,"\u0062",{"x":true}] } ,"_meta":{}}"#, r#"{ "a" : [1e2,"\u0062",{"x":true}] }"#),
        ] {
            let request = message("tools/call", Some(params));
            let mut state = session(SessionPhase::Ready);
            let before = state.clone();
            let action = dispatch(&mut state, &request).unwrap();
            drop(request);
            match action {
                McpAction::CallTool { id, endpoint_code, arguments } => {
                    assert_eq!(id, McpId::Number(7));
                    assert_eq!(endpoint_code, 0);
                    assert_eq!(arguments.as_slice(), expected.as_bytes());
                }
                other => panic!("unexpected action: {other:?}"),
            }
            assert_eq!(state, before);
        }
    }

    #[test]
    fn call_parameters_are_closed_and_strictly_typed() {
        for params in [
            None, Some("[]"), Some("{}"), Some(r#"{"name":null}"#),
            Some(r#"{"name":"svc.public","arguments":null}"#),
            Some(r#"{"name":"svc.public","arguments":[]}"#),
            Some(r#"{"name":"svc.public","arguments":"{}"}"#),
            Some(r#"{"name":"svc.public","arguments":{},"extra":0}"#),
            Some(r#"{"name":"svc.public","_meta":false}"#),
            Some(r#"{"name":"svc.public","arguments":{"x":1,"\u0078":2}}"#),
            Some(r#"{"name":"svc.public","name":"svc.public"}"#),
        ] {
            rejected(&mut session(SessionPhase::Ready), &message("tools/call", params), INVALID_PARAMS);
        }
    }

    #[test]
    fn forbidden_and_unknown_names_are_indistinguishable_and_roles_are_exact_any_match() {
        let unknown = rejected(&mut session(SessionPhase::Ready), &message("tools/call", Some(r#"{"name":"absent"}"#)), INVALID_PARAMS);
        let forbidden = rejected(&mut session(SessionPhase::Ready), &message("tools/call", Some(r#"{"name":"svc.private"}"#)), INVALID_PARAMS);
        assert_eq!(unknown, forbidden);
        for (roles, accepted) in [
            (vec![String::from("admin")], true),
            (vec![String::from("irrelevant"), String::from("reader")], true),
            (vec![String::from("Admin")], false),
            (vec![String::from("admin ")], false),
        ] {
            let result = route(
                &mut session(SessionPhase::Ready),
                &message("tools/call", Some(r#"{"name":"svc.private"}"#)),
                &server(), &catalog(), &roles, EnvelopeLimits::default(),
            );
            if accepted {
                assert!(matches!(result.unwrap(), McpAction::CallTool { endpoint_code: u32::MAX, .. }));
            } else {
                assert_eq!(result.unwrap_err(), forbidden);
            }
        }
        for name in ["Svc.public", "svc.Public", "svc.public ", ""] {
            assert_eq!(rejected(&mut session(SessionPhase::Ready), &message("tools/call", Some(&alloc::format!(r#"{{"name":"{name}"}}"#))), INVALID_PARAMS), unknown);
        }
    }

    #[test]
    fn invalid_server_configuration_precedes_routing_and_never_mutates() {
        let baseline = server();
        let mut configurations = Vec::new();
        let mut value = baseline.clone(); value.implementation.name.clear(); configurations.push(value);
        let mut value = baseline.clone(); value.implementation.version.clear(); configurations.push(value);
        let mut value = baseline.clone(); value.protocol_versions.clear(); configurations.push(value);
        let mut value = baseline.clone(); value.protocol_versions.push(String::new()); configurations.push(value);
        let mut value = baseline.clone(); value.protocol_versions.push(String::from("preferred")); configurations.push(value);
        let mut value = baseline; value.capabilities.tools = false; configurations.push(value);
        for config in configurations {
            for request in [init("preferred"), notification("unknown", None)] {
                let mut state = Session::default();
                let before = state.clone();
                let error = route(&mut state, &request, &config, &[], &[], EnvelopeLimits::default()).unwrap_err();
                assert_eq!(error.code, INTERNAL_ERROR);
                assert_eq!(state, before);
            }
        }
    }

    fn invalid_catalogs() -> Vec<Vec<ServiceSpec>> {
        let mut cases = Vec::new();
        let mut value = catalog(); value[0].name.clear(); cases.push(value);
        let mut value = catalog(); value[0].name = String::from("invalid.name"); value[0].endpoints.clear(); cases.push(value);
        let mut value = catalog(); value[0].endpoints[0].service = String::from("other"); cases.push(value);
        for name in ["", "0bad", "bad-name", "é", "a.b"] {
            let mut value = catalog(); value[0].endpoints[0].name = String::from(name); cases.push(value);
        }
        let mut value = catalog(); value[0].endpoints[1].name = String::from("public"); cases.push(value);
        let mut value = catalog(); value[0].endpoints[1].code = 0; cases.push(value);
        let mut value = catalog(); value[0].endpoints[1].roles = vec![String::new()]; cases.push(value);
        let mut value = catalog(); value[0].endpoints[1].roles = vec![String::from("admin"), String::from("admin")]; cases.push(value);
        let mut value = catalog(); value.push(ServiceSpec { id: 0, name: String::from("other"), endpoints: Vec::new() }); cases.push(value);
        let mut value = catalog(); value.push(ServiceSpec { id: 1, name: String::from("svc"), endpoints: Vec::new() }); cases.push(value);
        let mut value = catalog(); value.push(ServiceSpec { id: 1, name: String::from("other"), endpoints: vec![endpoint("other", "public", 0, &[])] }); cases.push(value);
        cases
    }

    #[test]
    fn global_catalog_validation_includes_hidden_and_unused_entries() {
        for services in invalid_catalogs() {
            for request in [init("preferred"), message("ping", None), notification("unknown", None)] {
                let mut state = Session::default();
                let before = state.clone();
                let error = route(&mut state, &request, &server(), &services, &[], EnvelopeLimits::default()).unwrap_err();
                assert_eq!(error.code, INTERNAL_ERROR);
                assert_eq!(state, before);
            }
        }
        for roles in [vec![String::new()], vec![String::from("x"), String::from("x")]] {
            let mut state = Session::default();
            assert_eq!(route(&mut state, &init("preferred"), &server(), &catalog(), &roles, EnvelopeLimits::default()).unwrap_err().code, INTERNAL_ERROR);
            assert_eq!(state, Session::default());
        }
    }

    #[test]
    fn empty_catalogs_and_boundary_identifiers_are_valid() {
        let services = vec![
            ServiceSpec { id: 0, name: String::from("_"), endpoints: vec![endpoint("_", "_0", 0, &[])] },
            ServiceSpec { id: u16::MAX, name: String::from("Z9"), endpoints: vec![endpoint("Z9", "_0", u32::MAX, &["Admin", "admin"])] },
        ];
        assert!(validate_catalog(&services, &[String::from("Admin"), String::from("admin")]).is_ok());
        assert!(validate_catalog(&[], &[]).is_ok());
        assert!(matches!(route(&mut Session::default(), &init("preferred"), &server(), &[], &[], EnvelopeLimits::default()).unwrap(), McpAction::Initialize { .. }));
        let mut config = server();
        config.capabilities.tools_list_changed = false;
        assert!(validate_server(&config).is_ok());
    }

    #[test]
    fn closed_field_count_ignores_nested_colons_and_string_escapes() {
        let request = message("tools/call", Some(
            r#"{"na\u006de":"svc.public","argum\u0065nts":{"k:v":"\\\"[]{}:","nested":[{"a:b":1}]},"_meta":{"x:":"y:"}}"#,
        ));
        assert!(matches!(dispatch(&mut session(SessionPhase::Ready), &request).unwrap(), McpAction::CallTool { .. }));
        rejected(&mut session(SessionPhase::Ready), &message("tools/call", Some(r#"{"name":"svc.public","extra:field":{"arguments":{}}}"#)), INVALID_PARAMS);
    }

    #[test]
    fn malformed_manual_params_are_rejected_without_leaking_input() {
        for raw in ["{", "{} true", r#"{"_meta":{"secret":"\q"}}"#, r#"{"_meta":{"secret":"\uD800"}}"#] {
            let error = rejected(&mut Session::default(), &message("ping", Some(raw)), INVALID_PARAMS);
            assert!(!error.message.contains("secret"));
        }
        let mut request = message("ping", None);
        request.params = Some(Payload::from_slice(b"{\"x\":\"\xff\"}"));
        rejected(&mut Session::default(), &request, INVALID_PARAMS);
    }

    #[test]
    fn resource_limits_are_inclusive_and_fail_without_state_changes() {
        let request = message("ping", Some(r#"{"_meta":{"x":"é"}}"#));
        let length = request.params.as_ref().unwrap().len() as u32;
        let exact = EnvelopeLimits { max_payload_bytes: length, max_decoded_bytes: 8, max_depth: 2, max_fields: 1 };
        assert!(matches!(route(&mut Session::default(), &request, &server(), &[], &[], exact).unwrap(), McpAction::Ping { .. }));
        for limits in [
            EnvelopeLimits { max_payload_bytes: length - 1, ..exact },
            EnvelopeLimits { max_decoded_bytes: 7, ..exact },
            EnvelopeLimits { max_depth: 1, ..exact },
        ] {
            let mut state = Session::default();
            assert_eq!(route(&mut state, &request, &server(), &[], &[], limits).unwrap_err().code, INTERNAL_ERROR);
            assert_eq!(state, Session::default());
        }
        let request = message("ping", Some(r#"{"_meta":{"x":1,"y":2}}"#));
        let limits = EnvelopeLimits { max_fields: 1, ..EnvelopeLimits::default() };
        assert_eq!(route(&mut Session::default(), &request, &server(), &[], &[], limits).unwrap_err().code, INTERNAL_ERROR);
        for limits in [
            EnvelopeLimits { max_payload_bytes: 0, ..exact },
            EnvelopeLimits { max_decoded_bytes: 0, ..exact },
            EnvelopeLimits { max_depth: 0, ..exact },
            EnvelopeLimits { max_fields: 0, ..exact },
        ] {
            assert_eq!(route(&mut Session::default(), &message("ping", None), &server(), &[], &[], limits).unwrap_err().code, INTERNAL_ERROR);
        }
    }

    struct Validator<F> {
        calls: Cell<usize>,
        run: F,
    }

    impl<F> ToolArgumentsValidator for Validator<F>
    where F: Fn(&EndpointSchemaSpec, Payload) -> Result<ValidatedSlots> {
        fn validate_arguments(&self, endpoint: &EndpointSchemaSpec, arguments: Payload) -> Result<ValidatedSlots> {
            self.calls.set(self.calls.get() + 1);
            (self.run)(endpoint, arguments)
        }
    }

    fn validator<F>(run: F) -> Validator<F>
    where F: Fn(&EndpointSchemaSpec, Payload) -> Result<ValidatedSlots> {
        Validator { calls: Cell::new(0), run }
    }

    fn object_slots(payload: Payload) -> Result<ValidatedSlots> {
        // This test schema accepts any JSON object and materializes no slots.
        require_object(&payload, EnvelopeLimits::default())?;
        let length = u32::try_from(payload.len())
            .map_err(|_| local_error(ErrorCode::PayloadTooLarge, "test payload too large"))?;
        // SAFETY: The whole immutable payload has been validated against this
        // test's any-object schema (fingerprint 0x28). There are no references,
        // no decoded bytes, and the checked length exactly matches the buffer.
        let proof = unsafe { ValidationProof::issue_unchecked(0x28, length, 0) };
        Ok(ValidatedSlots { payload, decoded: Box::default(), slots: Vec::new(), proof })
    }

    fn call(code: u32, arguments: &str) -> McpAction {
        McpAction::CallTool {
            id: McpId::String(String::from("call-id")), endpoint_code: code,
            arguments: Payload::from_slice(arguments.as_bytes()),
        }
    }

    #[test]
    fn argument_validation_runs_exactly_once_and_preserves_the_canonical_proof() {
        let action = call(0, r#"{ "x": "\u0061" }"#);
        let check = validator(|endpoint, payload| {
            assert_eq!(endpoint.code, 0);
            assert_eq!(endpoint.service, "svc");
            assert_eq!(payload.as_slice(), br#"{ "x": "\u0061" }"#);
            object_slots(payload)
        });
        let result = validate_call(&action, &catalog(), &[], &check).unwrap();
        assert_eq!(check.calls.get(), 1);
        drop(action);
        assert_eq!(result.payload.as_slice(), br#"{ "x": "\u0061" }"#);
        assert_eq!(result.proof.schema_fingerprint(), 0x28);
        assert_eq!(result.proof.payload_len() as usize, result.payload.len());
        assert_eq!(result.proof.decoded_len(), 0);
    }

    #[test]
    fn validation_accepts_an_exact_owned_copy_and_a_nonempty_decoded_arena() {
        use jmax_abi::schema::SlotValue;
        use jmax_abi::span::{Span32, SpanSource, StrRef};

        let check = validator(|_, payload| {
            let literal = br#"{"value":"\u00e9"}"#;
            if payload.as_slice() != literal {
                return Err(local_error(ErrorCode::ConstraintViolation, "unexpected test literal"));
            }
            let decoded = "é".as_bytes().to_vec().into_boxed_slice();
            let slots = vec![SlotValue::Str(StrRef {
                span: Span32::new(0, 2), source: SpanSource::DecodedArena, scalar_len: 1,
            })];
            // SAFETY: Exact byte comparison validates this test's literal-object
            // schema, fingerprint 0x29. Its sole value decodes to two UTF-8 bytes
            // and one scalar. The slot resolves within that exact arena.
            let proof = unsafe {
                ValidationProof::issue_unchecked(0x29, literal.len() as u32, 2)
            };
            Ok(ValidatedSlots { payload: Payload::from_slice(literal), decoded, slots, proof })
        });
        let result = validate_call(&call(0, r#"{"value":"\u00e9"}"#), &catalog(), &[], &check).unwrap();
        assert_eq!(check.calls.get(), 1);
        assert_eq!(result.decoded.as_ref(), "é".as_bytes());
        assert_eq!(result.proof.decoded_len(), 2);
        assert_eq!(result.proof.schema_fingerprint(), 0x29);
        assert_eq!(result.slots.len(), 1);
    }

    #[test]
    fn initialization_resource_failure_and_initialized_resource_failure_do_not_commit() {
        let mut state = Session::default();
        let request = init("preferred");
        let limits = EnvelopeLimits { max_depth: 1, ..EnvelopeLimits::default() };
        assert_eq!(route(&mut state, &request, &server(), &catalog(), &[], limits).unwrap_err().code, INTERNAL_ERROR);
        assert_eq!(state, Session::default());
        dispatch(&mut state, &request).unwrap();
        let before = state.clone();
        let ready = notification("notifications/initialized", Some(r#"{"_meta":{}}"#));
        assert_eq!(route(&mut state, &ready, &server(), &catalog(), &[], limits).unwrap_err().code, INTERNAL_ERROR);
        assert_eq!(state, before);
        dispatch(&mut state, &ready).unwrap();
        assert_eq!(state.phase(), SessionPhase::Ready);
    }

    #[test]
    fn reauthorization_happens_by_code_before_validator_invocation() {
        let check = validator(|_, payload| object_slots(payload));
        let private = call(u32::MAX, "{}");
        let forbidden = validate_call(&private, &catalog(), &[], &check).unwrap_err();
        let missing = validate_call(&call(99, "{}"), &catalog(), &[], &check).unwrap_err();
        assert_eq!(forbidden, missing);
        assert_eq!(forbidden.code, INVALID_PARAMS);
        assert_eq!(check.calls.get(), 0);
        validate_call(&private, &catalog(), &[String::from("reader")], &check).unwrap();
        assert_eq!(check.calls.get(), 1);
        // Revocation between routing and validation must be honored.
        let routed = route(
            &mut session(SessionPhase::Ready), &message("tools/call", Some(r#"{"name":"svc.private"}"#)),
            &server(), &catalog(), &[String::from("admin")], EnvelopeLimits::default(),
        ).unwrap();
        assert_eq!(validate_call(&routed, &catalog(), &[], &check).unwrap_err().code, INVALID_PARAMS);
        assert_eq!(check.calls.get(), 1);
        // Rename the endpoint while keeping its code; validation resolves code.
        let mut renamed = catalog();
        renamed[0].endpoints[0].name = String::from("renamed");
        validate_call(&call(0, "{}"), &renamed, &[], &check).unwrap();
        assert_eq!(check.calls.get(), 2);
    }

    #[test]
    fn validation_rejects_other_action_kinds_without_invocation() {
        let check = validator(|_, payload| object_slots(payload));
        for action in [
            McpAction::Initialize { id: McpId::Number(1), protocol_version: String::from("v") },
            McpAction::Ping { id: McpId::Null },
            McpAction::ListTools { id: McpId::String(String::from("list")) },
            McpAction::Initialized, McpAction::IgnoreNotification,
        ] {
            let error = validate_call(&action, &catalog(), &[], &check).unwrap_err();
            assert_eq!(error.code, INVALID_REQUEST);
            assert_eq!(error.id, action_id(&action).cloned().unwrap_or(McpId::Null));
        }
        assert_eq!(check.calls.get(), 0);
    }

    #[test]
    fn validation_rechecks_the_entire_catalog_and_role_configuration() {
        let check = validator(|_, payload| object_slots(payload));
        for services in invalid_catalogs() {
            assert_eq!(validate_call(&call(0, "{}"), &services, &[], &check).unwrap_err().code, INTERNAL_ERROR);
        }
        for roles in [vec![String::new()], vec![String::from("x"), String::from("x")]] {
            assert_eq!(validate_call(&call(0, "{}"), &catalog(), &roles, &check).unwrap_err().code, INTERNAL_ERROR);
        }
        assert_eq!(check.calls.get(), 0);
    }

    #[test]
    fn validator_errors_map_to_invalid_params_without_disclosing_details() {
        for code in [ErrorCode::TypeMismatch, ErrorCode::ConstraintViolation, ErrorCode::SchemaImageInvalid, ErrorCode::BudgetExhausted] {
            let check = validator(|_, _| Err(Error::new(code, ErrorSource::SchemaVm, 12, "secret argument")));
            let error = validate_call(&call(0, "{}"), &catalog(), &[], &check).unwrap_err();
            assert_eq!(error.code, INVALID_PARAMS);
            assert_eq!(error.id, McpId::String(String::from("call-id")));
            assert!(!error.message.contains("secret"));
            assert_eq!(check.calls.get(), 1);
        }
    }

    #[test]
    fn different_argument_bytes_fail_even_when_lengths_and_proof_match() {
        let check = validator(|_, _| object_slots(Payload::from_slice(br#"{"b":1}"#)));
        let error = validate_call(&call(0, r#"{"a":1}"#), &catalog(), &[], &check).unwrap_err();
        assert_eq!(error.code, INTERNAL_ERROR);
        assert_eq!(check.calls.get(), 1);
    }

    #[test]
    fn payload_and_decoded_proof_lengths_are_checked_independently() {
        let payload_check = validator(|_, payload| {
            let mut slots = object_slots(Payload::from_slice(b"{}"))?;
            // The proof was issued correctly above. Public fields can later be
            // changed by a faulty provider; this boundary must reject that.
            slots.payload = payload;
            Ok(slots)
        });
        assert_eq!(validate_call(&call(0, " {} "), &catalog(), &[], &payload_check).unwrap_err().code, INTERNAL_ERROR);
        assert_eq!(payload_check.calls.get(), 1);
        let decoded_check = validator(|_, payload| {
            let mut slots = object_slots(payload)?;
            slots.decoded = vec![b'x'].into_boxed_slice();
            Ok(slots)
        });
        assert_eq!(validate_call(&call(0, "{}"), &catalog(), &[], &decoded_check).unwrap_err().code, INTERNAL_ERROR);
        assert_eq!(decoded_check.calls.get(), 1);
    }
}
