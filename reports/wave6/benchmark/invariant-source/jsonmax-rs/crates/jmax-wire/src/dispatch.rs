//! Synchronous, typed dispatch over validated endpoint envelopes.
//!
//! Routing checks the complete metadata set before returning an authorized
//! route. Parameter validators are the schema trust boundary: they must check
//! the entire object, including unknown and duplicate keys, before publishing
//! slots. This module checks that their returned owners and proof lengths agree.

use super::*;

/// Resolves an endpoint only after metadata validation and exact authorization.
///
/// Service IDs and names, endpoint codes and `(service, name)` identities, and
/// both required and granted role lists must be unambiguous. Roles are compared
/// exactly, without normalization, wildcards, inheritance, or case folding.
/// Empty required-role lists are public. This allocation-free implementation
/// uses pairwise comparisons over immutable metadata, with quadratic worst-case
/// work in the number of endpoints, services, and entries of each role list.
pub fn resolve_route<'a>(
    services: &'a [ServiceSpec],
    code: u32,
    roles: &[String],
) -> Result<Route<'a>> {
    resolve_route_with_role_ids(services, code, roles, &[])
}

/// Resolves a route with both named roles and numeric endpoint-libs role IDs.
/// Numeric requirements use canonical decimal strings in the shared ABI.
pub fn resolve_route_with_role_ids<'a>(
    services: &'a [ServiceSpec],
    code: u32,
    roles: &[String],
    role_ids: &[u32],
) -> Result<Route<'a>> {
    for (index, service) in services.iter().enumerate() {
        for previous in &services[..index] {
            if previous.id == service.id || previous.name == service.name {
                return Err(wire_error(ErrorCode::DuplicateObjectKey,
                    "duplicate service ID or name"));
            }
        }
    }

    for (service_index, service) in services.iter().enumerate() {
        for (endpoint_index, endpoint) in service.endpoints.iter().enumerate() {
            if endpoint.service != service.name {
                return Err(wire_error(ErrorCode::ConstraintViolation,
                    "endpoint service disagrees with containing service"));
            }
            validate_roles(&endpoint.roles)?;
            for (previous_index, previous_service) in services[..=service_index].iter().enumerate() {
                let previous_endpoints = if previous_index == service_index {
                    &previous_service.endpoints[..endpoint_index]
                } else {
                    previous_service.endpoints.as_slice()
                };
                for previous in previous_endpoints {
                    if previous.code == endpoint.code {
                        return Err(wire_error(ErrorCode::DuplicateObjectKey,
                            "duplicate endpoint code"));
                    }
                    if previous.service == endpoint.service && previous.name == endpoint.name {
                        return Err(wire_error(ErrorCode::DuplicateObjectKey,
                            "duplicate endpoint service and name"));
                    }
                }
            }
        }
    }
    validate_roles(roles)?;
    for (index, role_id) in role_ids.iter().enumerate() {
        if role_ids[..index].contains(role_id) {
            return Err(wire_error(ErrorCode::DuplicateObjectKey, "duplicate numeric role ID"));
        }
    }

    for service in services {
        if let Some(endpoint) = service.endpoints.iter().find(|endpoint| endpoint.code == code) {
            if !role_allowed_with_ids(endpoint, roles, role_ids) {
                return Err(wire_error(ErrorCode::AuthFailed,
                    "request roles do not authorize endpoint"));
            }
            return Ok(Route { service, endpoint });
        }
    }
    Err(wire_error(ErrorCode::ConstraintViolation, "unknown endpoint code"))
}

fn validate_roles(roles: &[String]) -> Result<()> {
    for (index, role) in roles.iter().enumerate() {
        if role.is_empty() {
            return Err(wire_error(ErrorCode::ConstraintViolation, "role must be nonempty"));
        }
        if roles[..index].contains(role) {
            return Err(wire_error(ErrorCode::DuplicateObjectKey, "duplicate role"));
        }
    }
    Ok(())
}

/// Validates parameters using their exact wire spelling and endpoint schema.
///
/// Callers must obtain authorization through [`resolve_route`] before invoking
/// this function. A present method must equal the decoded service name, a dot,
/// and the decoded endpoint name. The validator receives an independent owned
/// copy of the complete parameter object, with spans relative to that copy.
/// Byte-identical copies of that owner are permitted by the ABI proof contract.
/// Validator errors propagate unchanged; inconsistent returned storage is an
/// `InternalError`. Schema evidence is never issued or repaired here.
pub fn validate_params(
    request: &OwnedEnvelope,
    route: Route<'_>,
    validator: &dyn ParameterValidator,
) -> Result<ValidatedSlots> {
    let (code, raw) = request_params(request)?;
    if route.endpoint.code != code {
        return Err(wire_error(ErrorCode::ConstraintViolation,
            "request code disagrees with route"));
    }
    if route.endpoint.service != route.service.name {
        return Err(wire_error(ErrorCode::ConstraintViolation,
            "route endpoint disagrees with service"));
    }
    if request.view().method.is_some() {
        let method = request.method().ok_or_else(|| wire_error(ErrorCode::InternalError,
            "request method does not resolve within its owner"))?;
        let endpoint_name = method.strip_prefix(route.service.name.as_str())
            .and_then(|suffix| suffix.strip_prefix('.'));
        if endpoint_name != Some(route.endpoint.name.as_str()) {
            return Err(wire_error(ErrorCode::ConstraintViolation,
                "request method disagrees with route"));
        }
    }

    // Reserve before copying so failure cannot publish partial parameter data.
    // Payload::from_vec transfers this allocation without another byte copy.
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(raw.len()).map_err(|_| wire_error(ErrorCode::BudgetExhausted,
        "parameter payload allocation failed"))?;
    bytes.extend_from_slice(raw);
    let slots = validator.validate(route.endpoint, Payload::from_vec(bytes))?;
    if slots.payload.as_slice() != raw
        || u32::try_from(slots.payload.len()).ok() != Some(slots.proof.payload_len())
        || u32::try_from(slots.decoded.len()).ok() != Some(slots.proof.decoded_len())
    {
        return Err(wire_error(ErrorCode::InternalError,
            "validator returned inconsistent parameter owners or proof lengths"));
    }
    Ok(slots)
}

// OwnedEnvelope's private storage comes from the complete envelope parser.
// These checks guard the dispatch boundary without reparsing the JSON or
// imposing different syntax limits from those used to create the owner.
fn request_params(request: &OwnedEnvelope) -> Result<(u32, &[u8])> {
    let view = request.view();
    if view.kind != EnvelopeKind::EndpointRequest {
        return Err(wire_error(ErrorCode::TypeMismatch, "dispatch requires a request envelope"));
    }
    if view.seq.is_none() {
        return Err(wire_error(ErrorCode::MissingRequiredField, "request sequence is absent"));
    }
    let code = view.code.ok_or_else(|| wire_error(ErrorCode::MissingRequiredField,
        "request code is absent"))?;
    let span = view.params_span.ok_or_else(|| wire_error(ErrorCode::MissingRequiredField,
        "request parameters are absent"))?;
    if view.result_span.is_some() || view.error_span.is_some() {
        return Err(wire_error(ErrorCode::UnknownObjectKey,
            "response fields are forbidden in a request"));
    }
    let raw = request.resolve(span).ok_or_else(|| wire_error(ErrorCode::InternalError,
        "request parameters do not resolve within their owner"))?;
    if raw.first() != Some(&b'{') || raw.last() != Some(&b'}') {
        return Err(wire_error(ErrorCode::TypeMismatch, "request parameters must be an object"));
    }
    Ok((code, raw))
}

/// Decodes typed parameters only when their validated schema fingerprint agrees.
///
/// A mismatch is rejected before calling user decoding code. Decoder errors
/// propagate with their original source, offset, schema, and field metadata.
pub fn decode_params<T: JsonDecode + SchemaProvider>(slots: &ValidatedSlots) -> Result<T> {
    if T::schema_fingerprint() != slots.proof.schema_fingerprint() {
        return Err(wire_error(ErrorCode::SchemaImageInvalid,
            "parameter schema fingerprint disagrees with requested type"));
    }
    T::decode_slots(slots)
}

/// Authorizes, validates, and calls the handler exactly once on success.
///
/// All budget fields must be nonzero and retained bytes must cover the complete
/// request payload. The unchanged budget and roles reach the handler. CPU units
/// and timeout are host accounting metadata; this synchronous boundary cannot
/// preempt execution and performs no waiting, task spawning, or runtime I/O.
pub fn dispatch(
    request: &OwnedEnvelope,
    services: &[ServiceSpec],
    context: RequestContext<'_>,
    validator: &dyn ParameterValidator,
    handler: &dyn EndpointHandler,
) -> Result<Box<dyn JsonEncode + Send + Sync>> {
    if context.budget.cpu_units == 0 || context.budget.retained_bytes == 0
        || context.budget.timeout_ms == 0
        || u32::try_from(request.payload().len()).map_or(true,
            |length| length > context.budget.retained_bytes)
    {
        return Err(wire_error(ErrorCode::BudgetExhausted, "request budget is exhausted"));
    }
    let (code, _) = request_params(request)?;
    let route = resolve_route_with_role_ids(services, code, context.roles, context.role_ids)?;
    let params = validate_params(request, route, validator)?;
    handler.handle(route, params, context)
}
