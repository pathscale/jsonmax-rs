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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use core::cell::RefCell;

    struct Provider<F> {
        calls: RefCell<Vec<(u32, u32)>>,
        run: F,
    }

    impl<F> ToolSchemaProvider for Provider<F>
    where F: Fn(&EndpointSchemaSpec, u32) -> Result<Payload> {
        fn input_schema(&self, endpoint: &EndpointSchemaSpec, max_bytes: u32) -> Result<Payload> {
            self.calls.borrow_mut().push((endpoint.code, max_bytes));
            (self.run)(endpoint, max_bytes)
        }
    }

    fn provider<F>(run: F) -> Provider<F>
    where F: Fn(&EndpointSchemaSpec, u32) -> Result<Payload> {
        Provider { calls: RefCell::new(Vec::new()), run }
    }

    fn endpoint(service: &str, name: &str, code: u32, roles: &[&str]) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, name: String::from(name), service: String::from(service),
            roles: roles.iter().map(|role| String::from(*role)).collect(),
            params: Vec::new(), response: Vec::new(), stream_response: None,
            description: String::new(),
        }
    }

    fn catalog() -> Vec<ServiceSpec> {
        vec![
            ServiceSpec {
                id: 0, name: String::from("Zoo"), endpoints: vec![
                    endpoint("Zoo", "public", 0, &[]),
                    endpoint("Zoo", "private", u32::MAX, &["reader", "admin"]),
                    endpoint("Zoo", "write", 8, &["writer"]),
                ],
            },
            ServiceSpec {
                id: u16::MAX, name: String::from("Alpha"),
                endpoints: vec![endpoint("Alpha", "public", 9, &[])],
            },
        ]
    }

    fn single() -> Vec<ServiceSpec> {
        vec![ServiceSpec {
            id: 0, name: String::from("s"),
            endpoints: vec![endpoint("s", "e", 0, &[])],
        }]
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| String::from(*value)).collect()
    }

    fn assert_local(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Wire);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
    }

    fn invalid_catalog(services: &[ServiceSpec], roles: &[String], code: ErrorCode) {
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_local(list_tools(services, roles, &schemas, u32::MAX).unwrap_err(), code);
        assert!(schemas.calls.borrow().is_empty());
        // A valid public target and an unknown target both require validation
        // of the full catalog, including unrelated hidden endpoints.
        assert_local(find_tool(services, roles, "Zoo.public").unwrap_err(), code);
        assert_local(find_tool(services, roles, "missing.tool").unwrap_err(), code);
    }

    #[test]
    fn empty_and_fully_hidden_catalogs_fit_a_zero_budget() {
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert!(list_tools(&[], &[], &schemas, 0).unwrap().is_empty());
        let mut services = single();
        services[0].endpoints[0].roles = strings(&["admin"]);
        services[0].endpoints[0].description = String::from("hidden metadata");
        assert!(list_tools(&services, &[], &schemas, 0).unwrap().is_empty());
        assert!(schemas.calls.borrow().is_empty());
        assert_local(find_tool(&[], &[], "s.e").unwrap_err(), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn list_preserves_catalog_order_codes_and_exact_metadata() {
        let mut services = catalog();
        services[0].endpoints[0].description = String::from("é\n\0🚀");
        let raw = b" \n{ \"type\" : \"object\", \"additionalProperties\":false }\t";
        let schemas = provider(|_, _| Ok(Payload::from_slice(raw)));
        let tools = list_tools(&services, &strings(&["reader"]), &schemas, 4096).unwrap();
        assert_eq!(tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
            ["Zoo.public", "Zoo.private", "Alpha.public"]);
        assert_eq!(tools.iter().map(|tool| tool.endpoint_code).collect::<Vec<_>>(),
            [0, u32::MAX, 9]);
        assert_eq!(tools[0].description, "é\n\0🚀");
        assert!(tools.iter().all(|tool| tool.input_schema.as_slice() == raw));
        assert_eq!(schemas.calls.borrow().iter().map(|call| call.0).collect::<Vec<_>>(),
            [0, u32::MAX, 9]);
        drop(services);
        drop(schemas);
        assert_eq!(tools[0].name, "Zoo.public");
        assert_eq!(tools[0].input_schema.as_slice(), raw);
    }

    #[test]
    fn authorization_is_exact_case_sensitive_any_role_intersection() {
        let services = catalog();
        for grants in [&[][..], &["Reader"][..], &["administrator"][..], &["reader "][..]] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
            let roles = strings(grants);
            assert_eq!(list_tools(&services, &roles, &schemas, 1024).unwrap().len(), 2);
            assert_local(find_tool(&services, &roles, "Zoo.private").unwrap_err(), ErrorCode::AuthFailed);
        }
        for grants in [&["reader"][..], &["admin"][..], &["unrelated", "reader"][..]] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
            let roles = strings(grants);
            assert_eq!(list_tools(&services, &roles, &schemas, 1024).unwrap().len(), 3);
            assert_eq!(find_tool(&services, &roles, "Zoo.private").unwrap().code, u32::MAX);
        }
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_eq!(list_tools(&services, &strings(&["reader", "writer"]), &schemas, 1024).unwrap().len(), 4);
    }

    #[test]
    fn find_borrows_original_endpoint_and_rejects_noncanonical_names() {
        let services = catalog();
        assert!(core::ptr::eq(find_tool(&services, &[], "Zoo.public").unwrap(),
            &services[0].endpoints[0]));
        for name in ["", "Zoo", "public", ".public", "Zoo.", "Zoo.public.extra", "Zoo..public",
            "zoo.public", "Zoo.Public", " Zoo.public", "Zoo.public ", "Zoo.public\0", "Zoo/public", "世界.public"] {
            assert_local(find_tool(&services, &[], name).unwrap_err(), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn duplicate_service_ids_and_names_include_empty_services() {
        let mut services = catalog();
        services.push(ServiceSpec { id: 0, name: String::from("Empty"), endpoints: Vec::new() });
        invalid_catalog(&services, &[], ErrorCode::DuplicateObjectKey);
        services[2].id = 1;
        services[2].name = String::from("Zoo");
        invalid_catalog(&services, &[], ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn endpoint_codes_are_global_and_names_are_per_service() {
        let mut services = catalog();
        services[0].endpoints[2].code = 0;
        invalid_catalog(&services, &[], ErrorCode::DuplicateObjectKey);
        let mut services = catalog();
        services[1].endpoints[0].code = u32::MAX;
        invalid_catalog(&services, &[], ErrorCode::DuplicateObjectKey);
        let mut services = catalog();
        services[0].endpoints[2].name = String::from("public");
        invalid_catalog(&services, &[], ErrorCode::DuplicateObjectKey);
        // The original catalog intentionally repeats "public" across services.
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_eq!(list_tools(&catalog(), &[], &schemas, 1024).unwrap().len(), 2);
    }

    #[test]
    fn mismatched_hidden_endpoint_service_is_rejected_before_provider_calls() {
        let mut services = catalog();
        services[0].endpoints[2].service = String::from("Alpha");
        invalid_catalog(&services, &[], ErrorCode::ConstraintViolation);
    }

    #[test]
    fn invalid_ascii_identifiers_are_rejected_everywhere() {
        for name in ["", "0bad", "a.b", "a-b", "a b", "a/b", "a\n", "a\0", "é", "a\u{7f}"] {
            let mut services = catalog();
            services[0].endpoints[2].name = String::from(name);
            invalid_catalog(&services, &[], ErrorCode::ConstraintViolation);
            let mut services = catalog();
            services.push(ServiceSpec { id: 1, name: String::from(name), endpoints: Vec::new() });
            invalid_catalog(&services, &[], ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn ascii_identifier_boundaries_match_root_tool_names() {
        for service_name in ["_", "A", "z", "_09AZaz"] {
            for endpoint_name in ["_", "a0", "Z9", "_09AZaz"] {
                let services = vec![ServiceSpec { id: u16::MAX, name: String::from(service_name),
                    endpoints: vec![endpoint(service_name, endpoint_name, u32::MAX, &[])] }];
                let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
                let tools = list_tools(&services, &[], &schemas, 1024).unwrap();
                let name = tool_name(service_name, endpoint_name).unwrap();
                assert_eq!(tools[0].name, name);
                assert!(core::ptr::eq(find_tool(&services, &[], &name).unwrap(), &services[0].endpoints[0]));
            }
        }
    }

    #[test]
    fn empty_and_duplicate_granted_or_required_roles_are_invalid() {
        invalid_catalog(&catalog(), &strings(&[""]), ErrorCode::ConstraintViolation);
        invalid_catalog(&catalog(), &strings(&["reader", "reader"]), ErrorCode::DuplicateObjectKey);
        invalid_catalog(&[], &strings(&[""]), ErrorCode::ConstraintViolation);
        for (roles, code) in [(&[""][..], ErrorCode::ConstraintViolation),
            (&["reader", "reader"][..], ErrorCode::DuplicateObjectKey)] {
            let mut services = catalog();
            services[0].endpoints[2].roles = strings(roles);
            invalid_catalog(&services, &[], code);
        }
    }

    #[test]
    fn roles_are_not_identifiers_and_are_not_normalized() {
        let mut services = single();
        services[0].endpoints[0].roles = strings(&["é", " role ", "\0"]);
        for grant in ["é", " role ", "\0"] {
            assert!(find_tool(&services, &strings(&[grant]), "s.e").is_ok());
        }
        assert_local(find_tool(&services, &strings(&["role"]), "s.e").unwrap_err(), ErrorCode::AuthFailed);
    }

    #[test]
    fn aggregate_budget_counts_utf8_bytes_at_the_exact_boundary() {
        let mut services = single();
        services[0].endpoints[0].description = String::from("é🚀");
        let raw = " \n{\"title\":\"界\"}\t";
        let exact = (3 + "é🚀".len() + raw.len()) as u32;
        let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
        assert_eq!(list_tools(&services, &[], &schemas, exact).unwrap().len(), 1);
        assert_eq!(*schemas.calls.borrow(), [(0, raw.len() as u32)]);
        let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
        assert_local(list_tools(&services, &[], &schemas, exact - 1).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(schemas.calls.borrow().len(), 1);
    }

    #[test]
    fn all_metadata_is_charged_before_any_provider_is_invoked() {
        let mut services = single();
        services[0].endpoints.push(endpoint("s", "f", 1, &[]));
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_local(list_tools(&services, &[], &schemas, 5).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert!(schemas.calls.borrow().is_empty());
        assert_local(list_tools(&services, &[], &schemas, 0).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert!(schemas.calls.borrow().is_empty());
    }

    #[test]
    fn each_provider_receives_remaining_budget_and_is_called_once() {
        let mut services = single();
        services[0].endpoints.push(endpoint("s", "f", 1, &[]));
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_eq!(list_tools(&services, &[], &schemas, 10).unwrap().len(), 2);
        assert_eq!(*schemas.calls.borrow(), [(0, 4), (1, 2)]);
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_local(list_tools(&services, &[], &schemas, 9).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(*schemas.calls.borrow(), [(0, 3), (1, 1)]);
    }

    #[test]
    fn minimum_object_schema_requires_two_remaining_bytes() {
        for (limit, remaining) in [(3, 0), (4, 1)] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
            assert_local(list_tools(&single(), &[], &schemas, limit).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_eq!(*schemas.calls.borrow(), [(0, remaining)]);
        }
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{}")));
        assert_eq!(list_tools(&single(), &[], &schemas, 5).unwrap().len(), 1);
        assert_eq!(*schemas.calls.borrow(), [(0, 2)]);
    }

    #[test]
    fn checked_byte_accounting_rejects_overflow_and_accepts_u32_boundary() {
        assert_eq!(charge(u32::MAX - 1, 1, u32::MAX).unwrap(), u32::MAX);
        assert_eq!(charge(u32::MAX, 0, u32::MAX).unwrap(), u32::MAX);
        assert_eq!(charge(0, 0, 0).unwrap(), 0);
        assert_local(charge(u32::MAX, 1, u32::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_local(charge(4, 1, 4).unwrap_err(), ErrorCode::PayloadTooLarge);
        if let Some(oversize) = (u32::MAX as usize).checked_add(1) {
            assert_local(charge(0, oversize, u32::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn hidden_endpoints_never_reach_the_provider() {
        let schemas = provider(|endpoint, _| {
            assert!(endpoint.roles.is_empty());
            Ok(Payload::from_slice(b"{}"))
        });
        assert_eq!(list_tools(&catalog(), &[], &schemas, 1024).unwrap().len(), 2);
        assert_eq!(schemas.calls.borrow().iter().map(|call| call.0).collect::<Vec<_>>(), [0, 9]);
    }

    #[test]
    fn provider_errors_propagate_exactly_and_stop_further_calls() {
        let expected = Error::new(ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile,
            17, "unresolved named schema").with_schema(7, 8);
        let schemas = provider(|_, _| Err(expected.clone()));
        assert_eq!(list_tools(&catalog(), &[], &schemas, 1024).unwrap_err(), expected);
        assert_eq!(schemas.calls.borrow().len(), 1);
        for code in [ErrorCode::BudgetExhausted, ErrorCode::ConstraintViolation] {
            let expected = Error::new(code, ErrorSource::SchemaCompile, 3, "schema provider error");
            let schemas = provider(|_, _| Err(expected.clone()));
            assert_eq!(list_tools(&single(), &[], &schemas, 1024).unwrap_err(), expected);
        }
    }

    #[test]
    fn malformed_schemas_propagate_canonical_parser_errors_and_offsets() {
        for (raw, code, offset) in [
            ("", ErrorCode::UnexpectedEof, 0),
            ("{", ErrorCode::UnexpectedEof, 1),
            ("{} false", ErrorCode::TrailingContent, 3),
            (r#"{"a":1,}"#, ErrorCode::InvalidSyntax, 7),
            (r#"{"a":01}"#, ErrorCode::InvalidNumber, 6),
            (r#"{"a":"\q"}"#, ErrorCode::InvalidEscape, 7),
            (r#"{"a":"\uD800"}"#, ErrorCode::LoneSurrogate, 6),
            ("{\"a\":\"\n\"}", ErrorCode::ControlCharInString, 6),
        ] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
            let error = list_tools(&single(), &[], &schemas, 1024).unwrap_err();
            assert_eq!(error.byte_offset, offset, "schema: {raw:?}");
            assert_local(error, code);
            assert_eq!(schemas.calls.borrow().len(), 1);
        }
    }

    #[test]
    fn invalid_schema_utf8_is_rejected() {
        let schemas = provider(|_, _| Ok(Payload::from_slice(b"{\"a\":\"\xff\"}")));
        let error = list_tools(&single(), &[], &schemas, 1024).unwrap_err();
        assert_eq!(error.byte_offset, 6);
        assert_local(error, ErrorCode::InvalidUtf8);
    }

    #[test]
    fn later_invalid_schema_discards_prior_output_and_stops_discovery() {
        let mut services = single();
        services[0].endpoints.push(endpoint("s", "f", 1, &[]));
        services[0].endpoints.push(endpoint("s", "g", 2, &[]));
        let schemas = provider(|endpoint, _| {
            Ok(Payload::from_slice(if endpoint.code == 0 { b"{}" } else { b"[]" }))
        });
        assert_local(list_tools(&services, &[], &schemas, 1024).unwrap_err(), ErrorCode::TypeMismatch);
        assert_eq!(schemas.calls.borrow().iter().map(|call| call.0).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn only_complete_json_objects_are_accepted_as_schemas() {
        for raw in ["[]", "[{}]", "null", "true", "42", "\"object\"", " \t[]"] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
            let error = list_tools(&single(), &[], &schemas, 1024).unwrap_err();
            assert_eq!(error.byte_offset, if raw.starts_with(' ') { 2 } else { 0 });
            assert_local(error, ErrorCode::TypeMismatch);
        }
        for raw in ["{}", " \t\r\n{}\n", r#"{"unknown":[true,null,1.2e3]}"#] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
            assert_eq!(list_tools(&single(), &[], &schemas, 1024).unwrap()[0].input_schema.as_slice(), raw.as_bytes());
        }
    }

    #[test]
    fn duplicate_decoded_schema_keys_are_rejected_at_every_level() {
        for raw in [
            r#"{"type":"object","type":"object"}"#,
            r#"{"type":"object","\u0074ype":"object"}"#,
            r#"{"properties":{"x":{"type":"string","type":"null"}}}"#,
            r#"{"allOf":[{"é":1,"\u00e9":2}]}"#,
        ] {
            let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
            assert_local(list_tools(&single(), &[], &schemas, 1024).unwrap_err(), ErrorCode::DuplicateObjectKey);
        }
        let raw = br#"{"allOf":[{"type":"string"},{"type":"null"}]}"#;
        let schemas = provider(|_, _| Ok(Payload::from_slice(raw)));
        assert!(list_tools(&single(), &[], &schemas, 1024).is_ok());
    }

    #[test]
    fn local_recursive_schema_and_provider_mapping_are_preserved_verbatim() {
        let mut services = single();
        services[0].endpoints[0].params.push(EndpointField {
            name: String::from("root"), ty: EndpointType::StructRef(String::from("Node")),
            description: String::from("root node"),
        });
        let raw = br##"{"type":"object","properties":{"root":{"$ref":"#/$defs/Node"}},"required":["root"],"additionalProperties":false,"$defs":{"Node":{"type":"object","properties":{"next":{"anyOf":[{"$ref":"#/$defs/Node"},{"type":"null"}]}},"additionalProperties":false}}}"##;
        let schemas = provider(|endpoint, _| {
            assert!(core::ptr::eq(endpoint, &services[0].endpoints[0]));
            assert_eq!(endpoint.params[0].ty, EndpointType::StructRef(String::from("Node")));
            Ok(Payload::from_slice(raw))
        });
        let tools = list_tools(&services, &[], &schemas, 4096).unwrap();
        assert_eq!(tools[0].input_schema.as_slice(), raw);
        assert_eq!(schemas.calls.borrow().len(), 1);
    }

    #[test]
    fn schema_limits_follow_the_explicit_budget_instead_of_parser_defaults() {
        let depth = usize::from(EnvelopeLimits::default().max_depth) + 1;
        let raw = format!("{{\"x\":{}0{}}}", "[".repeat(depth), "]".repeat(depth));
        let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
        assert!(list_tools(&single(), &[], &schemas, (raw.len() + 3) as u32).is_ok());
        let mut raw = String::from("{");
        for index in 0..=EnvelopeLimits::default().max_fields {
            if index != 0 { raw.push(','); }
            raw.push_str(&format!("\"k{index}\":null"));
        }
        raw.push('}');
        let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
        assert!(list_tools(&single(), &[], &schemas, (raw.len() + 3) as u32).is_ok());
    }

    #[test]
    fn full_abi_depth_limit_accepts_the_boundary_and_rejects_one_more_container() {
        for (arrays, succeeds) in [(usize::from(u16::MAX) - 1, true),
            (usize::from(u16::MAX), false)] {
            // The root schema object itself consumes one container depth.
            let raw = format!("{{\"x\":{}0{}}}", "[".repeat(arrays), "]".repeat(arrays));
            let schemas = provider(|_, _| Ok(Payload::from_slice(raw.as_bytes())));
            let result = list_tools(&single(), &[], &schemas, (raw.len() + 3) as u32);
            if succeeds {
                assert_eq!(result.unwrap()[0].input_schema.as_slice(), raw.as_bytes());
            } else {
                assert_local(result.unwrap_err(), ErrorCode::DepthLimitExceeded);
            }
            assert_eq!(schemas.calls.borrow().len(), 1);
        }
    }

    #[test]
    fn first_configuration_error_is_deterministic_before_budget_or_provider_errors() {
        let mut services = catalog();
        services[0].endpoints[2].service = String::from("wrong");
        services[1].id = services[0].id;
        let schemas = provider(|_, _| Err(local_error(ErrorCode::InternalError, "provider failure")));
        for _ in 0..4 {
            let error = list_tools(&services, &[], &schemas, 0).unwrap_err();
            assert_eq!(error.detail, "endpoint service mismatch");
            assert_local(error, ErrorCode::ConstraintViolation);
        }
        assert!(schemas.calls.borrow().is_empty());
    }
}
