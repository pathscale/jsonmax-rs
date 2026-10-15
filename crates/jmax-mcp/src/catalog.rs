//! Validated, role-filtered tool discovery with bounded owned schema output.
//!
//! Schema generation and local reference resolution belong to the provider.
//! This boundary checks the complete catalog before authorization, then checks
//! provider JSON without interpreting or rewriting its schema vocabulary.

use super::*;

/// List visible tools in catalog order within an aggregate UTF-8 byte budget.
///
/// The budget counts tool names, descriptions, and complete raw schema payloads,
/// including schema whitespace. It does not count JSON serialization overhead.
/// Each visible endpoint's provider is called once on success; hidden endpoints
/// never reach the provider. Errors discard all intermediate output. Names and
/// descriptions are copied only after every schema fits the aggregate budget.
pub fn list_tools(
    services: &[ServiceSpec],
    roles: &[String],
    schemas: &dyn ToolSchemaProvider,
    max_bytes: u32,
) -> Result<Vec<ToolDefinition>> {
    validate_catalog(services, roles)?;

    let mut used = 0;
    let mut visible = 0usize;
    for service in services {
        for endpoint in &service.endpoints {
            if role_allowed(endpoint, roles) {
                used = charge(used, service.name.len(), max_bytes)?;
                used = charge(used, 1, max_bytes)?;
                used = charge(used, endpoint.name.len(), max_bytes)?;
                used = charge(used, endpoint.description.len(), max_bytes)?;
                visible = visible.checked_add(1).ok_or_else(|| {
                    local_error(ErrorCode::BudgetExhausted, "tool count exceeds capacity")
                })?;
            }
        }
    }

    // Retain provider-owned payloads, with borrowed catalog metadata, until the
    // full aggregate has passed. No endpoint or schema payload is deep-cloned.
    let mut pending = Vec::new();
    pending.try_reserve_exact(visible).map_err(|_| {
        local_error(ErrorCode::BudgetExhausted, "schema list allocation failed")
    })?;
    for service in services {
        for endpoint in &service.endpoints {
            if !role_allowed(endpoint, roles) {
                continue;
            }
            let schema = schemas.input_schema(endpoint, max_bytes - used)?;
            // A provider is not trusted to honor its supplied byte limit.
            used = charge(used, schema.len(), max_bytes)?;
            validate_schema_object(&schema)?;
            pending.push((endpoint, schema));
        }
    }

    let mut tools = Vec::new();
    tools.try_reserve_exact(visible).map_err(|_| {
        local_error(ErrorCode::BudgetExhausted, "tool list allocation failed")
    })?;
    for (endpoint, input_schema) in pending {
        tools.push(ToolDefinition {
            name: tool_name(&endpoint.service, &endpoint.name)?,
            description: copy_string(&endpoint.description)?,
            endpoint_code: endpoint.code,
            input_schema,
        });
    }
    Ok(tools)
}

/// Borrow the exact catalog endpoint for an authorized, case-sensitive name.
///
/// All catalog identities and role entries are validated even when the requested
/// name is absent or the matching endpoint is public. The protocol boundary may
/// combine unknown and forbidden errors into one nondisclosing response.
pub fn find_tool<'a>(
    services: &'a [ServiceSpec],
    roles: &[String],
    name: &str,
) -> Result<&'a EndpointSchemaSpec> {
    validate_catalog(services, roles)?;
    if let Some((service_name, endpoint_name)) = name.split_once('.') {
        for service in services {
            if service.name != service_name {
                continue;
            }
            for endpoint in &service.endpoints {
                if endpoint.name == endpoint_name {
                    return if role_allowed(endpoint, roles) {
                        Ok(endpoint)
                    } else {
                        Err(local_error(ErrorCode::AuthFailed, "tool access denied"))
                    };
                }
            }
        }
    }
    Err(local_error(ErrorCode::ConstraintViolation, "unknown tool name"))
}

fn local_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Wire, 0, detail)
}

fn charge(used: u32, bytes: usize, limit: u32) -> Result<u32> {
    u32::try_from(bytes).ok()
        .and_then(|bytes| used.checked_add(bytes))
        .filter(|total| *total <= limit)
        .ok_or_else(|| local_error(ErrorCode::PayloadTooLarge, "tool catalog exceeds byte limit"))
}

fn copy_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| {
        local_error(ErrorCode::BudgetExhausted, "tool description allocation failed")
    })?;
    owned.push_str(value);
    Ok(owned)
}

fn valid_identifier(name: &str) -> bool {
    // Match the frozen root tool_name convention without allocating a name
    // during global validation or before aggregate byte accounting succeeds.
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
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
    // Pairwise borrowed comparisons keep validation allocation-free and avoid
    // introducing a second owned index for untrusted catalog metadata.
    for (service_index, service) in services.iter().enumerate() {
        if !valid_identifier(&service.name) {
            return Err(local_error(ErrorCode::ConstraintViolation, "invalid tool identifier"));
        }
        if services[..service_index].iter().any(|previous| {
            previous.id == service.id || previous.name == service.name
        }) {
            return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate service identity"));
        }
        for (endpoint_index, endpoint) in service.endpoints.iter().enumerate() {
            if endpoint.service != service.name {
                return Err(local_error(ErrorCode::ConstraintViolation, "endpoint service mismatch"));
            }
            if !valid_identifier(&endpoint.name) {
                return Err(local_error(ErrorCode::ConstraintViolation, "invalid tool identifier"));
            }
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
    // Dot-free unique service names and per-service endpoint names also prove
    // global uniqueness of their service.endpoint tool-name projections.
    Ok(())
}

fn validate_schema_object(schema: &Payload) -> Result<()> {
    let length = u32::try_from(schema.len()).map_err(|_| {
        local_error(ErrorCode::PayloadTooLarge, "tool schema exceeds byte limit")
    })?;
    // The aggregate budget has already bounded raw bytes. Decoded strings and
    // object field counts cannot exceed raw JSON length, so these limits impose
    // no additional size restriction. Depth uses the ABI's full representable
    // range and the envelope parser's explicit, fallibly allocated stack.
    let limits = EnvelopeLimits {
        max_payload_bytes: length.max(1),
        max_decoded_bytes: length.max(1),
        max_depth: u16::MAX,
        max_fields: length.max(1),
    };
    envelope::validate_json(schema, limits)?;
    let start = schema.as_slice().iter().position(|byte| {
        !matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
    }).unwrap_or(schema.len());
    if schema.as_slice().get(start) != Some(&b'{') {
        return Err(Error::new(
            ErrorCode::TypeMismatch, ErrorSource::Wire, start as u32,
            "expected tool schema object",
        ));
    }
    Ok(())
}
