//! Shared endpoint and service descriptions for schema and code generation.

use alloc::{boxed::Box, string::String, vec::Vec};

/// A language-independent endpoint field type.
///
/// Named references are resolved by the endpoint model or schema compiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointType {
    UInt32,
    Int32,
    Int64,
    Float64,
    Boolean,
    String,
    Bytea,
    UUID,
    NanoId { len: u8 },
    IpAddr,
    Struct { name: String, fields: Vec<EndpointField> },
    StructRef(String),
    Object,
    StructTable { struct_ref: String },
    Vec(Box<EndpointType>),
    Unit,
    Optional(Box<EndpointType>),
    Enum { name: String, variants: Vec<EnumVariantSpec> },
    EnumRef { name: String, prefixed_name: bool },
    TimeStampMs,
    BlockchainDecimal,
    BlockchainAddress,
    BlockchainTransactionHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariantSpec {
    pub name: String,
    pub value: i32,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointField {
    pub name: String,
    pub ty: EndpointType,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointSchemaSpec {
    pub code: u32,
    pub name: String,
    pub service: String,
    pub roles: Vec<String>,
    pub params: Vec<EndpointField>,
    pub response: Vec<EndpointField>,
    pub stream_response: Option<EndpointType>,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    pub id: u16,
    pub name: String,
    pub endpoints: Vec<EndpointSchemaSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField {
            name: String::from(name), ty, description: String::from("field description"),
        }
    }

    #[test]
    fn all_endpoint_variants_are_distinct_and_cloneable() {
        let variants = alloc::vec![
            EndpointType::UInt32, EndpointType::Int32, EndpointType::Int64,
            EndpointType::Float64, EndpointType::Boolean, EndpointType::String,
            EndpointType::Bytea, EndpointType::UUID, EndpointType::NanoId { len: 21 },
            EndpointType::IpAddr,
            EndpointType::Struct {
                name: String::from("Record"), fields: alloc::vec![field("id", EndpointType::UInt32)],
            },
            EndpointType::StructRef(String::from("Record")), EndpointType::Object,
            EndpointType::StructTable { struct_ref: String::from("Record") },
            EndpointType::Vec(Box::new(EndpointType::Int64)), EndpointType::Unit,
            EndpointType::Optional(Box::new(EndpointType::Int64)),
            EndpointType::Enum {
                name: String::from("State"), variants: alloc::vec![EnumVariantSpec {
                    name: String::from("Ready"), value: 1, description: String::from("ready"),
                }],
            },
            EndpointType::EnumRef { name: String::from("State"), prefixed_name: true },
            EndpointType::TimeStampMs, EndpointType::BlockchainDecimal,
            EndpointType::BlockchainAddress, EndpointType::BlockchainTransactionHash,
        ];
        for (index, variant) in variants.iter().enumerate() {
            assert_eq!(*variant, variant.clone());
            for other in &variants[index + 1..] {
                assert_ne!(variant, other);
            }
        }
    }

    #[test]
    fn variant_parameters_participate_in_equality() {
        assert_ne!(EndpointType::NanoId { len: 0 }, EndpointType::NanoId { len: u8::MAX });
        assert_ne!(
            EndpointType::EnumRef { name: String::from("E"), prefixed_name: false },
            EndpointType::EnumRef { name: String::from("E"), prefixed_name: true },
        );
        assert_ne!(
            EndpointType::StructRef(String::from("A")),
            EndpointType::StructRef(String::from("B")),
        );
        let mut variant = EnumVariantSpec {
            name: String::from("Limit"), value: i32::MIN, description: String::from("minimum"),
        };
        let original = variant.clone();
        variant.value = i32::MAX;
        assert_ne!(variant, original);
        variant.value = original.value;
        variant.description.clear();
        assert_ne!(variant, original);
    }

    #[test]
    fn nested_types_clone_their_entire_owned_structure() {
        let mut ty = EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(
            EndpointType::Struct {
                name: String::from("Item"), fields: alloc::vec![field("name", EndpointType::String)],
            },
        ))));
        let cloned = ty.clone();
        if let EndpointType::Optional(optional) = &mut ty {
            if let EndpointType::Vec(element) = optional.as_mut() {
                if let EndpointType::Struct { name, fields } = element.as_mut() {
                    name.clear();
                    fields[0].name.clear();
                } else {
                    panic!("expected struct element");
                }
            } else {
                panic!("expected vector");
            }
        } else {
            panic!("expected optional");
        }
        assert_ne!(ty, cloned);
        assert_eq!(cloned, EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(
            EndpointType::Struct {
                name: String::from("Item"), fields: alloc::vec![field("name", EndpointType::String)],
            },
        )))));
    }

    #[test]
    fn service_clone_preserves_endpoint_metadata_and_responses() {
        let endpoint = EndpointSchemaSpec {
            code: u32::MAX, name: String::from("subscribe"), service: String::from("market"),
            roles: alloc::vec![String::from("reader")],
            params: alloc::vec![field("symbol", EndpointType::String)],
            response: alloc::vec![field("accepted", EndpointType::Boolean)],
            stream_response: Some(EndpointType::BlockchainDecimal),
            description: String::from("Subscribe to market updates."),
        };
        let mut service = ServiceSpec {
            id: u16::MAX, name: String::from("market"), endpoints: alloc::vec![endpoint.clone()],
        };
        let cloned = service.clone();
        service.name.clear();
        service.endpoints[0].roles[0].clear();
        service.endpoints[0].params[0].description.clear();
        service.endpoints[0].response.clear();
        service.endpoints[0].stream_response = None;
        assert_eq!(cloned.id, u16::MAX);
        assert_eq!(cloned.name, "market");
        assert_eq!(cloned.endpoints, [endpoint]);
        assert_ne!(service, cloned);
    }

    #[test]
    fn empty_service_and_unary_endpoint_are_representable() {
        let mut service = ServiceSpec {
            id: 0, name: String::new(), endpoints: Vec::new(),
        };
        assert!(service.endpoints.is_empty());
        service.endpoints.push(EndpointSchemaSpec {
            code: 0, name: String::from("ping"), service: String::new(),
            roles: Vec::new(), params: Vec::new(), response: Vec::new(),
            stream_response: None, description: String::new(),
        });
        assert_eq!(service, service.clone());
        assert_eq!(service.endpoints[0].stream_response, None);
    }
}
