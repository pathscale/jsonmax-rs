//! Loss-checked conversion between the supported legacy model shapes.
//!
//! The document variant selects the shape; `ModelVersion` is preserved model
//! metadata, not a legacy format selector. Parsing and rejection of unsupported
//! future document versions belong to the importing parser.

use super::registry::build_registry;
use super::{
    model_error, EndpointModel, ErrorCode, LegacyDocument, LegacyModelV1,
    LegacyVersion, Result, RoleSpec,
};
use alloc::{string::String, vec::Vec};

/// Import a legacy document, validating the complete resulting model.
///
/// V1 role names become roles with empty includes and descriptions, and its
/// absent errors and headers become empty lists. V2 already has the canonical
/// shape. All existing values and declaration order are preserved in either
/// case. Registry validation errors are returned without modification.
pub fn import_legacy(document: LegacyDocument) -> Result<EndpointModel> {
    let model = match document {
        LegacyDocument::V1(legacy) => EndpointModel {
            version: legacy.version,
            services: legacy.services,
            definitions: legacy.definitions,
            roles: legacy.roles.into_iter().map(|name| RoleSpec {
                name,
                includes: Vec::new(),
                description: String::new(),
            }).collect(),
            errors: Vec::new(),
            headers: Vec::new(),
        },
        LegacyDocument::V2(model) => model,
    };
    build_registry(&model)?;
    Ok(model)
}

/// Validate and export a model without discarding information.
///
/// V1 cannot represent errors, headers, role inheritance, or role descriptions.
/// Any such metadata, including a whitespace-only description, produces
/// `UnsupportedSchemaFeature`. Validation always precedes this compatibility
/// decision. V2 clones the entire model exactly. Neither path mutates the input.
pub fn export_legacy(model: &EndpointModel, version: LegacyVersion) -> Result<LegacyDocument> {
    build_registry(model)?;
    match version {
        LegacyVersion::V1 => {
            if !model.errors.is_empty() {
                return Err(model_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "legacy V1 cannot represent error declarations",
                ));
            }
            if !model.headers.is_empty() {
                return Err(model_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "legacy V1 cannot represent header declarations",
                ));
            }
            if model.roles.iter().any(|role| !role.includes.is_empty() || !role.description.is_empty()) {
                return Err(model_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "legacy V1 cannot represent role includes or descriptions",
                ));
            }
            Ok(LegacyDocument::V1(LegacyModelV1 {
                version: model.version,
                services: model.services.clone(),
                definitions: model.definitions.clone(),
                roles: model.roles.iter().map(|role| role.name.clone()).collect(),
            }))
        }
        LegacyVersion::V2 => Ok(LegacyDocument::V2(model.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, Error,
        ErrorSource, ErrorSpec, HeaderSpec, ModelVersion, ServiceSpec,
        TypeDefinition, MAX_REFERENCE_DEPTH,
    };
    use alloc::{boxed::Box, vec};

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField {
            name: String::from(name),
            ty,
            description: String::from("Field\r\n説明\t\0"),
        }
    }

    fn role(name: &str) -> RoleSpec {
        RoleSpec {
            name: String::from(name),
            includes: Vec::new(),
            description: String::new(),
        }
    }

    fn endpoint(service: &str, name: &str, code: u32) -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code,
            name: String::from(name),
            service: String::from(service),
            roles: Vec::new(),
            params: Vec::new(),
            response: Vec::new(),
            stream_response: None,
            description: String::from("Endpoint café\n\0"),
        }
    }

    // Orders deliberately disagree with lexical and numeric sorting. References
    // include forward aliases and nominal recursion; no expansion is permitted.
    fn model() -> EndpointModel {
        let mut subscribe = endpoint("HTTPStore_ID", "ZSubscribe", u32::MAX);
        subscribe.roles = vec![String::from("aWriter"), String::from("ZReader")];
        subscribe.params = vec![
            field("検索 語\0", EndpointType::StructRef(String::from("NodeAlias"))),
            field("a-field", EndpointType::EnumRef {
                name: String::from("StateAlias"), prefixed_name: true,
            }),
        ];
        subscribe.response = vec![
            field("z-last", EndpointType::Boolean),
            field("A-first", EndpointType::Optional(Box::new(EndpointType::Bytea))),
        ];
        subscribe.stream_response = Some(EndpointType::StructTable {
            struct_ref: String::from("NodeAlias"),
        });
        EndpointModel {
            version: ModelVersion::new(u32::MAX, 0, 42),
            services: vec![
                ServiceSpec {
                    id: u16::MAX,
                    name: String::from("HTTPStore_ID"),
                    endpoints: vec![subscribe, endpoint("HTTPStore_ID", "aPing", 0)],
                },
                ServiceSpec {
                    id: 0,
                    name: String::from("aService"),
                    endpoints: vec![endpoint("aService", "Read", 7)],
                },
                ServiceSpec { id: 17, name: String::from("Empty"), endpoints: Vec::new() },
            ],
            definitions: vec![
                TypeDefinition {
                    name: String::from("NodeAlias"),
                    ty: EndpointType::StructRef(String::from("Node")),
                },
                TypeDefinition {
                    name: String::from("StateAlias"),
                    ty: EndpointType::EnumRef {
                        name: String::from("State"), prefixed_name: false,
                    },
                },
                TypeDefinition {
                    name: String::from("Node"),
                    ty: EndpointType::Struct {
                        name: String::from("Node"),
                        fields: vec![
                            field("zNext", EndpointType::Optional(Box::new(
                                EndpointType::StructRef(String::from("Node")),
                            ))),
                            field("aValues", EndpointType::Vec(Box::new(EndpointType::UInt32))),
                        ],
                    },
                },
                TypeDefinition {
                    name: String::from("State"),
                    ty: EndpointType::Enum {
                        name: String::from("State"),
                        variants: vec![
                            EnumVariantSpec {
                                name: String::from("High"), value: i32::MAX,
                                description: String::from("最大\n"),
                            },
                            EnumVariantSpec {
                                name: String::from("Low"), value: i32::MIN,
                                description: String::from("\0minimum"),
                            },
                            EnumVariantSpec {
                                name: String::from("Zero"), value: 0,
                                description: String::new(),
                            },
                        ],
                    },
                },
            ],
            roles: vec![role("ZReader"), role("aWriter"), role("_Audit1")],
            errors: Vec::new(),
            headers: Vec::new(),
        }
    }

    // Construct the old shape independently of the conversion under test.
    fn v1_shape(model: &EndpointModel) -> LegacyModelV1 {
        LegacyModelV1 {
            version: model.version,
            services: model.services.clone(),
            definitions: model.definitions.clone(),
            roles: model.roles.iter().map(|role| role.name.clone()).collect(),
        }
    }

    fn full_model() -> EndpointModel {
        let mut value = model();
        value.roles[1].includes.push(String::from("ZReader"));
        value.roles[1].description = String::from("書き込み\n\0");
        value.roles[2].includes.push(String::from("aWriter"));
        value.roles[2].description = String::from(" \t ");
        value.errors = vec![
            ErrorSpec {
                code: u32::MAX, name: String::from("ZFailure"),
                message: String::from("失敗\r\n\0"),
            },
            ErrorSpec { code: 0, name: String::from("aFailure"), message: String::new() },
        ];
        value.headers = vec![
            HeaderSpec {
                name: String::from("X-Trace_ID"), ty: EndpointType::NanoId { len: u8::MAX },
                required: true, description: String::from("Trace\n\0"),
            },
            HeaderSpec {
                name: String::from("x-empty"), ty: EndpointType::String,
                required: false, description: String::new(),
            },
        ];
        value
    }

    fn assert_error_context(error: &Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_round_trips(value: &EndpointModel) {
        let before = value.clone();
        for version in [LegacyVersion::V1, LegacyVersion::V2] {
            let document = export_legacy(value, version).unwrap();
            match (&document, version) {
                (LegacyDocument::V1(actual), LegacyVersion::V1) => {
                    assert_eq!(actual, &v1_shape(value));
                }
                (LegacyDocument::V2(actual), LegacyVersion::V2) => assert_eq!(actual, value),
                _ => panic!("export selected the wrong document shape"),
            }
            assert_eq!(&import_legacy(document).unwrap(), value);
            assert_eq!(value, &before);
        }
    }

    // Invalid models used here have no V2-only metadata, so the same invalid
    // service/type/role data can be presented to both importing shapes.
    fn assert_rejected_everywhere(value: &EndpointModel, code: ErrorCode) {
        let before = value.clone();
        let expected = build_registry(value).unwrap_err();
        assert_error_context(&expected, code);
        assert_eq!(import_legacy(LegacyDocument::V1(v1_shape(value))).unwrap_err(), expected);
        assert_eq!(import_legacy(LegacyDocument::V2(value.clone())).unwrap_err(), expected);
        for version in [LegacyVersion::V1, LegacyVersion::V2] {
            assert_eq!(export_legacy(value, version).unwrap_err(), expected);
        }
        assert_eq!(value, &before);
    }

    #[test]
    fn empty_shapes_and_model_version_boundaries_are_preserved() {
        for version in [
            ModelVersion::new(0, 0, 0),
            ModelVersion::new(1, 2, 3),
            ModelVersion::new(2, 1, 0),
            ModelVersion::new(u32::MAX, u32::MAX, u32::MAX),
        ] {
            let value = EndpointModel { version, ..EndpointModel::default() };
            assert_round_trips(&value);
            assert_eq!(import_legacy(LegacyDocument::V1(v1_shape(&value))).unwrap(), value);
            assert_eq!(import_legacy(LegacyDocument::V2(value.clone())).unwrap(), value);
        }
    }

    #[test]
    fn v1_import_supplies_only_the_missing_metadata() {
        let expected = model();
        let document = v1_shape(&expected);
        assert_eq!(document.roles, ["ZReader", "aWriter", "_Audit1"]);
        let actual = import_legacy(LegacyDocument::V1(document.clone())).unwrap();
        assert_eq!(actual, expected);
        assert!(actual.errors.is_empty());
        assert!(actual.headers.is_empty());
        assert!(actual.roles.iter().all(|role| role.includes.is_empty() && role.description.is_empty()));
        assert_eq!(export_legacy(&actual, LegacyVersion::V1).unwrap(), LegacyDocument::V1(document));
    }

    #[test]
    fn v1_and_v2_preserve_order_codes_strings_and_recursive_types() {
        let value = model();
        assert_round_trips(&value);
        let imported = import_legacy(LegacyDocument::V1(v1_shape(&value))).unwrap();
        assert_eq!(imported.services.iter().map(|service| service.id).collect::<Vec<_>>(), [u16::MAX, 0, 17]);
        let endpoints = &imported.services[0].endpoints;
        assert_eq!(endpoints.iter().map(|endpoint| endpoint.code).collect::<Vec<_>>(), [u32::MAX, 0]);
        assert_eq!(endpoints[0].name, "ZSubscribe");
        assert_eq!(endpoints[0].service, "HTTPStore_ID");
        assert_eq!(endpoints[0].params[0].name, "検索 語\0");
        assert_eq!(endpoints[0].roles, ["aWriter", "ZReader"]);
        assert_eq!(endpoints[0].params[0].description, "Field\r\n説明\t\0");
        assert!(endpoints[1].stream_response.is_none());
        if let EndpointType::Enum { variants, .. } = &imported.definitions[3].ty {
            assert_eq!(variants.iter().map(|variant| variant.value).collect::<Vec<_>>(), [i32::MAX, i32::MIN, 0]);
            assert_eq!(variants[1].description, "\0minimum");
        } else {
            panic!("enum declaration was rewritten");
        }
    }

    #[test]
    fn v2_preserves_all_metadata_and_can_be_exported_repeatedly() {
        let value = full_model();
        let before = value.clone();
        let expected = LegacyDocument::V2(value.clone());
        assert_eq!(import_legacy(expected.clone()).unwrap(), value);
        assert_eq!(export_legacy(&value, LegacyVersion::V2).unwrap(), expected);
        assert_eq!(export_legacy(&value, LegacyVersion::V2).unwrap(), expected);
        assert_eq!(value, before);
    }

    #[test]
    fn exported_documents_own_independent_copies() {
        let mut value = model();
        let before = value.clone();
        let v1 = export_legacy(&value, LegacyVersion::V1).unwrap();
        let v2 = export_legacy(&value, LegacyVersion::V2).unwrap();
        value.version = ModelVersion::default();
        value.services[0].name.clear();
        value.services[0].endpoints[0].params[0].description.clear();
        value.definitions.clear();
        value.roles[0].name.clear();
        assert_eq!(import_legacy(v1).unwrap(), before);
        assert_eq!(import_legacy(v2).unwrap(), before);

        let mut value = full_model();
        let before = value.clone();
        let v2 = export_legacy(&value, LegacyVersion::V2).unwrap();
        value.errors[0].message.clear();
        value.headers[0].description.clear();
        value.roles[1].includes.clear();
        assert_eq!(import_legacy(v2).unwrap(), before);
    }

    fn type_vectors() -> Vec<EndpointType> {
        vec![
            EndpointType::UInt32, EndpointType::Int32, EndpointType::Int64,
            EndpointType::Float64, EndpointType::Boolean, EndpointType::String,
            EndpointType::Bytea, EndpointType::UUID,
            EndpointType::NanoId { len: 1 }, EndpointType::NanoId { len: 21 },
            EndpointType::NanoId { len: u8::MAX }, EndpointType::IpAddr,
            EndpointType::Struct {
                name: String::from("InlinePayload"),
                fields: vec![field("値", EndpointType::String)],
            },
            EndpointType::Struct { name: String::from("EmptyRecord"), fields: Vec::new() },
            EndpointType::StructRef(String::from("NodeAlias")),
            EndpointType::Object,
            EndpointType::StructTable { struct_ref: String::from("NodeAlias") },
            EndpointType::Vec(Box::new(EndpointType::Object)),
            EndpointType::Unit,
            EndpointType::Optional(Box::new(EndpointType::Unit)),
            EndpointType::Optional(Box::new(EndpointType::Optional(Box::new(EndpointType::Int64)))),
            EndpointType::Enum {
                name: String::from("InlineFlag"),
                variants: vec![EnumVariantSpec {
                    name: String::from("Negative"), value: -1, description: String::from("保留"),
                }],
            },
            EndpointType::Enum { name: String::from("EmptyEnum"), variants: Vec::new() },
            EndpointType::EnumRef { name: String::from("StateAlias"), prefixed_name: false },
            EndpointType::EnumRef { name: String::from("StateAlias"), prefixed_name: true },
            EndpointType::TimeStampMs, EndpointType::BlockchainDecimal,
            EndpointType::BlockchainAddress, EndpointType::BlockchainTransactionHash,
        ]
    }

    #[test]
    fn every_abi_type_survives_in_params_response_and_stream_positions() {
        for ty in type_vectors() {
            for position in 0..3 {
                let mut value = model();
                let endpoint = &mut value.services[0].endpoints[0];
                match position {
                    0 => endpoint.params = vec![field("自由", ty.clone())],
                    1 => endpoint.response = vec![field("自由", ty.clone())],
                    _ => endpoint.stream_response = Some(ty.clone()),
                }
                assert_round_trips(&value);
            }
        }
    }

    #[test]
    fn v1_rejects_each_unrepresentable_metadata_kind_without_mutation() {
        for case in 0..8 {
            let mut value = model();
            match case {
                0 => value.errors.push(ErrorSpec {
                    code: 0, name: String::from("Failure"), message: String::new(),
                }),
                1 => value.headers.push(HeaderSpec {
                    name: String::from("X-Optional"), ty: EndpointType::String,
                    required: false, description: String::new(),
                }),
                2 => value.roles[2].includes.push(String::from("ZReader")),
                3 => value.roles[2].description = String::from("description"),
                4 => value.roles[2].description = String::from(" \t\n"),
                5 => value.roles[2].description = String::from("\0"),
                6 => value = full_model(),
                _ => {
                    value.roles[2].includes.push(String::from("ZReader"));
                    value.roles[2].description = String::from("audit");
                }
            }
            let before = value.clone();
            let error = export_legacy(&value, LegacyVersion::V1).unwrap_err();
            assert_error_context(&error, ErrorCode::UnsupportedSchemaFeature);
            assert_eq!(value, before);
            let document = export_legacy(&value, LegacyVersion::V2).unwrap();
            assert_eq!(document, LegacyDocument::V2(before.clone()));
            assert_eq!(import_legacy(document).unwrap(), before);
        }
    }

    #[test]
    fn validation_errors_propagate_unchanged_through_every_conversion() {
        for case in 0..19 {
            let mut value = model();
            match case {
                0 => value.services[1].id = value.services[0].id,
                1 => value.services[1].endpoints[0].code = u32::MAX,
                2 => value.services[0].endpoints[0].service = String::from("wrong"),
                3 => value.definitions.push(value.definitions[0].clone()),
                4 => value.roles.push(value.roles[0].clone()),
                5 => value.roles[0].name = String::from("_"),
                6 => value.services[0].endpoints[0].roles.push(String::from("Unknown")),
                7 => value.services[0].endpoints[0].roles.push(String::from("ZReader")),
                8 => value.services[0].endpoints[0].params[0].name.clear(),
                9 => value.services[0].endpoints[0].params[1].name = String::from("検索 語\0"),
                10 => value.services[0].endpoints[0].stream_response = Some(EndpointType::NanoId { len: 0 }),
                11 => value.services[0].endpoints[0].stream_response = Some(
                    EndpointType::StructRef(String::from("Missing")),
                ),
                12 => value.services[0].endpoints[0].stream_response = Some(
                    EndpointType::StructRef(String::from("State")),
                ),
                13 => value.services[0].endpoints[0].response[0].ty = EndpointType::Vec(Box::new(
                    EndpointType::Optional(Box::new(EndpointType::EnumRef {
                        name: String::from("Missing"), prefixed_name: false,
                    })),
                )),
                14 => value.definitions[0].ty = EndpointType::StructRef(String::from("NodeAlias")),
                15 => value.definitions[2].name = String::from("WrongNominal"),
                16 => value.services[0].name = String::from("invalid.service"),
                17 => {
                    if let EndpointType::Enum { variants, .. } = &mut value.definitions[3].ty {
                        variants[1].value = variants[0].value;
                    } else {
                        panic!("fixture must contain State enum");
                    }
                }
                _ => value.services[0].endpoints[0].stream_response = Some(EndpointType::Struct {
                    name: String::from("Node"), fields: Vec::new(),
                }),
            }
            assert_rejected_everywhere(&value, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn v2_metadata_is_validated_and_invalid_models_take_precedence_over_v1_loss_checks() {
        for case in 0..6 {
            let mut value = full_model();
            match case {
                0 => value.roles[2].includes.push(String::from("Unknown")),
                1 => value.roles[0].includes.push(String::from("_Audit1")),
                2 => value.errors[1].code = value.errors[0].code,
                3 => value.errors[1].name = value.errors[0].name.clone(),
                4 => value.headers[1].name = String::from("x-trace_id"),
                _ => value.headers[1].ty = EndpointType::StructRef(String::from("Missing")),
            }
            let before = value.clone();
            let expected = build_registry(&value).unwrap_err();
            assert_error_context(&expected, ErrorCode::ConstraintViolation);
            assert_eq!(import_legacy(LegacyDocument::V2(value.clone())).unwrap_err(), expected);
            assert_eq!(export_legacy(&value, LegacyVersion::V1).unwrap_err(), expected);
            assert_eq!(export_legacy(&value, LegacyVersion::V2).unwrap_err(), expected);
            assert_eq!(value, before);
        }
    }

    fn nested_type(depth: usize) -> EndpointType {
        let mut ty = EndpointType::String;
        for index in 0..depth {
            ty = if index % 2 == 0 {
                EndpointType::Vec(Box::new(ty))
            } else {
                EndpointType::Optional(Box::new(ty))
            };
        }
        ty
    }

    #[test]
    fn exact_depth_limit_round_trips_and_excess_depth_is_propagated() {
        let mut value = EndpointModel {
            definitions: vec![TypeDefinition {
                name: String::from("Deep"), ty: nested_type(MAX_REFERENCE_DEPTH),
            }],
            ..EndpointModel::default()
        };
        assert_round_trips(&value);
        value.definitions[0].ty = nested_type(MAX_REFERENCE_DEPTH + 1);
        assert_rejected_everywhere(&value, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn role_names_remain_case_sensitive_and_unreferenced_roles_are_retained() {
        let mut value = model();
        value.roles.push(role("zreader"));
        value.roles.push(role("Unused"));
        assert_round_trips(&value);
        let imported = import_legacy(LegacyDocument::V1(v1_shape(&value))).unwrap();
        assert_eq!(
            imported.roles.iter().map(|role| role.name.as_str()).collect::<Vec<_>>(),
            ["ZReader", "aWriter", "_Audit1", "zreader", "Unused"],
        );
    }
}
