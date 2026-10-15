//! Semantic validation of normalized endpoint configurations.
//!
//! The endpoint-model registry owns the canonical invariants. Keeping this
//! adapter on that path gives loaded configurations the same reference, method,
//! role, header and error semantics as models constructed by other adapters.

use crate::{EndpointModel, Result};

/// Validate every record in a normalized model, including unused declarations.
///
/// Registry construction checks names, global endpoint codes, canonical methods,
/// reference kinds and depth, nominal recursion, roles, headers and error keys.
/// Its canonical ABI errors are returned unchanged, including diagnostic context
/// and deterministic declaration-order failures. The temporary borrowed index
/// is discarded on success; the caller's model is never modified.
///
/// This validates model semantics only. Version requirements are checked by
/// `crate::version::validate_version`, and no payload validation proof is issued.
pub fn validate_model(model: &EndpointModel) -> Result<()> {
    jmax_endpoint_model::registry::build_registry(model).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
    use crate::{
        EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, Error,
        ErrorCode, ErrorSource, ErrorSpec, HeaderSpec, ModelVersion, RoleSpec,
        ServiceSpec, TypeDefinition,
    };
    use jmax_endpoint_model::{registry, MAX_REFERENCE_DEPTH};

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::from("field metadata") }
    }

    fn record(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn variant(name: &str, value: i32) -> EnumVariantSpec {
        EnumVariantSpec { name: String::from(name), value, description: String::new() }
    }

    fn enumeration(name: &str) -> EndpointType {
        EndpointType::Enum {
            name: String::from(name),
            variants: vec![variant("Minimum", i32::MIN), variant("Maximum", i32::MAX)],
        }
    }

    fn definition(name: &str, ty: EndpointType) -> TypeDefinition {
        TypeDefinition { name: String::from(name), ty }
    }

    fn role(name: &str, includes: &[&str]) -> RoleSpec {
        RoleSpec {
            name: String::from(name),
            includes: includes.iter().map(|name| String::from(*name)).collect(),
            description: String::from("role metadata"),
        }
    }

    fn error_spec(code: u32, name: &str) -> ErrorSpec {
        ErrorSpec { code, name: String::from(name), message: String::from("message\nwith metadata") }
    }

    fn header(name: &str, ty: EndpointType) -> HeaderSpec {
        HeaderSpec { name: String::from(name), ty, required: false, description: String::new() }
    }

    fn endpoint(code: u32, service: &str, name: &str) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code, name: String::from(name), service: String::from(service),
            roles: Vec::new(), params: Vec::new(), response: Vec::new(),
            stream_response: None, description: String::from("endpoint metadata"),
        }
    }

    fn service(id: u16, name: &str, endpoints: Vec<EndpointSchemaSpec>) -> ServiceSpec {
        ServiceSpec { id, name: String::from(name), endpoints }
    }

    fn model() -> EndpointModel {
        let mut read = endpoint(0, "Store", "read");
        read.roles.push(String::from("Reader"));
        read.params.push(field("検索 語", EndpointType::UInt32));
        read.response.push(field("node", EndpointType::StructRef(String::from("NodeAlias"))));
        read.stream_response = Some(EndpointType::StructTable { struct_ref: String::from("Node") });
        EndpointModel {
            version: ModelVersion::new(1, 2, 3),
            services: vec![
                service(0, "Store", vec![read]),
                service(u16::MAX, "Health", vec![endpoint(u32::MAX, "Health", "ping")]),
            ],
            definitions: vec![
                definition("NodeAlias", EndpointType::StructRef(String::from("Node"))),
                definition("StateAlias", EndpointType::EnumRef {
                    name: String::from("State"), prefixed_name: true,
                }),
                definition("Node", record("Node", vec![field("next", EndpointType::Optional(
                    Box::new(EndpointType::StructRef(String::from("Node"))),
                ))])),
                definition("State", enumeration("State")),
            ],
            roles: vec![role("Admin", &["Writer"]), role("Writer", &["Reader"]), role("Reader", &[])],
            errors: vec![error_spec(0, "Unavailable"), error_spec(u32::MAX, "Denied")],
            headers: vec![header("X-State", EndpointType::EnumRef {
                name: String::from("StateAlias"), prefixed_name: false,
            })],
        }
    }

    fn failure(model: &EndpointModel, code: ErrorCode) -> Error {
        let error = validate_model(model).expect_err("invalid model must be rejected");
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        assert!(!error.detail.is_empty());
        assert_eq!(Err(error.clone()), registry::build_registry(model).map(|_| ()));
        error
    }

    #[derive(Clone, Copy, Debug)]
    enum TypeSite { Params, Response, Stream, Definition, Header }

    const TYPE_SITES: [TypeSite; 5] = [
        TypeSite::Params, TypeSite::Response, TypeSite::Stream,
        TypeSite::Definition, TypeSite::Header,
    ];

    fn model_with_type(site: TypeSite, ty: EndpointType) -> EndpointModel {
        let mut model = EndpointModel {
            services: vec![service(0, "Api", vec![endpoint(0, "Api", "call")])],
            ..EndpointModel::default()
        };
        match site {
            TypeSite::Params => model.services[0].endpoints[0].params.push(field("value", ty)),
            TypeSite::Response => model.services[0].endpoints[0].response.push(field("value", ty)),
            TypeSite::Stream => model.services[0].endpoints[0].stream_response = Some(ty),
            TypeSite::Definition => {
                let name = match &ty {
                    EndpointType::Struct { name, .. } | EndpointType::Enum { name, .. } => name.clone(),
                    _ => String::from("Unused"),
                };
                model.definitions.push(definition(&name, ty));
            }
            TypeSite::Header => model.headers.push(header("X-Value", ty)),
        }
        model
    }

    fn nested_type(depth: usize, mut ty: EndpointType) -> EndpointType {
        for index in 0..depth {
            ty = if index % 2 == 0 {
                EndpointType::Optional(Box::new(ty))
            } else {
                EndpointType::Vec(Box::new(ty))
            };
        }
        ty
    }

    #[test]
    fn empty_model_and_semantic_validation_without_version_requirements_are_valid() {
        let mut empty = EndpointModel::default();
        assert_eq!(validate_model(&empty), Ok(()));
        empty.version = ModelVersion::new(u32::MAX, u32::MAX, u32::MAX);
        assert_eq!(validate_model(&empty), Ok(()));
    }

    #[test]
    fn complete_model_preserves_order_metadata_and_numeric_boundaries() {
        let model = model();
        let original = model.clone();
        assert_eq!(validate_model(&model), Ok(()));
        assert_eq!(validate_model(&model), Ok(()));
        assert_eq!(model, original);
        let registry = registry::build_registry(&model).unwrap();
        for (code, method) in [(0, "Store.read"), (u32::MAX, "Health.ping")] {
            let by_code = registry::endpoint_by_code(&registry, code).unwrap();
            assert!(core::ptr::eq(by_code, registry::endpoint_by_method(&registry, method).unwrap()));
            assert_eq!(registry::error_by_code(&registry, code).unwrap().code, code);
        }
        assert_eq!(registry::service_by_id(&registry, 0).unwrap().name, "Store");
        assert_eq!(registry::service_by_id(&registry, u16::MAX).unwrap().name, "Health");
    }

    #[test]
    fn service_and_endpoint_keys_are_unique_in_their_required_scopes() {
        let mut duplicate_id = model();
        duplicate_id.services[1].id = 0;
        assert_eq!(failure(&duplicate_id, ErrorCode::ConstraintViolation).detail, "duplicate service id");

        let mut duplicate_service = model();
        duplicate_service.services[1].name = String::from("Store");
        failure(&duplicate_service, ErrorCode::ConstraintViolation);

        let mut duplicate_code = model();
        duplicate_code.services[1].endpoints[0].code = 0;
        assert_eq!(failure(&duplicate_code, ErrorCode::ConstraintViolation).detail, "duplicate endpoint code");

        let mut duplicate_method = model();
        duplicate_method.services[0].endpoints.push(endpoint(1, "Store", "read"));
        failure(&duplicate_method, ErrorCode::ConstraintViolation);

        let mut distinct_methods = model();
        distinct_methods.services[1].endpoints[0].name = String::from("read");
        assert_eq!(validate_model(&distinct_methods), Ok(()));
    }

    #[test]
    fn method_components_are_exact_ascii_identifiers() {
        for invalid in ["", "_", "1start", "Api.call", "has-dash", " has_space", "café", "a\0b"] {
            let mut bad_service = EndpointModel::default();
            bad_service.services.push(service(0, invalid, Vec::new()));
            failure(&bad_service, ErrorCode::ConstraintViolation);
            let mut bad_endpoint = model();
            bad_endpoint.services[0].endpoints[0].name = String::from(invalid);
            failure(&bad_endpoint, ErrorCode::ConstraintViolation);
        }
        let valid = EndpointModel {
            services: vec![service(0, "_Api2", vec![endpoint(0, "_Api2", "_call3")])],
            ..EndpointModel::default()
        };
        assert_eq!(validate_model(&valid), Ok(()));
    }

    #[test]
    fn endpoint_service_must_match_with_exact_case() {
        for name in ["", "store", "Health", "Store.read"] {
            let mut model = model();
            model.services[0].endpoints[0].service = String::from(name);
            failure(&model, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn independent_name_namespaces_and_case_sensitive_methods_are_accepted() {
        let mut model = EndpointModel {
            services: vec![
                service(0, "Shared", vec![endpoint(0, "Shared", "read"), endpoint(1, "Shared", "Read")]),
                service(1, "shared", Vec::new()),
            ],
            definitions: vec![definition("Shared", record("Shared", Vec::new()))],
            roles: vec![role("Shared", &[])],
            errors: vec![error_spec(0, "Shared")],
            ..EndpointModel::default()
        };
        model.services[0].endpoints[0].roles.push(String::from("Shared"));
        assert_eq!(validate_model(&model), Ok(()));
    }

    #[test]
    fn every_type_root_rejects_unknown_references_even_when_unused() {
        for site in TYPE_SITES {
            for ty in [
                EndpointType::StructRef(String::from("Missing")),
                EndpointType::EnumRef { name: String::from("Missing"), prefixed_name: false },
                EndpointType::StructTable { struct_ref: String::from("Missing") },
            ] {
                let model = model_with_type(site, nested_type(3, ty));
                assert_eq!(failure(&model, ErrorCode::ConstraintViolation).detail, "unknown type reference");
            }
            let ty = record("Container", vec![field("missing", EndpointType::StructRef(String::from("Missing")))]);
            failure(&model_with_type(site, ty), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn reference_kinds_are_checked_at_every_type_root() {
        for site in TYPE_SITES {
            for ty in [
                EndpointType::StructRef(String::from("State")),
                EndpointType::StructTable { struct_ref: String::from("State") },
                EndpointType::EnumRef { name: String::from("Node"), prefixed_name: true },
            ] {
                let mut model = model_with_type(site, ty);
                model.definitions.push(definition("State", enumeration("State")));
                model.definitions.push(definition("Node", record("Node", Vec::new())));
                failure(&model, ErrorCode::ConstraintViolation);
            }
        }
        let mut scalar_target = model_with_type(TypeSite::Stream, EndpointType::StructRef(String::from("Scalar")));
        scalar_target.definitions.push(definition("Scalar", EndpointType::UInt32));
        failure(&scalar_target, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn forward_inline_declarations_are_visible_from_other_roots() {
        let mut model = model_with_type(TypeSite::Params, EndpointType::StructRef(String::from("Inline")));
        model.services[0].endpoints[0].response.push(field("value", record("Inline", Vec::new())));
        model.services[0].endpoints[0].stream_response = Some(EndpointType::EnumRef {
            name: String::from("InlineState"), prefixed_name: true,
        });
        model.headers.push(header("X-State", enumeration("InlineState")));
        assert_eq!(validate_model(&model), Ok(()));
    }

    #[test]
    fn mutual_nominal_recursion_and_struct_table_recursion_are_valid() {
        let model = EndpointModel {
            definitions: vec![
                definition("Left", record("Left", vec![field("right", EndpointType::Optional(
                    Box::new(EndpointType::StructRef(String::from("Right"))),
                ))])),
                definition("Right", record("Right", vec![field("left", EndpointType::StructTable {
                    struct_ref: String::from("Left"),
                })])),
            ],
            ..EndpointModel::default()
        };
        assert_eq!(validate_model(&model), Ok(()));
    }

    #[test]
    fn unused_alias_cycles_and_nominal_kind_changes_fail() {
        for definitions in [
            vec![definition("A", EndpointType::StructRef(String::from("A")))],
            vec![
                definition("A", EndpointType::StructRef(String::from("B"))),
                definition("B", EndpointType::StructRef(String::from("A"))),
            ],
            vec![
                definition("A", EndpointType::EnumRef { name: String::from("B"), prefixed_name: false }),
                definition("B", EndpointType::EnumRef { name: String::from("A"), prefixed_name: true }),
            ],
            vec![
                definition("A", EndpointType::StructRef(String::from("B"))),
                definition("B", EndpointType::EnumRef { name: String::from("State"), prefixed_name: false }),
                definition("State", enumeration("State")),
            ],
        ] {
            failure(&EndpointModel { definitions, ..EndpointModel::default() }, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn duplicate_definition_names_and_nominal_name_mismatches_fail() {
        let mut duplicate = model();
        duplicate.definitions.push(definition("NodeAlias", EndpointType::UInt32));
        failure(&duplicate, ErrorCode::ConstraintViolation);
        for ty in [record("Other", Vec::new()), enumeration("Other")] {
            let model = EndpointModel {
                definitions: vec![definition("Declared", ty)],
                ..EndpointModel::default()
            };
            failure(&model, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn separately_owned_inline_nominal_declarations_cannot_reuse_names() {
        let mut model = model_with_type(TypeSite::Params, record("Repeated", Vec::new()));
        model.services[0].endpoints[0].response.push(field("value", record("Repeated", Vec::new())));
        failure(&model, ErrorCode::ConstraintViolation);
        model.services[0].endpoints[0].response[0].ty = EndpointType::StructRef(String::from("Repeated"));
        assert_eq!(validate_model(&model), Ok(()));
        model.definitions.push(definition("Repeated", record("Repeated", Vec::new())));
        failure(&model, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn ownership_depth_accepts_the_limit_and_rejects_the_next_edge() {
        for site in TYPE_SITES {
            let valid = model_with_type(site, nested_type(MAX_REFERENCE_DEPTH, EndpointType::Unit));
            assert_eq!(validate_model(&valid), Ok(()), "site: {site:?}");
            let invalid = model_with_type(site, nested_type(MAX_REFERENCE_DEPTH + 1, EndpointType::Unit));
            failure(&invalid, ErrorCode::DepthLimitExceeded);
        }
    }

    fn alias_chain(hops: usize) -> EndpointModel {
        let mut model = EndpointModel::default();
        for index in 0..hops {
            model.definitions.push(definition(&format!("Alias{index}"), EndpointType::StructRef(
                if index + 1 == hops { String::from("Terminal") } else { format!("Alias{}", index + 1) },
            )));
        }
        model.definitions.push(definition("Terminal", record("Terminal", Vec::new())));
        model
    }

    #[test]
    fn alias_depth_accepts_the_limit_and_rejects_the_next_hop() {
        assert_eq!(validate_model(&alias_chain(MAX_REFERENCE_DEPTH)), Ok(()));
        failure(&alias_chain(MAX_REFERENCE_DEPTH + 1), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn ownership_and_reference_edges_share_one_depth_budget() {
        let mut model = alias_chain(1);
        let ty = EndpointType::StructRef(String::from("Alias0"));
        model.headers.push(header("X-Depth", nested_type(MAX_REFERENCE_DEPTH - 2, ty.clone())));
        // Alias0 and Terminal are two reference edges from this root.
        assert_eq!(validate_model(&model), Ok(()));
        model.headers[0].ty = nested_type(MAX_REFERENCE_DEPTH - 1, ty);
        failure(&model, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn a_shallow_visit_does_not_hide_an_overdeep_reference_path() {
        let mut model = model_with_type(TypeSite::Params, EndpointType::StructRef(String::from("Target")));
        model.definitions.push(definition("Target", record("Target", vec![field("leaf", EndpointType::Unit)])));
        model.services[0].endpoints[0].response.push(field("deep", nested_type(
            MAX_REFERENCE_DEPTH - 1, EndpointType::StructRef(String::from("Target")),
        )));
        failure(&model, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn scalar_types_and_nanoid_boundaries_are_valid_at_every_root() {
        let types = [
            EndpointType::UInt32, EndpointType::Int32, EndpointType::Int64,
            EndpointType::Float64, EndpointType::Boolean, EndpointType::String,
            EndpointType::Bytea, EndpointType::UUID, EndpointType::IpAddr,
            EndpointType::Object, EndpointType::Unit, EndpointType::TimeStampMs,
            EndpointType::BlockchainDecimal, EndpointType::BlockchainAddress,
            EndpointType::BlockchainTransactionHash,
            EndpointType::NanoId { len: 1 }, EndpointType::NanoId { len: u8::MAX },
        ];
        for site in TYPE_SITES {
            for ty in &types {
                assert_eq!(validate_model(&model_with_type(site, ty.clone())), Ok(()));
            }
            failure(&model_with_type(site, EndpointType::NanoId { len: 0 }), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn field_names_are_nonempty_unique_within_each_record_and_preserve_utf8() {
        let mut model = model_with_type(TypeSite::Params, EndpointType::Unit);
        model.services[0].endpoints[0].response.push(field("value", EndpointType::Unit));
        assert_eq!(validate_model(&model), Ok(()));
        for site in TYPE_SITES {
            for fields in [
                vec![field("", EndpointType::Unit)],
                vec![field("same", EndpointType::Unit), field("same", EndpointType::UInt32)],
            ] {
                failure(&model_with_type(site, record("Fields", fields)), ErrorCode::ConstraintViolation);
            }
            let valid = record("Fields", vec![field("with spaces", EndpointType::Unit), field("字段", EndpointType::Unit)]);
            assert_eq!(validate_model(&model_with_type(site, valid)), Ok(()));
        }
        for response in [false, true] {
            for invalid_fields in [
                vec![field("", EndpointType::Unit)],
                vec![field("duplicate", EndpointType::Unit), field("duplicate", EndpointType::Unit)],
            ] {
                let mut model = model_with_type(TypeSite::Params, EndpointType::Unit);
                let endpoint = &mut model.services[0].endpoints[0];
                let fields = if response { &mut endpoint.response } else { &mut endpoint.params };
                *fields = invalid_fields;
                failure(&model, ErrorCode::ConstraintViolation);
            }
        }
    }

    #[test]
    fn enums_reject_duplicate_names_values_and_invalid_variant_identifiers() {
        for variants in [
            vec![variant("Same", 0), variant("Same", 1)],
            vec![variant("First", i32::MIN), variant("Second", i32::MIN)],
            vec![variant("", 0)], vec![variant("_", 0)], vec![variant("bad-name", 0)],
        ] {
            for site in TYPE_SITES {
                let ty = EndpointType::Enum { name: String::from("State"), variants: variants.clone() };
                failure(&model_with_type(site, ty), ErrorCode::ConstraintViolation);
            }
        }
        assert_eq!(validate_model(&model_with_type(TypeSite::Definition, EndpointType::Enum {
            name: String::from("Empty"), variants: Vec::new(),
        })), Ok(()));
    }

    #[test]
    fn unused_definition_role_error_and_nominal_names_are_validated() {
        for name in ["", "_", "has.dot", "9name", "雪"] {
            let definition_model = EndpointModel {
                definitions: vec![definition(name, EndpointType::Unit)], ..EndpointModel::default()
            };
            failure(&definition_model, ErrorCode::ConstraintViolation);
            let role_model = EndpointModel { roles: vec![role(name, &[])], ..EndpointModel::default() };
            failure(&role_model, ErrorCode::ConstraintViolation);
            let error_model = EndpointModel { errors: vec![error_spec(0, name)], ..EndpointModel::default() };
            failure(&error_model, ErrorCode::ConstraintViolation);
            for ty in [record(name, Vec::new()), enumeration(name), EndpointType::StructRef(String::from(name))] {
                failure(&model_with_type(TypeSite::Stream, ty), ErrorCode::ConstraintViolation);
            }
        }
    }

    #[test]
    fn global_error_names_and_codes_are_independently_unique() {
        let mut duplicate_name = model();
        duplicate_name.errors[1].name = duplicate_name.errors[0].name.clone();
        assert_eq!(failure(&duplicate_name, ErrorCode::ConstraintViolation).detail, "duplicate error name");
        let mut duplicate_code = model();
        duplicate_code.errors[1].code = 0;
        assert_eq!(failure(&duplicate_code, ErrorCode::ConstraintViolation).detail, "duplicate error code");
        let model = EndpointModel {
            errors: vec![error_spec(0, "Fault"), error_spec(u32::MAX, "fault")],
            ..EndpointModel::default()
        };
        assert_eq!(validate_model(&model), Ok(()));
    }

    #[test]
    fn public_endpoints_do_not_skip_validation_of_unused_roles() {
        for roles in [
            vec![role("Reader", &[]), role("Reader", &[])],
            vec![role("Reader", &["Missing"])],
            vec![role("Reader", &["Reader"])],
            vec![role("A", &["B"]), role("B", &["C"]), role("C", &["A"])],
            vec![role("Admin", &["Reader", "Reader"]), role("Reader", &[])],
        ] {
            let model = EndpointModel {
                services: vec![service(0, "Api", vec![endpoint(0, "Api", "public")])],
                roles, ..EndpointModel::default()
            };
            failure(&model, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn endpoint_roles_must_exist_and_be_unique() {
        for roles in [vec![String::from("Missing")], vec![String::from("Reader"), String::from("Reader")]] {
            let mut model = model();
            model.services[1].endpoints[0].roles = roles;
            failure(&model, ErrorCode::ConstraintViolation);
        }
        let mut case_mismatch = model();
        case_mismatch.services[0].endpoints[0].roles[0] = String::from("reader");
        failure(&case_mismatch, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn role_diamonds_and_chains_longer_than_type_depth_are_valid() {
        let mut model = EndpointModel {
            roles: vec![
                role("Admin", &["Left", "Right"]), role("Left", &["Reader"]),
                role("Right", &["Reader"]), role("Reader", &[]),
            ],
            ..EndpointModel::default()
        };
        assert_eq!(validate_model(&model), Ok(()));
        for index in 0..MAX_REFERENCE_DEPTH + 2 {
            let includes = if index == 0 { Vec::new() } else { vec![format!("Role{}", index - 1)] };
            model.roles.push(RoleSpec { name: format!("Role{index}"), includes, description: String::new() });
        }
        assert_eq!(validate_model(&model), Ok(()));
    }

    #[test]
    fn header_names_use_http_token_grammar_and_case_insensitive_uniqueness() {
        for name in ["", "X Header", "X:Header", "X\r\nInjected", "Héader", "X\0Header"] {
            failure(&EndpointModel {
                headers: vec![header(name, EndpointType::String)], ..EndpointModel::default()
            }, ErrorCode::ConstraintViolation);
        }
        let mut model = EndpointModel {
            headers: vec![header("X-Trace", EndpointType::String), header("x-trace", EndpointType::Unit)],
            ..EndpointModel::default()
        };
        failure(&model, ErrorCode::ConstraintViolation);
        model.headers[1].name = String::from("!#$%&'*+-.^_`|~");
        assert_eq!(validate_model(&model), Ok(()));
    }

    #[test]
    fn failures_are_deterministic_and_leave_the_complete_model_unchanged() {
        let mut model = model();
        model.services[1].endpoints[0].code = 0;
        model.roles.push(role("Broken", &["Missing"]));
        let original = model.clone();
        let first = failure(&model, ErrorCode::ConstraintViolation);
        assert_eq!(first.detail, "duplicate endpoint code");
        assert_eq!((first.schema_id, first.field_id), (0, 0));
        assert_eq!(validate_model(&model), Err(first));
        assert_eq!(model, original);
    }

    #[test]
    fn parsed_and_normalized_references_are_validated_without_rewriting() {
        let source = r#"Config(
            version: "1.2.3",
            services: [Service(id: 0, name: "Api", endpoints: [Endpoint(
                code: 0, method: "read", roles: ["Reader"],
                response: [Field(name: "node", ty: StructRef("Node"))],
                stream_response: Some(StructTable(struct_ref: "Node")),
            )])],
            definitions: [Definition(name: "Node", ty: Struct(name: "Node", fields: [
                Field(name: "next", ty: Optional(StructRef("Node"))),
            ]))],
            roles: ["Reader"],
            errors: [Error(code: 0, name: "Failure", message: "failed")],
            headers: [Header(name: "X-Id", ty: NanoId(len: 21))],
        )"#;
        let loaded = crate::load::parse_str(source, "<validation-test>", &crate::ConfigLimits::default()).unwrap();
        let model = crate::normalize::normalize(&loaded).unwrap();
        let original = model.clone();
        assert_eq!(validate_model(&model), Ok(()));
        assert_eq!(model, original);
        let loaded = crate::load::parse_str(
            r#"(version: "1.2.3", definitions: [(name: "Unused", ty: StructRef("Missing"))])"#,
            "<invalid-validation-test>", &crate::ConfigLimits::default(),
        ).unwrap();
        let model = crate::normalize::normalize(&loaded).unwrap();
        failure(&model, ErrorCode::ConstraintViolation);
    }
}
