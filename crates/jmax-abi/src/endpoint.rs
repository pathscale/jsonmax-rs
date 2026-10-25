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
