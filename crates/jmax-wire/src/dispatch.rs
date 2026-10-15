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

    for service in services {
        if let Some(endpoint) = service.endpoints.iter().find(|endpoint| endpoint.code == code) {
            if !role_allowed(endpoint, roles) {
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
    let route = resolve_route(services, code, context.roles)?;
    let params = validate_params(request, route, validator)?;
    handler.handle(route, params, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use core::cell::{Cell, RefCell};
    use jmax_abi::endpoint::{EndpointField, EndpointType};
    use jmax_abi::schema::{FieldId, SchemaFieldIr, SchemaId, SchemaIr, SlotValue};
    use jmax_abi::syntax::{Event, ScalarKind, ScanStep};
    use jmax_cursor::{walk, Cursor, SkipMode};

    const VALUE_FINGERPRINT: u64 = 0x5733_3237_5354_5201;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| String::from(*value)).collect()
    }

    fn endpoint(service: &str, name: &str, code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, name: String::from(name), service: String::from(service), roles: strings(roles),
            params: vec![EndpointField {
                name: String::from("value"), ty: EndpointType::String, description: String::new(),
            }],
            response: Vec::new(), stream_response: None, description: String::new(),
        }
    }

    fn services() -> Vec<ServiceSpec> {
        vec![
            ServiceSpec {
                id: 0, name: String::from("account"),
                endpoints: vec![endpoint("account", "echo", 0, &[]),
                    endpoint("account", "change", 7, &["writer", "admin"])],
            },
            ServiceSpec {
                id: u16::MAX, name: String::from("audit"),
                endpoints: vec![endpoint("audit", "echo", u32::MAX, &["reader"])],
            },
        ]
    }

    fn request(code: u32, params: &str, method: Option<&str>) -> OwnedEnvelope {
        let method = method.map(|value| format!(",\"method\":{value}")).unwrap_or_default();
        let json = format!("{{\"kind\":\"request\",\"seq\":18446744073709551615,\"code\":{code},\"params\": \n{params} \t{method}}}");
        envelope::decode_request(Payload::from_vec(json.into_bytes()), EnvelopeLimits::default()).unwrap()
    }

    fn context<'a>(request: &OwnedEnvelope, roles: &'a [String]) -> RequestContext<'a> {
        RequestContext {
            roles,
            budget: ResourceBudget {
                cpu_units: 1, retained_bytes: u32::try_from(request.payload().len()).unwrap(),
                timeout_ms: 1,
            },
        }
    }

    fn local_failure<T>(result: Result<T>, code: ErrorCode) -> Error {
        let error = match result {
            Ok(_) => panic!("expected a dispatch error"),
            Err(error) => error,
        };
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Wire);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
        error
    }

    fn fixture_error(code: ErrorCode) -> Error {
        Error::new(code, ErrorSource::SchemaVm, 5, "fixture validation error").with_schema(27, 3)
    }

    /// A complete one-field schema validator used only by the tests. Traversal
    /// through Finished establishes syntax, UTF-8, duplicate policy and owners;
    /// the state machine checks the required string and rejects unknown keys.
    #[derive(Default)]
    struct ValueValidator {
        calls: Cell<usize>,
        observed: RefCell<Vec<u8>>,
        input_pointer: Cell<usize>,
        observed_code: Cell<u32>,
    }

    fn next_event(cursor: &mut Cursor) -> Result<Option<Event>> {
        match walk::next(cursor)? {
            ScanStep::Emit(event) => Ok(Some(event)),
            ScanStep::Finished => Ok(None),
            ScanStep::NeedMore => Err(fixture_error(ErrorCode::TransportIncomplete)),
        }
    }

    impl ParameterValidator for ValueValidator {
        fn validate(&self, endpoint: &EndpointSchemaSpec, payload: Payload) -> Result<ValidatedSlots> {
            self.calls.set(self.calls.get() + 1);
            self.observed.replace(payload.as_slice().to_vec());
            self.input_pointer.set(payload.as_slice().as_ptr() as usize);
            self.observed_code.set(endpoint.code);
            assert_eq!(endpoint.params.len(), 1);
            assert_eq!(endpoint.params[0].name, "value");
            assert_eq!(endpoint.params[0].ty, EndpointType::String);
            let mut cursor = walk::from_payload(payload.clone(), Limits::strict_default(), SkipMode::Strict)?;
            if !matches!(next_event(&mut cursor)?, Some(Event::ObjectStart { .. })) {
                return Err(fixture_error(ErrorCode::TypeMismatch));
            }
            let mut value = None;
            loop {
                match next_event(&mut cursor)? {
                    Some(Event::Key { str_ref }) => {
                        if cursor.resolve_str(str_ref) != Some("value") {
                            return Err(fixture_error(ErrorCode::UnknownObjectKey));
                        }
                        if value.is_some() {
                            return Err(fixture_error(ErrorCode::DuplicateObjectKey));
                        }
                        match next_event(&mut cursor)? {
                            Some(Event::Scalar { kind: ScalarKind::String,
                                decoded: Some(reference), .. }) => value = Some(reference),
                            _ => return Err(fixture_error(ErrorCode::TypeMismatch)),
                        }
                    }
                    Some(Event::ObjectEnd { .. }) => break,
                    _ => return Err(fixture_error(ErrorCode::TypeMismatch)),
                }
            }
            if next_event(&mut cursor)?.is_some() {
                return Err(fixture_error(ErrorCode::TrailingContent));
            }
            let reference = value.ok_or_else(|| fixture_error(ErrorCode::MissingRequiredField))?;
            assert_eq!(cursor.raw_bytes(), payload.as_slice());
            let text = cursor.resolve_str(reference).unwrap();
            assert_eq!(text.chars().count(), reference.scalar_len as usize);
            let decoded = cursor.decoded_bytes().to_vec().into_boxed_slice();
            // SAFETY: Complete strict cursor traversal and the schema state
            // machine above validated this exact payload as one required string
            // field with no unknown or duplicate keys. The arena is an exact
            // copy of the cursor arena, the sole StrRef was checked against
            // those buffers, and both lengths use checked conversions.
            let proof = unsafe { ValidationProof::issue_unchecked(VALUE_FINGERPRINT,
                u32::try_from(payload.len()).unwrap(), u32::try_from(decoded.len()).unwrap()) };
            Ok(ValidatedSlots { payload, decoded, slots: vec![SlotValue::Str(reference)], proof })
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Value(String);

    impl SchemaProvider for Value {
        fn schema_ir() -> SchemaIr {
            SchemaIr {
                name: String::from("Value"), schema_id: SchemaId(27), required_count: 1,
                reject_unknown: true,
                fields: vec![SchemaFieldIr {
                    field_id: FieldId(0), name: String::from("value"), required: true,
                    scalar_kind: Some(ScalarKind::String), format: None, min_len: None,
                    max_len: None, min_i64: None, max_i64: None,
                }],
            }
        }

        fn schema_fingerprint() -> u64 { VALUE_FINGERPRINT }
    }

    impl JsonDecode for Value {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            let reference = match slots.slots.as_slice() {
                [SlotValue::Str(reference)] => *reference,
                _ => return Err(fixture_error(ErrorCode::TypeMismatch)),
            };
            let bytes = match reference.source {
                SpanSource::RawPayload => slots.payload.as_slice(),
                SpanSource::DecodedArena => &slots.decoded,
            };
            let text = reference.span.slice(bytes)
                .and_then(|bytes| core::str::from_utf8(bytes).ok())
                .ok_or_else(|| fixture_error(ErrorCode::InternalError))?;
            Ok(Self(String::from(text)))
        }
    }

    impl JsonEncode for Value {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            write_json_string(&self.0, sink)
        }
    }

    #[derive(Default)]
    struct Handler {
        calls: Cell<usize>,
        observed_budget: Cell<Option<ResourceBudget>>,
        observed_roles: RefCell<Vec<String>>,
        observed_route: RefCell<Option<(String, String, u32)>>,
        retained: RefCell<Option<ValidatedSlots>>,
        failure: Option<Error>,
    }

    impl EndpointHandler for Handler {
        fn handle(&self, route: Route<'_>, params: ValidatedSlots, context: RequestContext<'_>)
            -> Result<Box<dyn JsonEncode + Send + Sync>>
        {
            self.calls.set(self.calls.get() + 1);
            self.observed_budget.set(Some(context.budget));
            self.observed_roles.replace(context.roles.to_vec());
            self.observed_route.replace(Some((route.service.name.clone(),
                route.endpoint.name.clone(), route.endpoint.code)));
            let value = decode_params::<Value>(&params)?;
            self.retained.replace(Some(params));
            if let Some(error) = &self.failure { return Err(error.clone()); }
            Ok(Box::new(value))
        }
    }

    #[test]
    fn public_and_any_role_routes_borrow_the_original_metadata() {
        let services = services();
        for roles in [Vec::new(), strings(&["unrelated"])] {
            let route = resolve_route(&services, 0, &roles).unwrap();
            assert!(core::ptr::eq(route.service, &services[0]));
            assert!(core::ptr::eq(route.endpoint, &services[0].endpoints[0]));
        }
        for roles in [strings(&["writer"]), strings(&["admin"]),
            strings(&["unrelated", "writer"]), strings(&["writer", "admin"])]
        {
            assert_eq!(resolve_route(&services, 7, &roles).unwrap().endpoint.name, "change");
        }
        let route = resolve_route(&services, u32::MAX, &strings(&["reader"])).unwrap();
        assert_eq!(route.service.id, u16::MAX);
        assert_eq!(route.endpoint.code, u32::MAX);
        for roles in [Vec::new(), strings(&["Writer"]), strings(&["writer "]),
            strings(&["*"]), strings(&["reader"])]
        {
            local_failure(resolve_route(&services, 7, &roles), ErrorCode::AuthFailed);
        }
        local_failure(resolve_route(&services, 1, &[]), ErrorCode::ConstraintViolation);
        local_failure(resolve_route(&[], 0, &[]), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn distinct_service_identities_may_reuse_endpoint_names() {
        let services = services();
        assert_eq!(resolve_route(&services, 0, &[]).unwrap().endpoint.name, "echo");
        assert_eq!(resolve_route(&services, u32::MAX, &strings(&["reader"]))
            .unwrap().endpoint.name, "echo");
        let empty = [ServiceSpec { id: 9, name: String::from("empty"), endpoints: Vec::new() }];
        local_failure(resolve_route(&empty, 9, &[]), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn complete_registry_is_validated_before_lookup_or_authorization() {
        for duplicate_id in [true, false] {
            let mut services = services();
            if duplicate_id { services[1].id = services[0].id; }
            else { services[1].name = services[0].name.clone(); }
            for code in [0, 7, 123] {
                local_failure(resolve_route(&services, code, &[]), ErrorCode::DuplicateObjectKey);
            }
        }
        for cross_service in [false, true] {
            let mut services = services();
            if cross_service { services[1].endpoints[0].code = 0; }
            else { services[0].endpoints[1].code = 0; }
            local_failure(resolve_route(&services, 0, &[]), ErrorCode::DuplicateObjectKey);
            local_failure(resolve_route(&services, 123, &[]), ErrorCode::DuplicateObjectKey);
        }
        let mut duplicate_identity = services();
        duplicate_identity[0].endpoints[1].name = String::from("echo");
        local_failure(resolve_route(&duplicate_identity, 0, &[]), ErrorCode::DuplicateObjectKey);
        let mut invalid = services();
        invalid[1].endpoints[0].service = String::from("account");
        local_failure(resolve_route(&invalid, 0, &[]), ErrorCode::ConstraintViolation);
        local_failure(resolve_route(&invalid, 7, &[]), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn role_entries_are_nonempty_unique_and_exact() {
        for (required, code) in [(&[""][..], ErrorCode::ConstraintViolation),
            (&["reader", "reader"][..], ErrorCode::DuplicateObjectKey)]
        {
            let mut services = services();
            services[1].endpoints[0].roles = strings(required);
            local_failure(resolve_route(&services, 0, &[]), code);
            local_failure(resolve_route(&services, 123, &[]), code);
        }
        let valid = services();
        for code in [0, 7, 123] {
            local_failure(resolve_route(&valid, code, &strings(&[""])),
                ErrorCode::ConstraintViolation);
            local_failure(resolve_route(&valid, code, &strings(&["writer", "writer"])),
                ErrorCode::DuplicateObjectKey);
        }
        let mut unicode = services();
        unicode[0].endpoints[0].roles = strings(&["é"]);
        resolve_route(&unicode, 0, &strings(&["é"])).unwrap();
        local_failure(resolve_route(&unicode, 0, &strings(&["e\u{301}"])), ErrorCode::AuthFailed);
        unicode[0].endpoints[0].roles = strings(&["reader", "Reader"]);
        resolve_route(&unicode, 0, &strings(&["Reader"])).unwrap();
    }

    #[test]
    fn exact_parameter_bytes_are_copied_and_survive_all_input_owners() {
        let services = services();
        let validator = ValueValidator::default();
        let params = r#"{ "value" : "a\n\u00e9\uD83D\uDE00" }"#;
        for shared in [false, true] {
            let mut request = request(0, params, Some(r#""account.echo""#));
            if shared { request.payload = Payload::from_slice(request.payload().as_slice()); }
            let raw = request.resolve(request.view().params_span.unwrap()).unwrap();
            let request_pointer = raw.as_ptr() as usize;
            let route = resolve_route(&services, 0, &[]).unwrap();
            let slots = validate_params(&request, route, &validator).unwrap();
            assert_eq!(&*validator.observed.borrow(), params.as_bytes());
            assert_ne!(validator.input_pointer.get(), request_pointer);
            assert_eq!(slots.payload.as_slice().as_ptr() as usize, validator.input_pointer.get());
            assert_eq!(slots.payload.as_slice(), params.as_bytes());
            assert_eq!(slots.proof.payload_len() as usize, params.len());
            assert_eq!(slots.proof.decoded_len() as usize, slots.decoded.len());
            assert_eq!(validator.observed_code.get(), 0);
            drop(request);
            assert_eq!(decode_params::<Value>(&slots).unwrap(), Value(String::from("a\né😀")));
        }
        assert_eq!(validator.calls.get(), 2);
    }

    #[test]
    fn raw_and_decoded_string_slots_include_empty_and_unicode_boundaries() {
        let services = services();
        let validator = ValueValidator::default();
        for (raw, expected, source) in [
            (r#"{"value":""}"#, "", SpanSource::RawPayload),
            (r#"{"value":"plain"}"#, "plain", SpanSource::RawPayload),
            (r#"{"value":"é😀"}"#, "é😀", SpanSource::RawPayload),
            (r#"{"value":"\u0000\uDBFF\uDFFF"}"#, "\0\u{10ffff}", SpanSource::DecodedArena),
            (r#"{"v\u0061lue":"\u0061"}"#, "a", SpanSource::DecodedArena),
        ] {
            let request = request(0, raw, None);
            let slots = validate_params(&request, resolve_route(&services, 0, &[]).unwrap(),
                &validator).unwrap();
            assert_eq!(slots.payload.as_slice(), raw.as_bytes());
            assert_eq!(decode_params::<Value>(&slots).unwrap(), Value(String::from(expected)));
            match slots.slots[0] {
                SlotValue::Str(reference) => {
                    assert_eq!(reference.source, source);
                    assert_eq!(reference.scalar_len as usize, expected.chars().count());
                }
                _ => panic!("expected a string slot"),
            }
        }
    }

    #[test]
    fn methods_are_optional_exact_and_compared_after_unicode_decoding() {
        let mut services = services();
        let validator = ValueValidator::default();
        for method in [None, Some(r#""account.echo""#), Some(r#""acc\u006funt.ec\u0068o""#)] {
            let request = request(0, r#"{"value":"ok"}"#, method);
            validate_params(&request, resolve_route(&services, 0, &[]).unwrap(), &validator).unwrap();
        }
        let calls = validator.calls.get();
        for method in [r#""""#, r#""echo""#, r#""Account.echo""#, r#""account.Echo""#,
            r#""account.echo.extra""#, r#""account.echo ""#, r#""accountXecho""#,
            r#""account..echo""#, r#""account.echo\u0000""#]
        {
            let request = request(0, r#"{"value":"ok"}"#, Some(method));
            local_failure(validate_params(&request, resolve_route(&services, 0, &[]).unwrap(),
                &validator), ErrorCode::ConstraintViolation);
        }
        assert_eq!(validator.calls.get(), calls);
        services[0].name = String::from("é");
        services[0].endpoints[0].service = String::from("é");
        services[0].endpoints[1].service = String::from("é");
        services[0].endpoints[0].name = String::from("😀");
        let request = request(0, r#"{"value":"ok"}"#, Some(r#""\u00e9.\uD83D\uDE00""#));
        validate_params(&request, resolve_route(&services, 0, &[]).unwrap(), &validator).unwrap();
    }

    #[test]
    fn wrong_route_code_and_service_are_rejected_before_validation() {
        let services = services();
        let validator = ValueValidator::default();
        let request = request(0, r#"{"value":"ok"}"#, None);
        let wrong_code = resolve_route(&services, 7, &strings(&["writer"])).unwrap();
        local_failure(validate_params(&request, wrong_code, &validator), ErrorCode::ConstraintViolation);
        let wrong_service = Route { service: &services[1], endpoint: &services[0].endpoints[0] };
        local_failure(validate_params(&request, wrong_service, &validator), ErrorCode::ConstraintViolation);
        assert_eq!(validator.calls.get(), 0);
    }

    #[test]
    fn request_shape_and_owner_checks_reject_inconsistent_internal_metadata() {
        let services = services();
        let validator = ValueValidator::default();
        let route = resolve_route(&services, 0, &[]).unwrap();
        let valid = request(0, r#"{"value":"ok"}"#, Some(r#""account.echo""#));
        for kind in [EnvelopeKind::EndpointImmediate, EnvelopeKind::EndpointStream,
            EnvelopeKind::EndpointError, EnvelopeKind::EndpointLog, EnvelopeKind::EndpointForwarded,
            EnvelopeKind::EndpointClose, EnvelopeKind::McpRequest, EnvelopeKind::McpNotification,
            EnvelopeKind::McpResponse, EnvelopeKind::McpError]
        {
            let mut invalid = valid.clone();
            invalid.view.kind = kind;
            local_failure(validate_params(&invalid, route, &validator), ErrorCode::TypeMismatch);
        }
        for field in 0..3 {
            let mut invalid = valid.clone();
            match field {
                0 => invalid.view.seq = None,
                1 => invalid.view.code = None,
                _ => invalid.view.params_span = None,
            }
            local_failure(validate_params(&invalid, route, &validator), ErrorCode::MissingRequiredField);
        }
        for span in [Span32::new(u32::MAX, 1), Span32::new(0, u32::MAX),
            Span32::new(valid.payload().len() as u32 + 1, 0)]
        {
            let mut invalid = valid.clone();
            invalid.view.params_span = Some(span);
            local_failure(validate_params(&invalid, route, &validator), ErrorCode::InternalError);
        }
        for span in [Span32::new(0, 0), Span32::new(0, 1)] {
            let mut invalid = valid.clone();
            invalid.view.params_span = Some(span);
            local_failure(validate_params(&invalid, route, &validator), ErrorCode::TypeMismatch);
        }
        for error_field in [false, true] {
            let mut invalid = valid.clone();
            if error_field { invalid.view.error_span = Some(Span32::new(0, 1)); }
            else { invalid.view.result_span = Some(Span32::new(0, 1)); }
            local_failure(validate_params(&invalid, route, &validator), ErrorCode::UnknownObjectKey);
        }
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            let mut invalid = valid.clone();
            invalid.view.method = Some(StrRef {
                span: Span32::new(u32::MAX, 1), source, scalar_len: 1,
            });
            local_failure(validate_params(&invalid, route, &validator), ErrorCode::InternalError);
        }
        assert_eq!(validator.calls.get(), 0);
    }

    #[test]
    fn schema_unknown_missing_and_wrong_type_failures_preserve_validator_errors() {
        let services = services();
        let validator = ValueValidator::default();
        let handler = Handler::default();
        for (raw, code) in [
            (r#"{}"#, ErrorCode::MissingRequiredField),
            (r#"{"extra":"x"}"#, ErrorCode::UnknownObjectKey),
            (r#"{"value":"x","extra":true}"#, ErrorCode::UnknownObjectKey),
            (r#"{"value":false}"#, ErrorCode::TypeMismatch),
            (r#"{"value":null}"#, ErrorCode::TypeMismatch),
            (r#"{"value":[]}"#, ErrorCode::TypeMismatch),
            (r#"{"value":{}}"#, ErrorCode::TypeMismatch),
        ] {
            let request = request(0, raw, None);
            let error = match dispatch(&request, &services, context(&request, &[]), &validator, &handler) {
                Err(error) => error,
                Ok(_) => panic!("invalid schema reached handler"),
            };
            assert_eq!(error, fixture_error(code));
        }
        assert_eq!(validator.calls.get(), 7);
        assert_eq!(handler.calls.get(), 0);
    }

    #[test]
    fn duplicate_parameter_keys_and_escaped_aliases_never_reach_dispatch() {
        for params in [r#"{"value":"a","value":"b"}"#,
            r#"{"value":"a","v\u0061lue":"b"}"#]
        {
            let json = format!("{{\"kind\":\"request\",\"seq\":0,\"code\":0,\"params\":{params}}}");
            assert_eq!(envelope::decode_request(Payload::from_slice(json.as_bytes()),
                EnvelopeLimits::default()).unwrap_err().code, ErrorCode::DuplicateObjectKey);
            let validator = ValueValidator::default();
            assert_eq!(validator.validate(&services()[0].endpoints[0],
                Payload::from_slice(params.as_bytes())).unwrap_err().code, ErrorCode::DuplicateObjectKey);
        }
    }

    #[derive(Clone, Copy)]
    enum Mutation {
        Replace, Reformat, PayloadLength, PayloadLengthShort, DecodedLength,
        TruncateDecoded, CopyOwner, CopyShared,
    }

    struct MutatingValidator {
        inner: ValueValidator,
        mutation: Mutation,
    }

    impl ParameterValidator for MutatingValidator {
        fn validate(&self, endpoint: &EndpointSchemaSpec, payload: Payload) -> Result<ValidatedSlots> {
            let mut slots = self.inner.validate(endpoint, payload)?;
            // Start with genuinely validated evidence, then corrupt only safe,
            // public storage fields. No invalid proof is issued in these tests.
            match self.mutation {
                Mutation::Replace => slots.payload = Payload::from_slice(br#"{"value":"y"}"#),
                Mutation::Reformat => slots.payload = Payload::from_slice(br#"{ "value": "x" }"#),
                Mutation::PayloadLength | Mutation::PayloadLengthShort => {
                    let alternate: &[u8] = match self.mutation {
                        Mutation::PayloadLengthShort => br#"{"value":""}"#,
                        _ => br#"{"value":"longer"}"#,
                    };
                    // Keep the exact request payload while substituting a
                    // proof genuinely issued for a different-sized object.
                    // This isolates the proof-length guard from byte equality.
                    slots.proof = ValueValidator::default()
                        .validate(endpoint, Payload::from_slice(alternate))?.proof;
                }
                Mutation::DecodedLength => slots.decoded = vec![b'x'].into_boxed_slice(),
                Mutation::TruncateDecoded => slots.decoded = Vec::new().into_boxed_slice(),
                Mutation::CopyOwner => slots.payload = Payload::from_vec(slots.payload.as_slice().to_vec()),
                Mutation::CopyShared => slots.payload = Payload::from_slice(slots.payload.as_slice()),
            }
            Ok(slots)
        }
    }

    #[test]
    fn altered_owners_are_rejected_but_identical_owned_copies_are_accepted() {
        let services = services();
        let plain_request = request(0, r#"{"value":"x"}"#, None);
        let handler = Handler::default();
        for mutation in [Mutation::Replace, Mutation::Reformat, Mutation::PayloadLength,
            Mutation::PayloadLengthShort, Mutation::DecodedLength]
        {
            let validator = MutatingValidator { inner: ValueValidator::default(), mutation };
            local_failure(dispatch(&plain_request, &services, context(&plain_request, &[]), &validator, &handler),
                ErrorCode::InternalError);
            assert_eq!(validator.inner.calls.get(), 1);
            assert_eq!(handler.calls.get(), 0);
        }
        let escaped_request = request(0, r#"{"value":"\u0061"}"#, None);
        let validator = MutatingValidator {
            inner: ValueValidator::default(), mutation: Mutation::TruncateDecoded,
        };
        local_failure(dispatch(&escaped_request, &services, context(&escaped_request, &[]),
            &validator, &handler), ErrorCode::InternalError);
        assert_eq!(validator.inner.calls.get(), 1);
        assert_eq!(handler.calls.get(), 0);
        for mutation in [Mutation::CopyOwner, Mutation::CopyShared] {
            let validator = MutatingValidator { inner: ValueValidator::default(), mutation };
            let slots = validate_params(&plain_request, resolve_route(&services, 0, &[]).unwrap(),
                &validator).unwrap();
            assert_eq!(decode_params::<Value>(&slots).unwrap(), Value(String::from("x")));
        }
    }

    struct WrongSchema;

    impl SchemaProvider for WrongSchema {
        fn schema_ir() -> SchemaIr { Value::schema_ir() }
        fn schema_fingerprint() -> u64 { VALUE_FINGERPRINT ^ 1 }
    }

    impl JsonDecode for WrongSchema {
        fn decode_slots(_: &ValidatedSlots) -> Result<Self> {
            panic!("schema mismatch must be rejected before decoding")
        }
    }

    struct FailingDecode;

    impl SchemaProvider for FailingDecode {
        fn schema_ir() -> SchemaIr { Value::schema_ir() }
        fn schema_fingerprint() -> u64 { VALUE_FINGERPRINT }
    }

    impl JsonDecode for FailingDecode {
        fn decode_slots(_: &ValidatedSlots) -> Result<Self> {
            Err(Error::new(ErrorCode::Cancelled, ErrorSource::Decode, 9,
                "deterministic decode failure").with_schema(17, 23))
        }
    }

    #[test]
    fn schema_fingerprints_gate_decoding_and_decoder_errors_are_unchanged() {
        let services = services();
        let validator = ValueValidator::default();
        let request = request(0, r#"{"value":"typed"}"#, None);
        let slots = validate_params(&request, resolve_route(&services, 0, &[]).unwrap(), &validator).unwrap();
        local_failure(decode_params::<WrongSchema>(&slots), ErrorCode::SchemaImageInvalid);
        let error = match decode_params::<FailingDecode>(&slots) {
            Err(error) => error,
            Ok(_) => panic!("expected decoder failure"),
        };
        assert_eq!(error, Error::new(ErrorCode::Cancelled, ErrorSource::Decode, 9,
            "deterministic decode failure").with_schema(17, 23));
        assert_eq!(decode_params::<Value>(&slots).unwrap(), Value(String::from("typed")));
    }

    #[test]
    fn dispatch_calls_once_preserves_context_and_returns_an_owned_typed_encoder() {
        let services = services();
        let validator = ValueValidator::default();
        let handler = Handler::default();
        let request = request(7, r#"{"value":"a\n\"é"}"#, Some(r#""account.change""#));
        let roles = strings(&["unrelated", "admin"]);
        let context = context(&request, &roles);
        let expected_budget = context.budget;
        let output = dispatch(&request, &services, context, &validator, &handler).unwrap();
        assert_eq!(validator.calls.get(), 1);
        assert_eq!(handler.calls.get(), 1);
        assert_eq!(handler.observed_budget.get(), Some(expected_budget));
        assert_eq!(*handler.observed_roles.borrow(), roles);
        assert_eq!(*handler.observed_route.borrow(), Some((String::from("account"),
            String::from("change"), 7)));
        let retained = handler.retained.borrow_mut().take().unwrap();
        drop(request);
        drop(services);
        drop(roles);
        drop(validator);
        drop(handler);
        assert_eq!(decode_params::<Value>(&retained).unwrap(), Value(String::from("a\n\"é")));
        let mut sink = BufferSink::new(64);
        output.encode_json(&mut sink).unwrap();
        assert_eq!(sink.as_bytes(), "\"a\\n\\\"é\"".as_bytes());
    }

    #[test]
    fn dispatch_preserves_zero_and_full_width_request_identifiers() {
        let services = services();
        for (seq, code, roles) in [(0, 0, Vec::new()),
            (u64::MAX, u32::MAX, strings(&["reader"]))]
        {
            let json = format!("{{\"kind\":\"request\",\"seq\":{seq},\"code\":{code},\"params\":{{\"value\":\"bound\"}}}}");
            let request = envelope::decode_request(Payload::from_slice(json.as_bytes()),
                EnvelopeLimits::default()).unwrap();
            let original_view = request.view();
            let validator = ValueValidator::default();
            let handler = Handler::default();
            let output = dispatch(&request, &services, context(&request, &roles), &validator, &handler).unwrap();
            assert_eq!(request.view(), original_view);
            assert_eq!(request.view().seq, Some(seq));
            assert_eq!(request.view().code, Some(code));
            assert_eq!(validator.observed_code.get(), code);
            assert_eq!(validator.calls.get(), 1);
            assert_eq!(handler.calls.get(), 1);
            let mut sink = BufferSink::new(7);
            output.encode_json(&mut sink).unwrap();
            assert_eq!(sink.as_bytes(), br#""bound""#);
        }
    }

    #[test]
    fn every_budget_boundary_is_enforced_before_routing_validation_or_handling() {
        let services = services();
        let validator = ValueValidator::default();
        let handler = Handler::default();
        let request = request(0, r#"{"value":"ok"}"#, None);
        let valid = context(&request, &[]);
        for budget in [
            ResourceBudget { cpu_units: 0, ..valid.budget },
            ResourceBudget { retained_bytes: 0, ..valid.budget },
            ResourceBudget { timeout_ms: 0, ..valid.budget },
            ResourceBudget { retained_bytes: valid.budget.retained_bytes - 1, ..valid.budget },
        ] {
            // Empty services would fail route lookup if budget preflight did
            // not run first. Neither extension point may observe this request.
            local_failure(dispatch(&request, &[], RequestContext { budget, ..valid },
                &validator, &handler), ErrorCode::BudgetExhausted);
        }
        assert_eq!(validator.calls.get(), 0);
        assert_eq!(handler.calls.get(), 0);
        dispatch(&request, &services, valid, &validator, &handler).unwrap();
        let maximum = ResourceBudget { cpu_units: u32::MAX, retained_bytes: u32::MAX,
            timeout_ms: u32::MAX };
        dispatch(&request, &services, RequestContext { budget: maximum, ..valid },
            &validator, &handler).unwrap();
        assert_eq!(validator.calls.get(), 2);
        assert_eq!(handler.calls.get(), 2);
        assert_eq!(handler.observed_budget.get(), Some(maximum));
    }

    #[test]
    fn authorization_metadata_unknown_routes_and_methods_stop_before_extensions() {
        let validator = ValueValidator::default();
        let handler = Handler::default();
        for (code, method, error) in [(7, None, ErrorCode::AuthFailed),
            (42, None, ErrorCode::ConstraintViolation),
            (0, Some(r#""wrong.method""#), ErrorCode::ConstraintViolation)]
        {
            let request = request(code, r#"{"value":"ok"}"#, method);
            local_failure(dispatch(&request, &services(), context(&request, &[]), &validator, &handler), error);
        }
        let request = request(0, r#"{"value":"ok"}"#, None);
        let mut invalid = services();
        invalid[1].id = invalid[0].id;
        local_failure(dispatch(&request, &invalid, context(&request, &[]), &validator, &handler),
            ErrorCode::DuplicateObjectKey);
        let response = envelope::decode_response(Payload::from_slice(
            br#"{"kind":"immediate","seq":0,"code":0,"result":null}"#),
            EnvelopeLimits::default()).unwrap();
        local_failure(dispatch(&response, &services(), context(&response, &[]), &validator, &handler),
            ErrorCode::TypeMismatch);
        assert_eq!(validator.calls.get(), 0);
        assert_eq!(handler.calls.get(), 0);
    }

    struct RejectingValidator { error: Error, calls: Cell<usize> }

    impl ParameterValidator for RejectingValidator {
        fn validate(&self, _: &EndpointSchemaSpec, _: Payload) -> Result<ValidatedSlots> {
            self.calls.set(self.calls.get() + 1);
            Err(self.error.clone())
        }
    }

    #[test]
    fn validator_and_handler_failures_propagate_without_retry_or_wrapping() {
        let services = services();
        let request = request(0, r#"{"value":"ok"}"#, None);
        let error = Error::new(ErrorCode::Cancelled, ErrorSource::SchemaVm, 37,
            "deterministic validator failure").with_schema(19, 29);
        let rejecting = RejectingValidator { error: error.clone(), calls: Cell::new(0) };
        let handler = Handler::default();
        let result = dispatch(&request, &services, context(&request, &[]), &rejecting, &handler);
        match result {
            Err(actual) => assert_eq!(actual, error),
            Ok(_) => panic!("validator failure must propagate"),
        }
        assert_eq!(rejecting.calls.get(), 1);
        assert_eq!(handler.calls.get(), 0);
        let validator = ValueValidator::default();
        let error = Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 41,
            "deterministic handler failure").with_schema(31, 43);
        let handler = Handler { failure: Some(error.clone()), ..Handler::default() };
        match dispatch(&request, &services, context(&request, &[]), &validator, &handler) {
            Err(actual) => assert_eq!(actual, error),
            Ok(_) => panic!("handler failure must propagate"),
        }
        assert_eq!(validator.calls.get(), 1);
        assert_eq!(handler.calls.get(), 1);
        assert!(handler.retained.borrow().is_some());
    }
}
