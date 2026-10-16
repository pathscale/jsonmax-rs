//! Bounded RON configuration loading, normalization and version requirements.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_endpoint_model::{EndpointField, EndpointModel, EndpointSchemaSpec, EndpointType,
    EnumVariantSpec, ErrorSpec, HeaderSpec, ModelVersion, RoleSpec, ServiceSpec, TypeDefinition};
use alloc::{boxed::Box, string::String, vec::Vec};

pub mod load;
pub mod native;
pub mod normalize;
pub mod validate;
pub use native::load_native_spec;
pub mod version;

pub const CONTRACT_VERSION: u16 = 1;
pub const CONFIG_FORMAT_VERSION: u32 = 1;
pub const RUNTIME_VERSION: ModelVersion = ModelVersion::new(0, 1, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigLimits {
    pub max_input_bytes: u32,
    pub max_depth: u16,
    pub max_nodes: u32,
    pub max_string_bytes: u32,
    pub max_collection_items: u32,
}

impl Default for ConfigLimits {
    fn default() -> Self {
        Self { max_input_bytes: 16 * 1024 * 1024, max_depth: 128, max_nodes: 1_000_000,
            max_string_bytes: 1024 * 1024, max_collection_items: 100_000 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadOptions {
    pub limits: ConfigLimits,
    pub require_version: bool,
}

impl Default for LoadOptions {
    fn default() -> Self { Self { limits: ConfigLimits::default(), require_version: true } }
}

/// Located syntax tree. Every span refers to UTF-8 byte offsets in LoadedConfig's original input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigNode {
    pub offset: u32,
    pub len: u32,
    pub value: ConfigValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigValue {
    Unit,
    Bool(bool),
    U64(u64),
    I64(i64),
    String(String),
    Identifier(String),
    Sequence(Vec<ConfigNode>),
    Tuple(Vec<ConfigNode>),
    Record(Vec<(String, ConfigNode)>),
    Variant { name: String, value: Box<ConfigNode> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedConfig {
    pub source_name: String,
    pub root: ConfigNode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigDefaults {
    pub service: Option<String>,
    pub roles: Vec<String>,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionRequirements {
    pub format_version: u32,
    pub model_version: ModelVersion,
    pub minimum_runtime: ModelVersion,
}

/// Synchronous, allocation-only pipeline. Filesystem access is confined to load's std APIs.
/// No model escapes until normalization, semantic validation and version validation succeed.
pub fn load_model(source: &str, version_toml: Option<&str>, options: &LoadOptions) -> Result<EndpointModel> {
    let requirements = match version_toml {
        Some(text) => Some(version::parse_version(text)?),
        None if options.require_version => return Err(config_error(ErrorCode::MissingRequiredField, 0, "version.toml is required")),
        None => None,
    };
    let loaded = load::parse_str(source, "<memory>", &options.limits)?;
    let model = normalize::normalize(&loaded)?;
    validate::validate_model(&model)?;
    if let Some(requirements) = requirements {
        version::validate_version(&model, &requirements)?;
    }
    Ok(model)
}

pub(crate) fn config_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, offset, detail)
}

/// Validate resource ceilings before allocating syntax nodes or narrowing offsets.
pub fn validate_input_size(input: &str, limits: &ConfigLimits) -> Result<()> {
    let len = u32::try_from(input.len()).map_err(|_| config_error(ErrorCode::PayloadTooLarge, 0, "configuration exceeds u32 address space"))?;
    if len > limits.max_input_bytes {
        return Err(config_error(ErrorCode::PayloadTooLarge, 0, "configuration exceeds input byte limit"));
    }
    if limits.max_depth == 0 || limits.max_nodes == 0 {
        return Err(config_error(ErrorCode::ConstraintViolation, 0, "configuration limits must permit a root node"));
    }
    Ok(())
}
