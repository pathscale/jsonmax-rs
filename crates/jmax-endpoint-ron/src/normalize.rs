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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use crate::{load::parse_str, ConfigLimits, ErrorSource};

    fn parse(input: &str) -> LoadedConfig {
        parse_str(input, "normalization-test", &ConfigLimits::default()).unwrap()
    }

    fn model(input: &str) -> EndpointModel {
        normalize(&parse(input)).unwrap()
    }

    fn ty(input: &str) -> EndpointType {
        normalize_type(&parse(input).root).unwrap()
    }

    fn node(value: ConfigValue) -> ConfigNode {
        ConfigNode { offset: 37, len: 1, value }
    }

    fn record_node(fields: Vec<(&str, ConfigNode)>) -> ConfigNode {
        node(ConfigValue::Record(fields.into_iter().map(|(key, value)| (String::from(key), value)).collect()))
    }

    fn check_error(error: Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code, "{error:?}");
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn model_failure(input: &str, code: ErrorCode) {
        let config = parse(input);
        let error = normalize(&config).unwrap_err();
        assert_eq!(error.code, code, "input: {input}; error: {error:?}");
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(normalize(&config).unwrap_err(), error);
    }

    fn type_failure(input: &str, code: ErrorCode) {
        let config = parse(input);
        let error = normalize_type(&config.root).unwrap_err();
        assert_eq!(error.code, code, "input: {input}; error: {error:?}");
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(normalize_type(&config.root).unwrap_err(), error);
    }

    #[test]
    fn minimal_models_accept_only_the_document_wrappers() {
        let expected = EndpointModel { version: ModelVersion::new(1, 2, 3), ..EndpointModel::default() };
        for input in [r#"(version: "1.2.3")"#, r#"{version: "1.2.3"}"#,
            r#"Config(version: "1.2.3")"#, r#"EndpointModel(version: "1.2.3")"#]
        {
            assert_eq!(model(input), expected);
        }
        model_failure(r#"Other(version: "1.2.3")"#, ErrorCode::TypeMismatch);
        model_failure("{}", ErrorCode::MissingRequiredField);
        for input in ["()", "[]", "Config", "Config()", "Config((version: \"1.2.3\"))"] {
            model_failure(input, ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn defaults_distinguish_omission_from_explicit_empty_values() {
        let input = r#"Config(
            version: "0.0.0",
            defaults: Defaults(service: Some("Catalog"), roles: ["reader", "writer"], description: " inherited \n"),
            services: [Service(id: 0, endpoints: [
                Endpoint(code: 0, name: "read"),
                Endpoint(code: 1, method: "write", roles: [], description: "", params: [], response: [], stream_response: None),
            ]), Service(id: 1, name: "Other", endpoints: [(code: 2, name: "ping")])],
        )"#;
        let result = model(input);
        let inherited = &result.services[0].endpoints[0];
        assert_eq!(result.services[0].name, "Catalog");
        assert_eq!(inherited.service, "Catalog");
        assert_eq!(inherited.roles, ["reader", "writer"]);
        assert_eq!(inherited.description, " inherited \n");
        assert!(inherited.params.is_empty());
        assert!(inherited.response.is_empty());
        assert_eq!(inherited.stream_response, None);
        let explicit = &result.services[0].endpoints[1];
        assert_eq!(explicit.name, "write");
        assert!(explicit.roles.is_empty());
        assert!(explicit.description.is_empty());
        assert_eq!(result.services[1].endpoints[0].service, "Other");
        assert_eq!(result.services[1].endpoints[0].roles, ["reader", "writer"]);
        assert!(result.roles.is_empty());
        assert_eq!(normalize(&parse(input)).unwrap(), result);
    }

    #[test]
    fn defaults_accept_direct_strings_none_and_some() {
        assert_eq!(normalize_defaults(&parse("{}").root).unwrap(), ConfigDefaults::default());
        assert_eq!(normalize_defaults(&parse("Defaults(service: None)").root).unwrap(), ConfigDefaults::default());
        for input in [r#"(service: "Api", roles: ["admin"], description: " text ")"#,
            r#"Defaults(service: Some("Api"), roles: ["admin"], description: " text ")"#]
        {
            assert_eq!(normalize_defaults(&parse(input).root).unwrap(), ConfigDefaults {
                service: Some(String::from("Api")), roles: vec![String::from("admin")],
                description: String::from(" text "),
            });
        }
        let empty = normalize_defaults(&parse(r#"(service: "", roles: [], description: "")"#).root).unwrap();
        assert_eq!(empty.service.as_deref(), Some(""));
        assert!(empty.roles.is_empty());
        assert!(empty.description.is_empty());
    }

    #[test]
    fn malformed_default_options_and_shapes_fail_closed() {
        for input in ["(service: Some)", "(service: None())", "(service: None(\"Api\"))",
            "(service: Some())", "(service: Some(\"a\", \"b\"))", "(service: Some(name: \"Api\"))"]
        {
            assert_eq!(normalize_defaults(&parse(input).root).unwrap_err().code, ErrorCode::ConstraintViolation, "{input}");
        }
        for input in ["(service: 0)", "(service: Some(None))", "(service: Some(0))",
            "(service: Other(\"Api\"))", "(roles: ())", "(roles: [1])", "(description: None)",
            "Defaults()", "Other(service: None)"]
        {
            assert_eq!(normalize_defaults(&parse(input).root).unwrap_err().code, ErrorCode::TypeMismatch, "{input}");
        }
        assert_eq!(normalize_defaults(&parse("(code: 1)").root).unwrap_err().code, ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn all_records_preserve_order_and_metadata() {
        let result = model(r#"EndpointModel(
            version: Version(major: 1, minor: 2, patch: 3),
            defaults: (description: "endpoint default"),
            services: [Service(id: 65535, name: "Store", endpoints: [Endpoint(
                code: 4294967295, method: "Read", service: "Store", roles: ["writer", "reader"],
                params: [Field(name: "z", ty: UInt32, description: " param "), (name: "a", ty: StructRef("Unresolved"))],
                response: [Field(name: "done", ty: Boolean)],
                stream_response: Some(Vec(EnumRef(name: "State", prefixed_name: true))),
            )]), (id: 0, name: "Health")],
            definitions: [Definition(name: "Second", ty: Struct(name: "Second", fields: [Field(name: "x", ty: String)])),
                (name: "First", ty: Enum(name: "First", variants: [
                    EnumVariant(name: "Last", value: -2147483648, description: " low "),
                    (name: "First", value: 2147483647),
                ]))],
            roles: [Role(name: "writer", includes: ["reader"], description: " Write "), "reader"],
            errors: [Error(code: 4294967295, name: "Unavailable", message: " Retry\n "),
                (code: 0, name: "Empty", message: "")],
            headers: [Header(name: "X-Z", ty: UUID, required: false, description: " header "),
                (name: "X-A", ty: String)],
        )"#);
        assert_eq!(result.version, ModelVersion::new(1, 2, 3));
        assert_eq!(result.services.iter().map(|value| value.name.as_str()).collect::<Vec<_>>(), ["Store", "Health"]);
        assert_eq!(result.services[0].id, u16::MAX);
        assert!(result.services[1].endpoints.is_empty());
        let endpoint = &result.services[0].endpoints[0];
        assert_eq!(endpoint.code, u32::MAX);
        assert_eq!(endpoint.name, "Read");
        assert_eq!(endpoint.service, "Store");
        assert_eq!(endpoint.roles, ["writer", "reader"]);
        assert_eq!(endpoint.description, "endpoint default");
        assert_eq!(endpoint.params.iter().map(|value| value.name.as_str()).collect::<Vec<_>>(), ["z", "a"]);
        assert_eq!(endpoint.params[0].description, " param ");
        assert_eq!(endpoint.params[1].description, "");
        assert_eq!(endpoint.params[1].ty, EndpointType::StructRef(String::from("Unresolved")));
        assert_eq!(endpoint.response[0], EndpointField { name: String::from("done"), ty: EndpointType::Boolean, description: String::new() });
        assert_eq!(endpoint.stream_response, Some(EndpointType::Vec(Box::new(EndpointType::EnumRef {
            name: String::from("State"), prefixed_name: true,
        }))));
        assert_eq!(result.definitions.iter().map(|value| value.name.as_str()).collect::<Vec<_>>(), ["Second", "First"]);
        let EndpointType::Struct { fields, .. } = &result.definitions[0].ty else { panic!("expected struct"); };
        assert_eq!(fields[0].description, "");
        let EndpointType::Enum { variants, .. } = &result.definitions[1].ty else { panic!("expected enum"); };
        assert_eq!(variants[0], EnumVariantSpec { name: String::from("Last"), value: i32::MIN, description: String::from(" low ") });
        assert_eq!(variants[1], EnumVariantSpec { name: String::from("First"), value: i32::MAX, description: String::new() });
        assert_eq!(result.roles[0], RoleSpec { name: String::from("writer"), includes: vec![String::from("reader")], description: String::from(" Write ") });
        assert_eq!(result.roles[1], RoleSpec { name: String::from("reader"), includes: vec![], description: String::new() });
        assert_eq!(result.errors[0], ErrorSpec { code: u32::MAX, name: String::from("Unavailable"), message: String::from(" Retry\n ") });
        assert_eq!(result.errors[1].code, 0);
        assert_eq!(result.errors[1].message, "");
        assert_eq!(result.headers[0], HeaderSpec { name: String::from("X-Z"), ty: EndpointType::UUID, required: false, description: String::from(" header ") });
        assert_eq!(result.headers[1], HeaderSpec { name: String::from("X-A"), ty: EndpointType::String, required: true, description: String::new() });
    }

    #[test]
    fn every_scalar_type_has_its_exact_spelling() {
        for (input, expected) in [
            ("UInt32", EndpointType::UInt32), ("Int32", EndpointType::Int32),
            ("Int64", EndpointType::Int64), ("Float64", EndpointType::Float64),
            ("Boolean", EndpointType::Boolean), ("String", EndpointType::String),
            ("Bytea", EndpointType::Bytea), ("UUID", EndpointType::UUID),
            ("IpAddr", EndpointType::IpAddr), ("Object", EndpointType::Object),
            ("Unit", EndpointType::Unit), ("TimeStampMs", EndpointType::TimeStampMs),
            ("BlockchainDecimal", EndpointType::BlockchainDecimal),
            ("BlockchainAddress", EndpointType::BlockchainAddress),
            ("BlockchainTransactionHash", EndpointType::BlockchainTransactionHash),
        ] {
            assert_eq!(ty(input), expected);
            type_failure(&format!("{input}()"), ErrorCode::TypeMismatch);
            type_failure(&format!("{input}(1)"), ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn payload_types_and_their_defaults_are_canonical() {
        for (input, expected) in [
            ("NanoId(len: 0)", EndpointType::NanoId { len: 0 }),
            ("NanoId(len: 255)", EndpointType::NanoId { len: u8::MAX }),
            (r#"Struct(name: "Item")"#, EndpointType::Struct { name: String::from("Item"), fields: vec![] }),
            (r#"StructRef("Item",)"#, EndpointType::StructRef(String::from("Item"))),
            (r#"StructTable(struct_ref: "Item")"#, EndpointType::StructTable { struct_ref: String::from("Item") }),
            ("Vec(UInt32)", EndpointType::Vec(Box::new(EndpointType::UInt32))),
            ("Optional(String)", EndpointType::Optional(Box::new(EndpointType::String))),
            (r#"Enum(name: "State")"#, EndpointType::Enum { name: String::from("State"), variants: vec![] }),
            (r#"EnumRef(name: "State")"#, EndpointType::EnumRef { name: String::from("State"), prefixed_name: false }),
            (r#"EnumRef(name: "State", prefixed_name: true)"#, EndpointType::EnumRef { name: String::from("State"), prefixed_name: true }),
        ] {
            assert_eq!(ty(input), expected);
        }
        assert_eq!(ty("Optional(Vec(Optional(Unit)))"), EndpointType::Optional(Box::new(
            EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::Unit)))),
        )));
    }

    #[test]
    fn unknown_types_and_strings_never_become_references() {
        for input in ["Unknown", "uint32", "Uuid", "TimestampMs", "None", "Some", "Unknown()", "Unknown(name: \"X\")"] {
            type_failure(input, ErrorCode::UnsupportedSchemaFeature);
        }
        for input in [r#""UInt32""#, r#""Vec(UInt32)""#, "()", "0", "true", "[]", "{}", "(UInt32,)"] {
            type_failure(input, ErrorCode::TypeMismatch);
        }
        for input in ["NanoId", "Struct", "StructRef", "StructTable", "Vec", "Optional", "Enum", "EnumRef"] {
            type_failure(input, ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn unary_type_payloads_require_one_tuple_argument() {
        for input in ["Vec()", "Vec(UInt32, Int32)", "Vec(ty: UInt32)",
            "Optional()", "Optional(UInt32, Int32)", "Optional(ty: UInt32)",
            "StructRef()", "StructRef(\"A\", \"B\")", "StructRef(name: \"A\")",
            "StructRef(UInt32)"]
        {
            type_failure(input, ErrorCode::TypeMismatch);
        }
        let malformed = node(ConfigValue::Variant {
            name: String::from("Vec"), value: Box::new(node(ConfigValue::Identifier(String::from("UInt32")))),
        });
        check_error(normalize_type(&malformed).unwrap_err(), ErrorCode::TypeMismatch, 37);
    }

    #[test]
    fn type_record_payloads_reject_wrong_shapes_unknown_and_missing_keys() {
        for input in ["NanoId()", "NanoId(1)", "Struct()", "Struct(\"A\")", "StructTable()",
            "Enum()", "EnumRef()", "EnumRef(\"A\")", "StructTable(\"A\")"]
        {
            type_failure(input, ErrorCode::TypeMismatch);
        }
        for input in ["NanoId(other: 1)", "Struct(name: \"A\", unknown: [])",
            "StructTable(struct_ref: \"A\", unknown: true)", "Enum(name: \"A\", unknown: [])",
            "EnumRef(name: \"A\", unknown: false)", "Struct(name: \"A\", fields: [Field(name: \"x\", ty: Unit, extra: 0)])",
            "Enum(name: \"A\", variants: [EnumVariant(name: \"x\", value: 0, extra: 0)])"]
        {
            type_failure(input, ErrorCode::UnknownObjectKey);
        }
        for input in ["Struct(fields: [])", "StructTable(other: \"A\")", "Enum(variants: [])", "EnumRef(prefixed_name: true)"] {
            let expected = if input.starts_with("StructTable") { ErrorCode::UnknownObjectKey } else { ErrorCode::MissingRequiredField };
            type_failure(input, expected);
        }
        for input in ["Struct(name: \"A\", fields: ())", "Enum(name: \"A\", variants: ())", "EnumRef(name: \"A\", prefixed_name: 0)"] {
            type_failure(input, ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn integer_domains_accept_boundaries_without_narrowing() {
        assert_eq!(model(r#"(version: "0.0.0", services: [(id: 0, name: "S", endpoints: [(code: 0, name: "m")])])"#).services[0].id, 0);
        assert_eq!(model(r#"(version: "0.0.0", services: [(id: 65535, name: "S", endpoints: [(code: 4294967295, name: "m")])])"#).services[0].endpoints[0].code, u32::MAX);
        assert_eq!(ty("NanoId(len: -0)"), EndpointType::NanoId { len: 0 });
        for value in [ConfigValue::U64(0), ConfigValue::I64(0), ConfigValue::I64(255)] {
            let input = node(value);
            assert!(unsigned::<u8>(&input).is_ok());
        }
        for value in [ConfigValue::U64(256), ConfigValue::U64(u64::MAX), ConfigValue::I64(-1), ConfigValue::I64(i64::MIN)] {
            check_error(unsigned::<u8>(&node(value)).unwrap_err(), ErrorCode::NumberOverflow, 37);
        }
        assert_eq!(signed_i32(&node(ConfigValue::I64(i64::from(i32::MIN)))).unwrap(), i32::MIN);
        assert_eq!(signed_i32(&node(ConfigValue::U64(u64::try_from(i32::MAX).unwrap()))).unwrap(), i32::MAX);
        for value in [ConfigValue::U64(u64::MAX), ConfigValue::I64(i64::MIN), ConfigValue::I64(i64::MAX)] {
            check_error(signed_i32(&node(value)).unwrap_err(), ErrorCode::NumberOverflow, 37);
        }
    }

    #[test]
    fn overflowing_integer_fields_return_number_overflow() {
        for suffix in ["services: [(id: 65536, name: \"S\")]", "services: [(id: -1, name: \"S\")]",
            "services: [(id: 18446744073709551615, name: \"S\")]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 4294967296, name: \"m\")])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: -1, name: \"m\")])]",
            "errors: [(code: 4294967296, name: \"E\", message: \"\")]",
            "errors: [(code: -1, name: \"E\", message: \"\")]"]
        {
            model_failure(&format!("(version: \"1.0.0\", {suffix})"), ErrorCode::NumberOverflow);
        }
        for input in ["NanoId(len: 256)", "NanoId(len: -1)",
            "Enum(name: \"E\", variants: [(name: \"X\", value: 2147483648)])",
            "Enum(name: \"E\", variants: [(name: \"X\", value: -2147483649)])"]
        {
            type_failure(input, ErrorCode::NumberOverflow);
        }
    }

    #[test]
    fn integer_fields_reject_nonintegers() {
        for suffix in ["services: [(id: true, name: \"S\")]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: \"1\", name: \"m\")])]",
            "errors: [(code: false, name: \"E\", message: \"\")]"]
        {
            model_failure(&format!("(version: \"1.0.0\", {suffix})"), ErrorCode::TypeMismatch);
        }
        type_failure("NanoId(len: \"21\")", ErrorCode::TypeMismatch);
        type_failure("Enum(name: \"E\", variants: [(name: \"X\", value: true)])", ErrorCode::TypeMismatch);
    }

    #[test]
    fn version_strings_are_strict_and_errors_use_the_config_node_offset() {
        assert_eq!(model(r#"(version: "4294967295.0.4294967295")"#).version, ModelVersion::new(u32::MAX, 0, u32::MAX));
        for value in ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.03", "+1.2.3",
            "-1.2.3", " 1.2.3", "1.2.3 ", "v1.2.3", "1.2.3-alpha", "1.2.3+build", "1.２.3"]
        {
            let input = format!("(version: \"{value}\")");
            check_error(normalize(&parse(&input)).unwrap_err(), ErrorCode::InvalidSyntax, 10);
        }
        for value in ["4294967296.0.0", "0.4294967296.0", "0.0.4294967296"] {
            let input = format!("(version: \"{value}\")");
            check_error(normalize(&parse(&input)).unwrap_err(), ErrorCode::NumberOverflow, 10);
        }
    }

    #[test]
    fn version_records_require_three_u32_components() {
        for input in ["(version: (major: 0, minor: 4294967295, patch: 0))",
            "(version: Version(major: 0, minor: 4294967295, patch: 0))"]
        {
            assert_eq!(model(input).version, ModelVersion::new(0, u32::MAX, 0));
        }
        for input in ["(version: (minor: 0, patch: 0))", "(version: (major: 0, patch: 0))", "(version: (major: 0, minor: 0))"] {
            model_failure(input, ErrorCode::MissingRequiredField);
        }
        for input in ["(version: Version(major: 4294967296, minor: 0, patch: 0))",
            "(version: Version(major: 0, minor: -1, patch: 0))", "(version: Version(major: 0, minor: 0, patch: 4294967296))"]
        {
            model_failure(input, ErrorCode::NumberOverflow);
        }
        for input in ["(version: 1)", "(version: (1, 2, 3))", "(version: Other(major: 1, minor: 2, patch: 3))",
            "(version: Version(major: true, minor: 0, patch: 0))"]
        {
            model_failure(input, ErrorCode::TypeMismatch);
        }
        model_failure("(version: Version(major: 1, minor: 2, patch: 3, extra: 0))", ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn endpoints_require_one_local_name_and_matching_service() {
        for endpoint in ["(code: 0)", "(name: \"m\")"] {
            model_failure(&format!("(version: \"0.0.0\", services: [(id: 0, name: \"S\", endpoints: [{endpoint}])])"), ErrorCode::MissingRequiredField);
        }
        for endpoint in ["(code: 0, name: \"m\", method: \"m\")", "(code: 0, name: \"m\", method: \"n\")",
            "(code: 0, name: \"S.m\")", "(code: 0, method: \"S.m\")", "(code: 0, name: \"m\", service: \"s\")"]
        {
            model_failure(&format!("(version: \"0.0.0\", services: [(id: 0, name: \"S\", endpoints: [{endpoint}])])"), ErrorCode::ConstraintViolation);
        }
        assert_eq!(model(r#"(version: "0.0.0", services: [(id: 0, name: "S", endpoints: [(code: 0, method: "m", service: "S")])])"#).services[0].endpoints[0].name, "m");
    }

    #[test]
    fn service_defaults_never_invent_ids_or_override_explicit_names() {
        for input in [r#"(version: "0.0.0", services: [(id: 0)])"#,
            r#"(version: "0.0.0", defaults: (service: None), services: [(id: 0)])"#,
            r#"(version: "0.0.0", defaults: (service: "S"), services: [(name: "T")])"#]
        {
            model_failure(input, ErrorCode::MissingRequiredField);
        }
        for input in [r#"(version: "0.0.0", defaults: (service: "S"), services: [(id: 0, name: "")])"#,
            r#"(version: "0.0.0", defaults: (service: ""), services: [(id: 0)])"#]
        {
            model_failure(input, ErrorCode::ConstraintViolation);
        }
        assert_eq!(model(r#"(version: "0.0.0", defaults: (service: "S"), services: [(id: 0, name: "T")])"#).services[0].name, "T");
    }

    #[test]
    fn stream_responses_accept_absence_options_and_direct_types() {
        for (input, expected) in [("None", None), ("Some(Unit)", Some(EndpointType::Unit)),
            ("UInt32", Some(EndpointType::UInt32)), ("Vec(String)", Some(EndpointType::Vec(Box::new(EndpointType::String)))),
            ("Some(Optional(UInt32))", Some(EndpointType::Optional(Box::new(EndpointType::UInt32))))]
        {
            let result = model(&format!("(version: \"0.0.0\", services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", stream_response: {input})])])"));
            assert_eq!(result.services[0].endpoints[0].stream_response, expected);
        }
        for input in ["Some", "None()", "None(UInt32)", "Some()", "Some(UInt32, Int32)", "Some(ty: UInt32)"] {
            check_error(stream_type(&parse(input).root).unwrap_err(), ErrorCode::ConstraintViolation,
                if input.starts_with("Some(") { 4 } else { 0 });
        }
        assert_eq!(stream_type(&parse("Some(1)").root).unwrap_err().code, ErrorCode::TypeMismatch);
        assert_eq!(stream_type(&parse("Some(Some(Unit))").root).unwrap_err().code, ErrorCode::UnsupportedSchemaFeature);
    }

    #[test]
    fn missing_keys_are_reported_for_every_record_kind() {
        for suffix in ["definitions: [(name: \"D\")]", "definitions: [(ty: Unit)]", "roles: [(includes: [])]",
            "errors: [(name: \"E\", message: \"\")]", "errors: [(code: 0, message: \"\")]", "errors: [(code: 0, name: \"E\")]",
            "headers: [(name: \"X\")]", "headers: [(ty: String)]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", params: [(name: \"p\")])])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", response: [(ty: Unit)])])]"]
        {
            model_failure(&format!("(version: \"0.0.0\", {suffix})"), ErrorCode::MissingRequiredField);
        }
        for input in ["Enum(name: \"E\", variants: [(name: \"V\")])", "Enum(name: \"E\", variants: [(value: 0)])",
            "Struct(name: \"S\", fields: [(name: \"x\")])", "Struct(name: \"S\", fields: [(ty: Unit)])"]
        {
            type_failure(input, ErrorCode::MissingRequiredField);
        }
        for input in ["NanoId", "StructTable"] {
            // A hand-built empty Record is distinct from the parser's Unit.
            let input = node(ConfigValue::Variant { name: String::from(input), value: Box::new(record_node(vec![])) });
            check_error(normalize_type(&input).unwrap_err(), ErrorCode::MissingRequiredField, 37);
        }
    }

    #[test]
    fn unknown_fields_include_endpoint_local_error_references() {
        for suffix in ["extra: 0", "defaults: (code: 0)", "services: [(id: 0, name: \"S\", extra: 0)]",
            "definitions: [(name: \"D\", ty: Unit, extra: 0)]", "roles: [(name: \"r\", extra: 0)]",
            "errors: [(code: 0, name: \"E\", message: \"\", extra: 0)]", "headers: [(name: \"X\", ty: Unit, extra: 0)]"]
        {
            model_failure(&format!("(version: \"0.0.0\", {suffix})"), ErrorCode::UnknownObjectKey);
        }
        for key in ["errors", "error", "error_refs", "extra"] {
            model_failure(&format!("(version: \"0.0.0\", services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", {key}: [])])])"), ErrorCode::UnknownObjectKey);
        }
    }

    #[test]
    fn duplicate_keys_in_manual_trees_are_never_hidden_by_defaults() {
        for (wrapper, key) in [("Config", "version"), ("EndpointModel", "services"),
            ("Defaults", "roles"), ("Service", "id"), ("Endpoint", "name"),
            ("Definition", "ty"), ("Field", "description"), ("Role", "includes"),
            ("Error", "message"), ("Header", "required"), ("EnumVariant", "value"),
            ("Version", "patch"), ("NanoId", "len"), ("Struct", "fields"),
            ("StructTable", "struct_ref"), ("Enum", "variants"), ("EnumRef", "prefixed_name")]
        {
            let first = node(ConfigValue::Unit);
            let mut second = node(ConfigValue::Unit);
            second.offset = 99;
            let payload = record_node(vec![(key, first), (key, second)]);
            let wrapped = node(ConfigValue::Variant { name: String::from(wrapper), value: Box::new(payload.clone()) });
            // Test the shared shape gate independently of required key order.
            check_error(record(&payload, &[wrapper], &[key]).err().unwrap(), ErrorCode::DuplicateObjectKey, 99);
            check_error(record(&wrapped, &[wrapper], &[key]).err().unwrap(), ErrorCode::DuplicateObjectKey, 99);
        }
        let input = LoadedConfig { source_name: String::from("manual"), root: record_node(vec![
            ("version", node(ConfigValue::String(String::from("1.2.3")))),
            ("version", node(ConfigValue::String(String::from("2.0.0")))),
        ]) };
        check_error(normalize(&input).unwrap_err(), ErrorCode::DuplicateObjectKey, 37);
        let defaults = record_node(vec![("roles", node(ConfigValue::Sequence(vec![]))), ("roles", node(ConfigValue::Sequence(vec![])))]);
        check_error(normalize_defaults(&defaults).unwrap_err(), ErrorCode::DuplicateObjectKey, 37);
        for tag in ["NanoId", "Struct", "StructTable", "Enum", "EnumRef"] {
            let key = match tag { "NanoId" => "len", "StructTable" => "struct_ref", _ => "name" };
            let input = node(ConfigValue::Variant { name: String::from(tag), value: Box::new(record_node(vec![
                (key, node(ConfigValue::Unit)), (key, node(ConfigValue::Unit)),
            ])) });
            check_error(normalize_type(&input).unwrap_err(), ErrorCode::DuplicateObjectKey, 37);
        }
    }

    #[test]
    fn arbitrary_and_nested_record_wrappers_are_rejected() {
        for suffix in ["defaults: Wrong(roles: [])", "services: [Wrong(id: 0, name: \"S\")]",
            "definitions: [Wrong(name: \"D\", ty: Unit)]", "roles: [Wrong(name: \"r\")]",
            "errors: [Wrong(code: 0, name: \"E\", message: \"\")]", "headers: [Wrong(name: \"X\", ty: Unit)]",
            "services: [(id: 0, name: \"S\", endpoints: [Wrong(code: 0, name: \"m\")])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", params: [Wrong(name: \"p\", ty: Unit)])])]"]
        {
            model_failure(&format!("(version: \"0.0.0\", {suffix})"), ErrorCode::TypeMismatch);
        }
        type_failure("Enum(name: \"E\", variants: [Wrong(name: \"V\", value: 0)])", ErrorCode::TypeMismatch);
        type_failure("Struct(name: \"S\", fields: [Wrong(name: \"f\", ty: Unit)])", ErrorCode::TypeMismatch);
        let nested = node(ConfigValue::Variant { name: String::from("Defaults"), value: Box::new(
            node(ConfigValue::Variant { name: String::from("Defaults"), value: Box::new(record_node(vec![])) }),
        ) });
        check_error(normalize_defaults(&nested).unwrap_err(), ErrorCode::TypeMismatch, 37);
        let nested_type = node(ConfigValue::Variant { name: String::from("Struct"), value: Box::new(
            node(ConfigValue::Variant { name: String::from("Struct"), value: Box::new(record_node(vec![])) }),
        ) });
        check_error(normalize_type(&nested_type).unwrap_err(), ErrorCode::TypeMismatch, 37);
    }

    #[test]
    fn sequence_and_scalar_shapes_are_not_coerced() {
        for suffix in ["services: ()", "definitions: {}", "roles: \"reader\"", "errors: None", "headers: false",
            "services: [(id: 0, name: \"S\", endpoints: ())]", "roles: [(name: \"r\", includes: ())]",
            "headers: [(name: \"X\", ty: Unit, required: 1)]", "headers: [(name: \"X\", ty: Unit, description: false)]",
            "errors: [(code: 0, name: \"E\", message: false)]", "definitions: [(name: D, ty: Unit)]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", roles: ())])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", params: {})])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", response: Unit)])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", description: None)])]"]
        {
            model_failure(&format!("(version: \"0.0.0\", {suffix})"), ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn empty_required_names_are_rejected_in_every_position() {
        for suffix in ["services: [(id: 0, name: \"\")]", "roles: [\"\"]", "roles: [(name: \"\")]",
            "roles: [(name: \"r\", includes: [\"\"])]", "definitions: [(name: \"\", ty: Unit)]",
            "errors: [(code: 0, name: \"\", message: \"\")]", "headers: [(name: \"\", ty: Unit)]",
            "defaults: (roles: [\"\"])",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"\")])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", service: \"\")])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", roles: [\"\"])])]",
            "services: [(id: 0, name: \"S\", endpoints: [(code: 0, name: \"m\", params: [(name: \"\", ty: Unit)])])]"]
        {
            model_failure(&format!("(version: \"0.0.0\", {suffix})"), ErrorCode::ConstraintViolation);
        }
        for input in ["Struct(name: \"\")", "StructRef(\"\")", "StructTable(struct_ref: \"\")", "Enum(name: \"\")",
            "EnumRef(name: \"\")", "Struct(name: \"S\", fields: [(name: \"\", ty: Unit)])",
            "Enum(name: \"E\", variants: [(name: \"\", value: 0)])"]
        {
            type_failure(input, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn semantic_errors_retain_relevant_byte_offsets() {
        let input = "/* λ */ Config(version: \"1.0.0\", extra: 42)";
        check_error(normalize(&parse(input)).unwrap_err(), ErrorCode::UnknownObjectKey,
            u32::try_from(input.find("42").unwrap()).unwrap());
        let input = "Config(version: \"1.0.0\", services: [Service(id: 65536, name: \"S\")])";
        check_error(normalize(&parse(input)).unwrap_err(), ErrorCode::NumberOverflow,
            u32::try_from(input.find("65536").unwrap()).unwrap());
        let input = "Config(version: \"1.0.0\", definitions: [Definition(name: \"D\")])";
        check_error(normalize(&parse(input)).unwrap_err(), ErrorCode::MissingRequiredField,
            u32::try_from(input.find("(name:").unwrap()).unwrap());
        let input = "Optional(Vec(NotAType))";
        check_error(normalize_type(&parse(input).root).unwrap_err(), ErrorCode::UnsupportedSchemaFeature, 13);
        let mut input = node(ConfigValue::Identifier(String::from("unknown")));
        input.offset = u32::MAX;
        input.len = u32::MAX;
        check_error(normalize_type(&input).unwrap_err(), ErrorCode::UnsupportedSchemaFeature, u32::MAX);
    }

    fn nested_type(edges: usize, mixed: bool) -> ConfigNode {
        let mut inner = ConfigNode { offset: 777, len: 1, value: ConfigValue::Identifier(String::from("Unit")) };
        for index in 0..edges {
            inner = if mixed && index % 3 == 2 {
                node(ConfigValue::Variant { name: String::from("Struct"), value: Box::new(record_node(vec![
                    ("name", node(ConfigValue::String(String::from("Nested")))),
                    ("fields", node(ConfigValue::Sequence(vec![record_node(vec![
                        ("name", node(ConfigValue::String(String::from("child")))), ("ty", inner),
                    ])]))),
                ])) })
            } else {
                node(ConfigValue::Variant {
                    name: String::from(if mixed && index % 3 == 1 { "Optional" } else { "Vec" }),
                    value: Box::new(node(ConfigValue::Tuple(vec![inner]))),
                })
            };
        }
        inner
    }

    #[test]
    fn manually_constructed_type_depth_observes_the_exact_model_boundary() {
        for mixed in [false, true] {
            assert!(normalize_type(&nested_type(0, mixed)).is_ok());
            assert!(normalize_type(&nested_type(MAX_REFERENCE_DEPTH - 1, mixed)).is_ok());
            assert!(normalize_type(&nested_type(MAX_REFERENCE_DEPTH, mixed)).is_ok());
            check_error(normalize_type(&nested_type(MAX_REFERENCE_DEPTH + 1, mixed)).unwrap_err(),
                ErrorCode::DepthLimitExceeded, 777);
        }
    }

    #[test]
    fn every_model_type_root_uses_the_same_depth_budget() {
        for edges in [MAX_REFERENCE_DEPTH, MAX_REFERENCE_DEPTH + 1] {
            for position in ["definition", "header", "params", "response", "stream", "some_stream"] {
                let deep = nested_type(edges, false);
                let mut fields = vec![("version", node(ConfigValue::String(String::from("0.0.0"))))];
                match position {
                    "definition" => fields.push(("definitions", node(ConfigValue::Sequence(vec![record_node(vec![
                        ("name", node(ConfigValue::String(String::from("D")))), ("ty", deep),
                    ])])))),
                    "header" => fields.push(("headers", node(ConfigValue::Sequence(vec![record_node(vec![
                        ("name", node(ConfigValue::String(String::from("X")))), ("ty", deep),
                    ])])))),
                    _ => {
                        let value = match position {
                            "params" | "response" => node(ConfigValue::Sequence(vec![record_node(vec![
                                ("name", node(ConfigValue::String(String::from("p")))), ("ty", deep),
                            ])])),
                            "some_stream" => node(ConfigValue::Variant { name: String::from("Some"),
                                value: Box::new(node(ConfigValue::Tuple(vec![deep]))) }),
                            _ => deep,
                        };
                        let key = if position == "stream" || position == "some_stream" { "stream_response" } else { position };
                        fields.push(("services", node(ConfigValue::Sequence(vec![record_node(vec![
                            ("id", node(ConfigValue::U64(0))),
                            ("name", node(ConfigValue::String(String::from("S")))),
                            ("endpoints", node(ConfigValue::Sequence(vec![record_node(vec![
                                ("code", node(ConfigValue::U64(0))),
                                ("name", node(ConfigValue::String(String::from("m")))), (key, value),
                            ])]))),
                        ])]))));
                    }
                }
                let input = LoadedConfig { source_name: String::from("manual"), root: record_node(fields) };
                let result = normalize(&input);
                if edges == MAX_REFERENCE_DEPTH {
                    assert!(result.is_ok(), "{position}: {result:?}");
                } else {
                    check_error(result.unwrap_err(), ErrorCode::DepthLimitExceeded, 777);
                }
            }
        }
    }

    #[test]
    fn normalization_preserves_unresolved_references_and_duplicate_codes() {
        let result = model(r#"(version: "0.0.0",
            services: [(id: 0, name: "S", endpoints: [(code: 0, name: "a"), (code: 0, name: "b")])],
            definitions: [(name: "Alias", ty: StructRef("Missing"))],
            roles: ["r", "r"], errors: [(code: 0, name: "E", message: ""), (code: 0, name: "E", message: "")],
            headers: [(name: "MiXeD", ty: EnumRef(name: "Missing"))],
        )"#);
        assert_eq!(result.services[0].endpoints.iter().map(|endpoint| endpoint.code).collect::<Vec<_>>(), [0, 0]);
        assert_eq!(result.definitions[0].ty, EndpointType::StructRef(String::from("Missing")));
        assert_eq!(result.roles[0], result.roles[1]);
        assert_eq!(result.errors[0], result.errors[1]);
        assert_eq!(result.headers[0].name, "MiXeD");
        assert_eq!(result.headers[0].ty, EndpointType::EnumRef { name: String::from("Missing"), prefixed_name: false });
    }

    #[test]
    fn normalization_keeps_unicode_text_casing_and_the_input_tree() {
        let input = r#"(version: "1.2.3", services: [(id: 1, name: "MiXeD", endpoints: [
            (code: 2, name: "Read", params: [(name: "字段 🦀", ty: String, description: " café\t\0\n ")]),
        ])], roles: ["reader"])"#;
        let loaded = parse(input);
        let snapshot = loaded.clone();
        let first = normalize(&loaded).unwrap();
        let second = normalize(&loaded).unwrap();
        assert_eq!(first, second);
        assert_eq!(loaded, snapshot);
        assert_eq!(first.services[0].name, "MiXeD");
        assert_eq!(first.services[0].endpoints[0].name, "Read");
        assert_eq!(first.services[0].endpoints[0].params[0].name, "字段 🦀");
        assert_eq!(first.services[0].endpoints[0].params[0].description, " café\t\0\n ");
    }
}
