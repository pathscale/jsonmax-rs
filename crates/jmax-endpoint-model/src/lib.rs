//! Canonical endpoint models and the shared contracts for their adapters.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub use jmax_abi::endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::{FieldId, SchemaFieldIr, SchemaId, SchemaIr};
pub use jmax_abi::syntax::ScalarKind;
use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::fmt;

pub mod registry;
pub mod legacy;
pub mod wire_names;
pub mod utilities;

pub const CONTRACT_VERSION: u16 = 1;
pub const MAX_REFERENCE_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ModelVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self { Self { major, minor, patch } }
}

impl fmt::Display for ModelVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeDefinition {
    pub name: String,
    pub ty: EndpointType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleSpec {
    pub name: String,
    /// Directly included roles. Registry construction rejects cycles.
    pub includes: Vec<String>,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorSpec {
    pub code: u32,
    pub name: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderSpec {
    pub name: String,
    pub ty: EndpointType,
    pub required: bool,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointModel {
    pub version: ModelVersion,
    pub services: Vec<ServiceSpec>,
    pub definitions: Vec<TypeDefinition>,
    pub roles: Vec<RoleSpec>,
    pub errors: Vec<ErrorSpec>,
    pub headers: Vec<HeaderSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageShape { Params, Response }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointLocation {
    pub service_index: usize,
    pub endpoint_index: usize,
}

/// An immutable borrowed index. Construction and all validation belong to registry.
#[derive(Debug)]
pub struct Registry<'a> {
    pub(crate) model: &'a EndpointModel,
    pub(crate) services_by_id: BTreeMap<u16, usize>,
    pub(crate) services_by_name: BTreeMap<&'a str, usize>,
    pub(crate) endpoints_by_code: BTreeMap<u32, EndpointLocation>,
    pub(crate) endpoints_by_method: BTreeMap<String, EndpointLocation>,
    pub(crate) definitions_by_name: BTreeMap<&'a str, usize>,
    pub(crate) roles_by_name: BTreeMap<&'a str, usize>,
    pub(crate) errors_by_code: BTreeMap<u32, usize>,
}

impl<'a> Registry<'a> {
    pub fn model(&self) -> &'a EndpointModel { self.model }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyVersion { V1, V2 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyModelV1 {
    pub version: ModelVersion,
    pub services: Vec<ServiceSpec>,
    pub definitions: Vec<TypeDefinition>,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegacyDocument { V1(LegacyModelV1), V2(EndpointModel) }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NameCase {
    #[default]
    Preserve,
    Snake,
    Camel,
    Pascal,
    ScreamingSnake,
    Kebab,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WireNamePolicy {
    pub case: NameCase,
    /// When set, collisions are compared after ASCII lowercasing.
    pub case_insensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireName {
    pub source: String,
    pub wire: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgType { Integer, BigInt, DoublePrecision, Boolean, Text, Bytea, Uuid, Inet, Jsonb, Numeric, TimestampMs }

impl PgType {
    pub const fn sql_name(self) -> &'static str {
        match self {
            Self::Integer => "integer", Self::BigInt => "bigint", Self::DoublePrecision => "double precision",
            Self::Boolean => "boolean", Self::Text => "text", Self::Bytea => "bytea", Self::Uuid => "uuid",
            Self::Inet => "inet", Self::Jsonb => "jsonb", Self::Numeric => "numeric", Self::TimestampMs => "bigint",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolHeader {
    pub name: String,
    pub value: String,
}

pub(crate) fn model_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, 0, detail)
}

/// Bounds checked accessor shared by every index query.
pub(crate) fn endpoint_at(model: &EndpointModel, location: EndpointLocation) -> Option<&EndpointSchemaSpec> {
    model.services.get(location.service_index)?.endpoints.get(location.endpoint_index)
}
