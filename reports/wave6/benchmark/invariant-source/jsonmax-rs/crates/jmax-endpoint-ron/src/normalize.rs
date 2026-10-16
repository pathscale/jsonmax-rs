//! Deterministic conversion from located configuration syntax to canonical models.
//!
//! Record wrappers and keys are checked before reading their values. Defaults
//! apply only to absent fields. References remain unresolved for model validation.
//! Type roots have depth zero, matching endpoint-model's ownership depth limit.

use alloc::{boxed::Box, string::String, vec::Vec};
use jmax_endpoint_model::MAX_REFERENCE_DEPTH;

use crate::{
    config_error, ConfigDefaults, ConfigNode, ConfigValue, EndpointField, EndpointModel,
    EndpointSchemaSpec, EndpointType, EnumVariantSpec, Error, ErrorCode, ErrorSpec,
    HeaderSpec, LoadedConfig, ModelVersion, Result, RoleSpec, ServiceSpec, TypeDefinition,
};

/// Normalize a complete configuration without resolving names or assigning ids.
pub fn normalize(config: &LoadedConfig) -> Result<EndpointModel> {
    let record = record(&config.root, &["Config", "EndpointModel"], &[
        "version", "services", "definitions", "roles", "errors", "headers", "defaults",
    ])?;
    let version = normalize_version(record.required("version")?)?;
    let defaults = record.get("defaults").map(normalize_defaults).transpose()?.unwrap_or_default();
    Ok(EndpointModel {
        version,
        services: list(record.get("services"), |node| service(node, &defaults))?,
        definitions: list(record.get("definitions"), definition)?,
        roles: list(record.get("roles"), role)?,
        errors: list(record.get("errors"), error_spec)?,
        headers: list(record.get("headers"), header)?,
    })
}

/// Normalize exactly one supported type expression, with bounded ownership depth.
pub fn normalize_type(node: &ConfigNode) -> Result<EndpointType> {
    type_at(node, 0)
}

/// Normalize defaults without inventing service ids, endpoint codes or roles.
pub fn normalize_defaults(node: &ConfigNode) -> Result<ConfigDefaults> {
    let record = record(node, &["Defaults"], &["service", "roles", "description"])?;
    Ok(ConfigDefaults {
        service: record.get("service").map(option_string).transpose()?.flatten(),
        roles: strings(record.get("roles"))?,
        description: description(&record)?,
    })
}

struct Record<'a> {
    node: &'a ConfigNode,
    fields: &'a [(String, ConfigNode)],
}

impl<'a> Record<'a> {
    fn get(&self, key: &str) -> Option<&'a ConfigNode> {
        self.fields.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    fn required(&self, key: &str) -> Result<&'a ConfigNode> {
        self.get(key).ok_or_else(|| at(self.node, ErrorCode::MissingRequiredField,
            "required configuration field is missing"))
    }
}

fn record<'a>(node: &'a ConfigNode, wrappers: &[&str], keys: &[&str]) -> Result<Record<'a>> {
    let payload = match &node.value {
        ConfigValue::Variant { name, value } => {
            if !wrappers.contains(&name.as_str()) {
                return Err(at(node, ErrorCode::TypeMismatch, "unexpected configuration record wrapper"));
            }
            value.as_ref()
        }
        _ => node,
    };
    let ConfigValue::Record(fields) = &payload.value else {
        return Err(at(payload, ErrorCode::TypeMismatch, "expected a configuration record"));
    };
    // Valid records have a small, fixed key vocabulary. Reject unknown keys
    // immediately, so this duplicate scan is bounded by that vocabulary even
    // for syntax trees constructed without the parser.
    for (index, (key, value)) in fields.iter().enumerate() {
        if !keys.contains(&key.as_str()) {
            return Err(at(value, ErrorCode::UnknownObjectKey, "unknown configuration record key"));
        }
        if fields[..index].iter().any(|(previous, _)| previous == key) {
            return Err(at(value, ErrorCode::DuplicateObjectKey, "duplicate configuration record key"));
        }
    }
    Ok(Record { node: payload, fields })
}

fn at(node: &ConfigNode, code: ErrorCode, detail: &'static str) -> Error {
    config_error(code, node.offset, detail)
}

fn copy_string(value: &str, node: &ConfigNode) -> Result<String> {
    let mut output = String::new();
    output.try_reserve(value.len()).map_err(|_| at(node, ErrorCode::BudgetExhausted,
        "cannot allocate normalized string"))?;
    output.push_str(value);
    Ok(output)
}

fn text(node: &ConfigNode) -> Result<String> {
    match &node.value {
        ConfigValue::String(value) => copy_string(value, node),
        _ => Err(at(node, ErrorCode::TypeMismatch, "expected a quoted string")),
    }
}

fn name(node: &ConfigNode) -> Result<String> {
    let value = text(node)?;
    if value.is_empty() {
        return Err(at(node, ErrorCode::ConstraintViolation, "required name must not be empty"));
    }
    Ok(value)
}

fn boolean(node: &ConfigNode) -> Result<bool> {
    match &node.value {
        ConfigValue::Bool(value) => Ok(*value),
        _ => Err(at(node, ErrorCode::TypeMismatch, "expected a boolean")),
    }
}

fn unsigned<T: TryFrom<u64>>(node: &ConfigNode) -> Result<T> {
    let overflow = || at(node, ErrorCode::NumberOverflow, "integer exceeds unsigned field width");
    let value = match &node.value {
        ConfigValue::U64(value) => *value,
        ConfigValue::I64(value) => u64::try_from(*value).map_err(|_| overflow())?,
        _ => return Err(at(node, ErrorCode::TypeMismatch, "expected an integer")),
    };
    T::try_from(value).map_err(|_| overflow())
}

fn signed_i32(node: &ConfigNode) -> Result<i32> {
    let overflow = || at(node, ErrorCode::NumberOverflow, "integer exceeds signed 32-bit field width");
    match &node.value {
        ConfigValue::U64(value) => i32::try_from(*value).map_err(|_| overflow()),
        ConfigValue::I64(value) => i32::try_from(*value).map_err(|_| overflow()),
        _ => Err(at(node, ErrorCode::TypeMismatch, "expected an integer")),
    }
}

fn list<T>(node: Option<&ConfigNode>, mut convert: impl FnMut(&ConfigNode) -> Result<T>) -> Result<Vec<T>> {
    let Some(node) = node else { return Ok(Vec::new()); };
    let ConfigValue::Sequence(values) = &node.value else {
        return Err(at(node, ErrorCode::TypeMismatch, "expected a configuration sequence"));
    };
    let mut output = Vec::new();
    output.try_reserve(values.len()).map_err(|_| at(node, ErrorCode::BudgetExhausted,
        "cannot allocate normalized sequence"))?;
    for value in values {
        output.push(convert(value)?);
    }
    Ok(output)
}

fn strings(node: Option<&ConfigNode>) -> Result<Vec<String>> {
    list(node, name)
}

fn copy_strings(values: &[String], node: &ConfigNode) -> Result<Vec<String>> {
    let mut output = Vec::new();
    output.try_reserve(values.len()).map_err(|_| at(node, ErrorCode::BudgetExhausted,
        "cannot allocate default roles"))?;
    for value in values {
        output.push(copy_string(value, node)?);
    }
    Ok(output)
}

fn description(record: &Record<'_>) -> Result<String> {
    record.get("description").map(text).transpose().map(Option::unwrap_or_default)
}

fn one_argument(node: &ConfigNode, option: bool) -> Result<&ConfigNode> {
    match &node.value {
        ConfigValue::Tuple(values) if values.len() == 1 => Ok(&values[0]),
        _ => Err(at(node, if option { ErrorCode::ConstraintViolation } else { ErrorCode::TypeMismatch },
            "wrapper requires exactly one tuple argument")),
    }
}

/// Only a bare None denotes absence. None(...) and bare Some are malformed.
fn option_string(node: &ConfigNode) -> Result<Option<String>> {
    match &node.value {
        ConfigValue::Identifier(tag) if tag == "None" => Ok(None),
        ConfigValue::Variant { name, value } if name == "Some" =>
            text(one_argument(value, true)?).map(Some),
        ConfigValue::Identifier(tag) if tag == "Some" =>
            Err(at(node, ErrorCode::ConstraintViolation, "Some requires a tuple payload")),
        ConfigValue::Variant { name, .. } if name == "None" =>
            Err(at(node, ErrorCode::ConstraintViolation, "None must not have a payload")),
        _ => text(node).map(Some),
    }
}

fn normalize_version(node: &ConfigNode) -> Result<ModelVersion> {
    if let ConfigValue::String(value) = &node.value {
        return crate::version::parse_semver(value).map_err(|mut error| {
            error.byte_offset = node.offset;
            error
        });
    }
    let record = record(node, &["Version"], &["major", "minor", "patch"])?;
    Ok(ModelVersion::new(
        unsigned(record.required("major")?)?,
        unsigned(record.required("minor")?)?,
        unsigned(record.required("patch")?)?,
    ))
}

fn service(node: &ConfigNode, defaults: &ConfigDefaults) -> Result<ServiceSpec> {
    let record = record(node, &["Service"], &["id", "name", "endpoints"])?;
    let id = unsigned(record.required("id")?)?;
    let service_name = match record.get("name") {
        Some(node) => name(node)?,
        None => match defaults.service.as_deref() {
            Some("") => return Err(at(record.node, ErrorCode::ConstraintViolation,
                "default service name must not be empty")),
            Some(value) => copy_string(value, record.node)?,
            None => return Err(at(record.node, ErrorCode::MissingRequiredField,
                "service name is required without a default")),
        },
    };
    let endpoints = list(record.get("endpoints"), |node| endpoint(node, &service_name, defaults))?;
    Ok(ServiceSpec { id, name: service_name, endpoints })
}

fn endpoint(node: &ConfigNode, service_name: &str, defaults: &ConfigDefaults) -> Result<EndpointSchemaSpec> {
    let record = record(node, &["Endpoint"], &[
        "code", "name", "method", "service", "roles", "params", "response", "stream_response", "description",
    ])?;
    let code = unsigned(record.required("code")?)?;
    let name_node = match (record.get("name"), record.get("method")) {
        (Some(_), Some(method)) => return Err(at(method, ErrorCode::ConstraintViolation,
            "endpoint name and method aliases are mutually exclusive")),
        (Some(name), None) | (None, Some(name)) => name,
        (None, None) => return Err(at(record.node, ErrorCode::MissingRequiredField,
            "endpoint requires a name or method")),
    };
    let endpoint_name = name(name_node)?;
    if endpoint_name.contains('.') {
        return Err(at(name_node, ErrorCode::ConstraintViolation,
            "endpoint name must not be a fully qualified method"));
    }
    let service = match record.get("service") {
        Some(node) => {
            let value = name(node)?;
            if value != service_name {
                return Err(at(node, ErrorCode::ConstraintViolation,
                    "endpoint service differs from its enclosing service"));
            }
            value
        }
        None => copy_string(service_name, record.node)?,
    };
    let roles = match record.get("roles") {
        Some(node) => strings(Some(node))?,
        None => copy_strings(&defaults.roles, record.node)?,
    };
    let description = match record.get("description") {
        Some(node) => text(node)?,
        None => copy_string(&defaults.description, record.node)?,
    };
    Ok(EndpointSchemaSpec {
        code, name: endpoint_name, service, roles,
        params: list(record.get("params"), |node| field(node, 0))?,
        response: list(record.get("response"), |node| field(node, 0))?,
        stream_response: record.get("stream_response").map(stream_type).transpose()?.flatten(),
        description,
    })
}

fn stream_type(node: &ConfigNode) -> Result<Option<EndpointType>> {
    match &node.value {
        ConfigValue::Identifier(tag) if tag == "None" => Ok(None),
        ConfigValue::Variant { name, value } if name == "Some" =>
            normalize_type(one_argument(value, true)?).map(Some),
        ConfigValue::Identifier(tag) if tag == "Some" =>
            Err(at(node, ErrorCode::ConstraintViolation, "Some requires a tuple payload")),
        ConfigValue::Variant { name, .. } if name == "None" =>
            Err(at(node, ErrorCode::ConstraintViolation, "None must not have a payload")),
        _ => normalize_type(node).map(Some),
    }
}

fn definition(node: &ConfigNode) -> Result<TypeDefinition> {
    let record = record(node, &["Definition"], &["name", "ty"])?;
    Ok(TypeDefinition {
        name: name(record.required("name")?)?,
        ty: normalize_type(record.required("ty")?)?,
    })
}

fn field(node: &ConfigNode, depth: usize) -> Result<EndpointField> {
    let record = record(node, &["Field"], &["name", "ty", "description"])?;
    Ok(EndpointField {
        name: name(record.required("name")?)?,
        ty: type_at(record.required("ty")?, depth)?,
        description: description(&record)?,
    })
}

fn role(node: &ConfigNode) -> Result<RoleSpec> {
    if matches!(&node.value, ConfigValue::String(_)) {
        return Ok(RoleSpec { name: name(node)?, includes: Vec::new(), description: String::new() });
    }
    let record = record(node, &["Role"], &["name", "includes", "description"])?;
    Ok(RoleSpec {
        name: name(record.required("name")?)?,
        includes: strings(record.get("includes"))?,
        description: description(&record)?,
    })
}

fn error_spec(node: &ConfigNode) -> Result<ErrorSpec> {
    let record = record(node, &["Error"], &["code", "name", "message"])?;
    Ok(ErrorSpec {
        code: unsigned(record.required("code")?)?,
        name: name(record.required("name")?)?,
        message: text(record.required("message")?)?,
    })
}

fn header(node: &ConfigNode) -> Result<HeaderSpec> {
    let record = record(node, &["Header"], &["name", "ty", "required", "description"])?;
    Ok(HeaderSpec {
        name: name(record.required("name")?)?,
        ty: normalize_type(record.required("ty")?)?,
        required: record.get("required").map(boolean).transpose()?.unwrap_or(true),
        description: description(&record)?,
    })
}

fn enum_variant(node: &ConfigNode) -> Result<EnumVariantSpec> {
    let record = record(node, &["EnumVariant"], &["name", "value", "description"])?;
    Ok(EnumVariantSpec {
        name: name(record.required("name")?)?,
        value: signed_i32(record.required("value")?)?,
        description: description(&record)?,
    })
}

fn type_at(node: &ConfigNode, depth: usize) -> Result<EndpointType> {
    if depth > MAX_REFERENCE_DEPTH {
        return Err(at(node, ErrorCode::DepthLimitExceeded, "endpoint type nesting exceeds model limit"));
    }
    let (tag, payload) = match &node.value {
        ConfigValue::Identifier(tag) => (tag.as_str(), None),
        ConfigValue::Variant { name, value } => (name.as_str(), Some(value.as_ref())),
        _ => return Err(at(node, ErrorCode::TypeMismatch, "expected a type identifier or variant")),
    };
    let scalar = match tag {
        "UInt32" => Some(EndpointType::UInt32),
        "Int32" => Some(EndpointType::Int32),
        "Int64" => Some(EndpointType::Int64),
        "Float64" => Some(EndpointType::Float64),
        "Boolean" => Some(EndpointType::Boolean),
        "String" => Some(EndpointType::String),
        "Bytea" => Some(EndpointType::Bytea),
        "UUID" => Some(EndpointType::UUID),
        "IpAddr" => Some(EndpointType::IpAddr),
        "Object" => Some(EndpointType::Object),
        "Unit" => Some(EndpointType::Unit),
        "TimeStampMs" => Some(EndpointType::TimeStampMs),
        "BlockchainDecimal" => Some(EndpointType::BlockchainDecimal),
        "BlockchainAddress" => Some(EndpointType::BlockchainAddress),
        "BlockchainTransactionHash" => Some(EndpointType::BlockchainTransactionHash),
        _ => None,
    };
    if let Some(scalar) = scalar {
        return if payload.is_none() { Ok(scalar) } else {
            Err(at(node, ErrorCode::TypeMismatch, "scalar type must be a bare identifier"))
        };
    }
    if !matches!(tag, "NanoId" | "Struct" | "StructRef" | "StructTable" | "Vec" | "Optional" | "Enum" | "EnumRef") {
        return Err(at(node, ErrorCode::UnsupportedSchemaFeature, "unknown endpoint type tag"));
    }
    let payload = payload.ok_or_else(|| at(node, ErrorCode::TypeMismatch,
        "endpoint type requires a payload"))?;
    match tag {
        "NanoId" => {
            let record = record(payload, &[], &["len"])?;
            Ok(EndpointType::NanoId { len: unsigned(record.required("len")?)? })
        }
        "Struct" => {
            let record = record(payload, &[], &["name", "fields"])?;
            Ok(EndpointType::Struct {
                name: name(record.required("name")?)?,
                fields: list(record.get("fields"), |node| field(node, depth + 1))?,
            })
        }
        "StructRef" => Ok(EndpointType::StructRef(name(one_argument(payload, false)?)?)),
        "StructTable" => {
            let record = record(payload, &[], &["struct_ref"])?;
            Ok(EndpointType::StructTable { struct_ref: name(record.required("struct_ref")?)? })
        }
        "Vec" => Ok(EndpointType::Vec(Box::new(type_at(one_argument(payload, false)?, depth + 1)?))),
        "Optional" => Ok(EndpointType::Optional(Box::new(type_at(one_argument(payload, false)?, depth + 1)?))),
        "Enum" => {
            let record = record(payload, &[], &["name", "variants"])?;
            Ok(EndpointType::Enum {
                name: name(record.required("name")?)?,
                variants: list(record.get("variants"), enum_variant)?,
            })
        }
        "EnumRef" => {
            let record = record(payload, &[], &["name", "prefixed_name"])?;
            Ok(EndpointType::EnumRef {
                name: name(record.required("name")?)?,
                prefixed_name: record.get("prefixed_name").map(boolean).transpose()?.unwrap_or(false),
            })
        }
        _ => Err(at(node, ErrorCode::UnsupportedSchemaFeature, "unknown endpoint type tag")),
    }
}
