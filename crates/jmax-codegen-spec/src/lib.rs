//! Deterministic, runtime-independent projections of endpoint specifications.
#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::{SchemaFieldIr, SchemaIr};
pub use jmax_abi::traits::{ByteSink, JsonEncode};

pub mod services;
pub mod json_schema;
pub mod protocol;
pub mod markdown;

pub const CONTRACT_VERSION: u16 = 1;
pub const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
pub const OPENAPI_VERSION: &str = "3.1.0";
pub const ASYNCAPI_VERSION: &str = "3.0.0";

/// Empty endpoint role lists denote public access. Role matching is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility<'a> {
    All,
    Public,
    Roles(&'a [String]),
}

impl Visibility<'_> {
    pub fn includes(self, endpoint: &EndpointSchemaSpec) -> bool {
        match self {
            Self::All => true,
            Self::Public => endpoint.roles.is_empty(),
            Self::Roles(roles) => endpoint.roles.is_empty()
                || endpoint.roles.iter().any(|required| roles.iter().any(|role| role == required)),
        }
    }
}

/// Definitions used to resolve named endpoint types. References can be recursive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeRegistry {
    pub structs: Vec<EndpointType>,
    pub enums: Vec<EndpointType>,
}

impl TypeRegistry {
    pub fn struct_type(&self, name: &str) -> Option<&EndpointType> {
        self.structs.iter().find(|ty| matches!(ty, EndpointType::Struct { name: candidate, .. } if candidate == name))
    }

    pub fn enum_type(&self, name: &str) -> Option<&EndpointType> {
        self.enums.iter().find(|ty| matches!(ty, EndpointType::Enum { name: candidate, .. } if candidate == name))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentOptions {
    pub title: String,
    pub version: String,
    pub description: String,
    pub server_url: Option<String>,
    pub max_output_bytes: u32,
    pub max_type_depth: u16,
}

impl Default for DocumentOptions {
    fn default() -> Self {
        Self {
            title: String::from("Endpoint API"),
            version: String::from("1.0.0"),
            description: String::new(),
            server_url: None,
            max_output_bytes: 16 * 1024 * 1024,
            max_type_depth: 128,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedDocument {
    pub media_type: &'static str,
    pub text: String,
}

impl GeneratedDocument {
    /// Checks byte length, rather than Unicode scalar count.
    pub fn new(media_type: &'static str, text: String, max_bytes: u32) -> Result<Self> {
        if text.len() > max_bytes as usize {
            return Err(spec_error(ErrorCode::PayloadTooLarge, "generated document exceeds output limit"));
        }
        Ok(Self { media_type, text })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub endpoint_code: u32,
    /// A complete JSON Schema object, including any referenced definitions.
    pub input_schema: GeneratedDocument,
    pub output_schema: GeneratedDocument,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaDirection {
    Params,
    Response,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorDoc {
    pub code: i32,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdOptions {
    pub description: String,
    pub executable: String,
    pub arguments: Vec<String>,
    pub user: Option<String>,
    pub working_directory: Option<String>,
    pub restart_seconds: u32,
}

/// Stable and injective for names accepted by `valid_identifier`.
pub fn tool_name(service: &str, endpoint: &str) -> Result<String> {
    if !valid_identifier(service) || !valid_identifier(endpoint) {
        return Err(spec_error(ErrorCode::ConstraintViolation, "invalid service or endpoint identifier"));
    }
    let capacity = service.len().checked_add(endpoint.len()).and_then(|len| len.checked_add(1))
        .ok_or_else(|| spec_error(ErrorCode::BudgetExhausted, "tool name length overflow"))?;
    let mut name = String::new();
    name.try_reserve(capacity)
        .map_err(|_| spec_error(ErrorCode::BudgetExhausted, "tool name allocation failed"))?;
    name.push_str(service);
    name.push('.');
    name.push_str(endpoint);
    Ok(name)
}

pub fn valid_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Validates catalog identity before filtering, including hidden endpoints.
/// Type-reference resolution and type constraints are checked by projections.
pub fn validate_services(services: &[ServiceSpec]) -> Result<()> {
    for (index, service) in services.iter().enumerate() {
        if !valid_identifier(&service.name) {
            return Err(spec_error(ErrorCode::ConstraintViolation, "invalid service identifier"));
        }
        if services[..index].iter().any(|other| other.id == service.id || other.name == service.name) {
            return Err(spec_error(ErrorCode::DuplicateObjectKey, "duplicate service identity"));
        }
        for (endpoint_index, endpoint) in service.endpoints.iter().enumerate() {
            if endpoint.service != service.name || !valid_identifier(&endpoint.name) {
                return Err(spec_error(ErrorCode::ConstraintViolation, "endpoint service or name mismatch"));
            }
            if service.endpoints[..endpoint_index].iter().any(|other| other.name == endpoint.name) {
                return Err(spec_error(ErrorCode::DuplicateObjectKey, "duplicate endpoint name"));
            }
            if services[..index].iter().flat_map(|other| &other.endpoints)
                .chain(service.endpoints[..endpoint_index].iter())
                .any(|other| other.code == endpoint.code)
            {
                return Err(spec_error(ErrorCode::DuplicateObjectKey, "duplicate endpoint code"));
            }
            for (role_index, role) in endpoint.roles.iter().enumerate() {
                if role.is_empty() || endpoint.roles[..role_index].contains(role) {
                    return Err(spec_error(ErrorCode::ConstraintViolation, "empty or duplicate endpoint role"));
                }
            }
            for fields in [&endpoint.params, &endpoint.response] {
                for (field_index, field) in fields.iter().enumerate() {
                    if field.name.is_empty() || fields[..field_index].iter().any(|other| other.name == field.name) {
                        return Err(spec_error(ErrorCode::DuplicateObjectKey, "empty or duplicate field name"));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) const fn spec_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}
