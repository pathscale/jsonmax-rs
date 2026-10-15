//! Strict loading and semantic validation for version-1 service models.

use super::*;
use std::{collections::{BTreeMap, BTreeSet}, fs::File, io::Read, path::Path};

use jmax_abi::endpoint::EnumVariantSpec;
use jmax_dom::{Array, Node, NodeKind, Object, Scalar};

pub const MODEL_VERSION: u16 = 1;

fn decode_error(code: ErrorCode, node: Option<Node<'_>>, detail: &'static str) -> ValidatorError {
    let offset = node.map(|n| jmax_dom::navigate::raw_span(n).offset).unwrap_or(0);
    ValidatorError::Validation(AbiError::new(code, ErrorSource::Decode, offset, detail))
}

fn error(code: ErrorCode, detail: &'static str) -> ValidatorError {
    ValidatorError::Validation(AbiError::new(code, ErrorSource::Decode, 0, detail))
}

fn object<'a>(node: Node<'a>) -> ValidatorResult<Object<'a>> {
    jmax_dom::navigate::object(node).map_err(ValidatorError::from)
}

fn array<'a>(node: Node<'a>) -> ValidatorResult<Array<'a>> {
    jmax_dom::navigate::array(node).map_err(ValidatorError::from)
}

fn kind(node: Node<'_>) -> NodeKind { jmax_dom::navigate::kind(node) }

fn members<'a>(node: Node<'a>) -> ValidatorResult<Vec<(String, Node<'a>)>> {
    let object = object(node)?;
    let mut result = Vec::new();
    for member in object.iter() {
        result.push((member.key.to_owned(), member.value));
    }
    Ok(result)
}

fn fields<'a>(node: Node<'a>, allowed: &[&str], required: &[&str]) -> ValidatorResult<BTreeMap<String, Node<'a>>> {
    let mut values = BTreeMap::new();
    for (key, value) in members(node)? {
        if !allowed.iter().any(|known| *known == key) {
            return Err(decode_error(ErrorCode::UnknownObjectKey, Some(value), "unknown model field"));
        }
        if values.insert(key, value).is_some() {
            return Err(decode_error(ErrorCode::DuplicateObjectKey, Some(value), "duplicate model field"));
        }
    }
    for required_key in required {
        if !values.contains_key(*required_key) {
            return Err(decode_error(ErrorCode::MissingRequiredField, Some(node), "missing required model field"));
        }
    }
    Ok(values)
}

fn required<'a>(values: &BTreeMap<String, Node<'a>>, key: &str) -> ValidatorResult<Node<'a>> {
    values.get(key).copied().ok_or_else(|| error(ErrorCode::MissingRequiredField, "missing required model field"))
}

fn string(node: Node<'_>, limits: Limits) -> ValidatorResult<String> {
    let text = jmax_dom::scalar::string(node).map_err(ValidatorError::from)?;
    if text.len() > limits.max_string_bytes as usize {
        return Err(decode_error(ErrorCode::PayloadTooLarge, Some(node), "model string exceeds limit"));
    }
    Ok(text.to_owned())
}

fn number_bytes<'a>(node: Node<'a>) -> ValidatorResult<&'a [u8]> {
    match jmax_dom::scalar::value(node).map_err(ValidatorError::from)? {
        Scalar::Number(bytes) => Ok(bytes),
        _ => Err(decode_error(ErrorCode::TypeMismatch, Some(node), "expected integer token")),
    }
}

fn unsigned<T>(node: Node<'_>, maximum: u128, limits: Limits) -> ValidatorResult<T>
where T: TryFrom<u128> {
    let bytes = number_bytes(node)?;
    if bytes.len() > 1 && bytes[0] == b'-' && bytes[1..].iter().all(|digit| digit.is_ascii_digit()) {
        return Err(decode_error(ErrorCode::NumberOverflow, Some(node), "unsigned integer cannot be negative"));
    }
    if bytes.len() > limits.max_number_digits as usize || bytes.is_empty()
        || bytes.iter().any(|b| !b.is_ascii_digit())
    {
        return Err(decode_error(ErrorCode::InvalidNumber, Some(node), "expected unsigned integer token"));
    }
    let mut value = 0u128;
    for digit in bytes {
        value = value.checked_mul(10).and_then(|v| v.checked_add((*digit - b'0') as u128))
            .ok_or_else(|| decode_error(ErrorCode::NumberOverflow, Some(node), "integer overflow"))?;
        if value > maximum { return Err(decode_error(ErrorCode::NumberOverflow, Some(node), "integer out of range")); }
    }
    T::try_from(value).map_err(|_| decode_error(ErrorCode::NumberOverflow, Some(node), "integer out of range"))
}

fn signed_i32(node: Node<'_>, limits: Limits) -> ValidatorResult<i32> {
    let bytes = number_bytes(node)?;
    if bytes.len() > limits.max_number_digits as usize || bytes.is_empty() {
        return Err(decode_error(ErrorCode::InvalidNumber, Some(node), "expected signed integer token"));
    }
    let (negative, digits) = if bytes[0] == b'-' { (true, &bytes[1..]) } else { (false, bytes) };
    if digits.is_empty() || digits.iter().any(|b| !b.is_ascii_digit()) {
        return Err(decode_error(ErrorCode::InvalidNumber, Some(node), "expected signed integer token"));
    }
    let mut value = 0i64;
    for digit in digits {
        value = value.checked_mul(10).and_then(|v| v.checked_add((*digit - b'0') as i64))
            .ok_or_else(|| decode_error(ErrorCode::NumberOverflow, Some(node), "integer overflow"))?;
        let magnitude_limit = if negative { i32::MAX as i64 + 1 } else { i32::MAX as i64 };
        if value > magnitude_limit { return Err(decode_error(ErrorCode::NumberOverflow, Some(node), "integer out of range")); }
    }
    Ok(if negative { -(value as i64) as i32 } else { value as i32 })
}

fn parse_endpoint_type(node: Node<'_>, limits: Limits, depth: u16) -> ValidatorResult<EndpointType> {
    if depth > limits.max_depth { return Err(decode_error(ErrorCode::DepthLimitExceeded, Some(node), "model type nesting exceeds limit")); }
    if kind(node) == NodeKind::String {
        return Ok(match string(node, limits)?.as_str() {
            "UInt32" => EndpointType::UInt32,
            "Int32" => EndpointType::Int32,
            "Int64" => EndpointType::Int64,
            "Float64" => EndpointType::Float64,
            "Boolean" => EndpointType::Boolean,
            "String" => EndpointType::String,
            "Bytea" => EndpointType::Bytea,
            "UUID" => EndpointType::UUID,
            "IpAddr" => EndpointType::IpAddr,
            "Object" => EndpointType::Object,
            "Unit" => EndpointType::Unit,
            "TimeStampMs" => EndpointType::TimeStampMs,
            "BlockchainDecimal" => EndpointType::BlockchainDecimal,
            "BlockchainAddress" => EndpointType::BlockchainAddress,
            "BlockchainTransactionHash" => EndpointType::BlockchainTransactionHash,
            _ => return Err(decode_error(ErrorCode::InvalidToken, Some(node), "unknown endpoint type tag")),
        });
    }
    let outer = members(node)?;
    if outer.len() != 1 { return Err(decode_error(ErrorCode::TypeMismatch, Some(node), "endpoint type requires one tagged member")); }
    let (tag, body) = &outer[0];
    let nested = depth.saturating_add(1);
    match tag.as_str() {
        "NanoId" => {
            let map = fields(*body, &["len"], &["len"])?;
            Ok(EndpointType::NanoId { len: unsigned::<u8>(required(&map, "len")?, u8::MAX as u128, limits)? })
        }
        "StructRef" => Ok(EndpointType::StructRef(string(*body, limits)?)),
        "Vec" => Ok(EndpointType::Vec(Box::new(parse_endpoint_type(*body, limits, nested)?))),
        "Optional" => Ok(EndpointType::Optional(Box::new(parse_endpoint_type(*body, limits, nested)?))),
        "StructTable" => {
            let map = fields(*body, &["struct_ref"], &["struct_ref"])?;
            Ok(EndpointType::StructTable { struct_ref: string(required(&map, "struct_ref")?, limits)? })
        }
        "EnumRef" => {
            let map = fields(*body, &["name", "prefixed_name"], &["name", "prefixed_name"])?;
            let name = string(required(&map, "name")?, limits)?;
            let prefixed_name = boolean(required(&map, "prefixed_name")?)?;
            Ok(EndpointType::EnumRef { name, prefixed_name })
        }
        "Struct" => {
            let map = fields(*body, &["name", "fields"], &["name", "fields"])?;
            let name = string(required(&map, "name")?, limits)?;
            let fields = parse_endpoint_fields(required(&map, "fields")?, limits, nested)?;
            Ok(EndpointType::Struct { name, fields })
        }
        "Enum" => {
            let map = fields(*body, &["name", "variants"], &["name", "variants"])?;
            let name = string(required(&map, "name")?, limits)?;
            let variants_node = required(&map, "variants")?;
            let variants_array = array(variants_node)?;
            check_array_count(variants_array.len(), variants_node, limits)?;
            let mut variants = Vec::new();
            for variant_node in variants_array.iter() {
                let variant = fields(variant_node, &["name", "value", "description"], &["name", "value", "description"])?;
                variants.push(EnumVariantSpec {
                    name: string(required(&variant, "name")?, limits)?,
                    value: signed_i32(required(&variant, "value")?, limits)?,
                    description: string(required(&variant, "description")?, limits)?,
                });
            }
            Ok(EndpointType::Enum { name, variants })
        }
        _ => Err(decode_error(ErrorCode::InvalidToken, Some(node), "unknown endpoint type tag")),
    }
}

fn boolean(node: Node<'_>) -> ValidatorResult<bool> {
    match jmax_dom::scalar::value(node).map_err(ValidatorError::from)? {
        Scalar::Bool(value) => Ok(value),
        _ => Err(decode_error(ErrorCode::TypeMismatch, Some(node), "expected boolean")),
    }
}

fn check_array_count(count: usize, node: Node<'_>, limits: Limits) -> ValidatorResult<()> {
    if count > limits.max_array_items as usize {
        Err(decode_error(ErrorCode::PayloadTooLarge, Some(node), "model array exceeds item limit"))
    } else { Ok(()) }
}

fn parse_endpoint_fields(node: Node<'_>, limits: Limits, depth: u16) -> ValidatorResult<Vec<EndpointField>> {
    let values = array(node)?;
    check_array_count(values.len(), node, limits)?;
    let mut result = Vec::new();
    for value in values.iter() {
        let map = fields(value, &["name", "ty", "description"], &["name", "ty", "description"])?;
        result.push(EndpointField {
            name: string(required(&map, "name")?, limits)?,
            ty: parse_endpoint_type(required(&map, "ty")?, limits, depth.saturating_add(1))?,
            description: string(required(&map, "description")?, limits)?,
        });
    }
    Ok(result)
}

fn parse_endpoint(node: Node<'_>, service_name: &str, limits: Limits) -> ValidatorResult<EndpointSchemaSpec> {
    let map = fields(node, &["code", "name", "service", "roles", "params", "response", "stream_response", "description"],
        &["code", "name", "service", "roles", "params", "response", "stream_response", "description"])?;
    let roles_node = required(&map, "roles")?;
    let role_nodes = array(roles_node)?;
    check_array_count(role_nodes.len(), roles_node, limits)?;
    let mut roles = Vec::new();
    for role in role_nodes.iter() { roles.push(string(role, limits)?); }
    let stream_node = required(&map, "stream_response")?;
    let stream_response = if kind(stream_node) == NodeKind::Null { None } else { Some(parse_endpoint_type(stream_node, limits, 0)?) };
    let service = string(required(&map, "service")?, limits)?;
    if service != service_name { return Err(decode_error(ErrorCode::ConstraintViolation, Some(required(&map, "service")?), "endpoint service does not match containing service")); }
    Ok(EndpointSchemaSpec {
        code: unsigned(required(&map, "code")?, u32::MAX as u128, limits)?,
        name: string(required(&map, "name")?, limits)?,
        service,
        roles,
        params: parse_endpoint_fields(required(&map, "params")?, limits, 0)?,
        response: parse_endpoint_fields(required(&map, "response")?, limits, 0)?,
        stream_response,
        description: string(required(&map, "description")?, limits)?,
    })
}

fn parse_service(node: Node<'_>, limits: Limits) -> ValidatorResult<ServiceSpec> {
    let map = fields(node, &["id", "name", "endpoints"], &["id", "name", "endpoints"])?;
    let name = string(required(&map, "name")?, limits)?;
    let endpoints_node = required(&map, "endpoints")?;
    let endpoint_nodes = array(endpoints_node)?;
    check_array_count(endpoint_nodes.len(), endpoints_node, limits)?;
    let mut endpoints = Vec::new();
    for endpoint in endpoint_nodes.iter() { endpoints.push(parse_endpoint(endpoint, &name, limits)?); }
    Ok(ServiceSpec { id: unsigned(required(&map, "id")?, u16::MAX as u128, limits)?, name, endpoints })
}

fn parse_preset(node: Node<'_>, limits: Limits) -> ValidatorResult<Preset> {
    let map = fields(node, &["name", "endpoint", "parameters"], &["name", "endpoint", "parameters"])?;
    let endpoint_map = fields(required(&map, "endpoint")?, &["service_id", "code"], &["service_id", "code"])?;
    let parameter_node = required(&map, "parameters")?;
    let parameter_members = members(parameter_node)?;
    if parameter_members.len() > limits.max_fields_per_object as usize {
        return Err(decode_error(ErrorCode::PayloadTooLarge, Some(parameter_node), "preset parameter count exceeds limit"));
    }
    let mut parameters = BTreeMap::new();
    for (key, value) in parameter_members {
        if key.len() > limits.max_string_bytes as usize { return Err(decode_error(ErrorCode::PayloadTooLarge, Some(value), "parameter name exceeds limit")); }
        if parameters.insert(key, string(value, limits)?).is_some() {
            return Err(decode_error(ErrorCode::DuplicateObjectKey, Some(value), "duplicate preset parameter"));
        }
    }
    Ok(Preset {
        name: string(required(&map, "name")?, limits)?,
        endpoint: EndpointKey {
            service_id: unsigned(required(&endpoint_map, "service_id")?, u16::MAX as u128, limits)?,
            code: unsigned(required(&endpoint_map, "code")?, u32::MAX as u128, limits)?,
        },
        parameters,
    })
}

pub fn parse_model(payload: Payload, limits: Limits) -> ValidatorResult<ValidatorModel> {
    if payload.len() > limits.max_payload_bytes as usize || payload.len() > u32::MAX as usize {
        return Err(error(ErrorCode::PayloadTooLarge, "model payload exceeds limit"));
    }
    std::str::from_utf8(payload.as_slice()).map_err(|_| error(ErrorCode::InvalidUtf8, "model input must be UTF-8"))?;
    let mut strict = limits;
    strict.reject_duplicate_keys = true;
    strict.reject_unknown_keys = true;
    let document = jmax_dom::build::parse(payload, strict).map_err(ValidatorError::from)?;
    let root = jmax_dom::navigate::root(&document);
    let root_fields = fields(root, &["version", "services", "presets"], &["version", "services"])?;
    let version = unsigned::<u16>(required(&root_fields, "version")?, u16::MAX as u128, limits)?;
    if version != MODEL_VERSION { return Err(decode_error(ErrorCode::ConstraintViolation, root_fields.get("version").copied(), "unsupported model version")); }
    let services_node = required(&root_fields, "services")?;
    let service_nodes = array(services_node)?;
    check_array_count(service_nodes.len(), services_node, limits)?;
    let mut services = Vec::new();
    for service in service_nodes.iter() { services.push(parse_service(service, limits)?); }
    let mut presets = Vec::new();
    if let Some(presets_node) = root_fields.get("presets").copied() {
        let preset_nodes = array(presets_node)?;
        check_array_count(preset_nodes.len(), presets_node, limits)?;
        for preset in preset_nodes.iter() { presets.push(parse_preset(preset, limits)?); }
    }
    let model = ValidatorModel { services, presets };
    validate_model(&model, limits)?;
    Ok(model)
}

pub fn load_model(path: &Path, limits: Limits) -> ValidatorResult<ValidatorModel> {
    let maximum = u64::from(limits.max_payload_bytes).min(super::MAX_MODEL_BYTES);
    let capacity = usize::try_from(maximum).unwrap_or(usize::MAX);
    let mut file = File::open(path).map_err(|source| ValidatorError::Io { operation: "open model", path: path.to_path_buf(), source })?;
    let mut bytes = Vec::new();
    let reserve = capacity.min(1024 * 1024);
    bytes.try_reserve(reserve).map_err(|_| error(ErrorCode::BudgetExhausted, "unable to allocate model input"))?;
    let mut buffer = [0u8; 8192];
    while (bytes.len() as u64) < maximum {
        let remaining = (maximum - bytes.len() as u64).min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..remaining]).map_err(|source| ValidatorError::Io { operation: "read model", path: path.to_path_buf(), source })?;
        if read == 0 { break; }
        bytes.try_reserve(read).map_err(|_| error(ErrorCode::BudgetExhausted, "unable to allocate model input"))?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    if (bytes.len() as u64) == maximum {
        let mut extra = [0u8; 1];
        let read = file.read(&mut extra).map_err(|source| ValidatorError::Io { operation: "read model", path: path.to_path_buf(), source })?;
        if read != 0 { return Err(error(ErrorCode::PayloadTooLarge, "model file exceeds byte limit")); }
    }
    parse_model(Payload::from_vec(bytes), limits)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NamedDefinition { Struct(Vec<EndpointField>), Enum(Vec<EnumVariantSpec>) }

fn collect_definitions(ty: &EndpointType, definitions: &mut BTreeMap<String, NamedDefinition>, limits: Limits, depth: u16) -> ValidatorResult<()> {
    if depth > limits.max_depth { return Err(error(ErrorCode::DepthLimitExceeded, "endpoint type nesting exceeds limit")); }
    match ty {
        EndpointType::Struct { name, fields } => {
            insert_definition(definitions, name, NamedDefinition::Struct(fields.clone()))?;
            for field in fields { collect_definitions(&field.ty, definitions, limits, depth.saturating_add(1))?; }
        }
        EndpointType::Enum { name, variants } => insert_definition(definitions, name, NamedDefinition::Enum(variants.clone()))?,
        EndpointType::Vec(inner) | EndpointType::Optional(inner) => collect_definitions(inner, definitions, limits, depth.saturating_add(1))?,
        _ => {}
    }
    Ok(())
}

fn insert_definition(definitions: &mut BTreeMap<String, NamedDefinition>, name: &str, definition: NamedDefinition) -> ValidatorResult<()> {
    if let Some(existing) = definitions.get(name) {
        if existing != &definition { return Err(error(ErrorCode::ConstraintViolation, "named definition is inconsistent")); }
    } else { definitions.insert(name.to_owned(), definition); }
    Ok(())
}

fn validate_fields(fields: &[EndpointField], limits: Limits, depth: u16) -> ValidatorResult<()> {
    if fields.len() > limits.max_fields_per_object as usize { return Err(error(ErrorCode::PayloadTooLarge, "field count exceeds limit")); }
    let mut names = BTreeSet::new();
    for field in fields {
        if field.name.is_empty() || field.name.len() > limits.max_string_bytes as usize {
            return Err(error(ErrorCode::ConstraintViolation, "field name must be nonempty and within limits"));
        }
        if !names.insert(field.name.as_str()) { return Err(error(ErrorCode::DuplicateObjectKey, "duplicate field name")); }
        if field.description.len() > limits.max_string_bytes as usize { return Err(error(ErrorCode::PayloadTooLarge, "field description exceeds limit")); }
        validate_type(&field.ty, limits, depth.saturating_add(1))?;
    }
    Ok(())
}

fn validate_type(ty: &EndpointType, limits: Limits, depth: u16) -> ValidatorResult<()> {
    if depth > limits.max_depth { return Err(error(ErrorCode::DepthLimitExceeded, "endpoint type nesting exceeds limit")); }
    match ty {
        EndpointType::NanoId { len: 0 } => Err(error(ErrorCode::ConstraintViolation, "NanoId length must be positive")),
        EndpointType::NanoId { .. } => Ok(()),
        EndpointType::Struct { name, fields } => {
            if name.is_empty() || name.len() > limits.max_string_bytes as usize { return Err(error(ErrorCode::ConstraintViolation, "struct name must be nonempty and within limits")); }
            validate_fields(fields, limits, depth)
        }
        EndpointType::Enum { name, variants } => {
            if name.is_empty() || name.len() > limits.max_string_bytes as usize { return Err(error(ErrorCode::ConstraintViolation, "enum name must be nonempty and within limits")); }
            let mut names = BTreeSet::new(); let mut values = BTreeSet::new();
            for variant in variants {
                if variant.name.is_empty() || variant.name.len() > limits.max_string_bytes as usize || !names.insert(variant.name.as_str()) || !values.insert(variant.value) {
                    return Err(error(ErrorCode::ConstraintViolation, "enum variants must have unique nonempty names and values"));
                }
                if variant.description.len() > limits.max_string_bytes as usize { return Err(error(ErrorCode::PayloadTooLarge, "enum variant description exceeds limit")); }
            }
            Ok(())
        }
        EndpointType::StructRef(name) | EndpointType::EnumRef { name, .. } => if name.is_empty() || name.len() > limits.max_string_bytes as usize { Err(error(ErrorCode::ConstraintViolation, "reference name must be nonempty and within limits")) } else { Ok(()) },
        EndpointType::StructTable { struct_ref } => if struct_ref.is_empty() || struct_ref.len() > limits.max_string_bytes as usize { Err(error(ErrorCode::ConstraintViolation, "struct reference must be nonempty and within limits")) } else { Ok(()) },
        EndpointType::Vec(inner) | EndpointType::Optional(inner) => validate_type(inner, limits, depth.saturating_add(1)),
        _ => Ok(()),
    }
}

fn validate_references(ty: &EndpointType, definitions: &BTreeMap<String, NamedDefinition>) -> ValidatorResult<()> {
    match ty {
        EndpointType::StructRef(name) | EndpointType::StructTable { struct_ref: name } => match definitions.get(name) {
            Some(NamedDefinition::Struct(_)) => Ok(()),
            _ => Err(error(ErrorCode::MissingRequiredField, "unresolved struct reference")),
        },
        EndpointType::EnumRef { name, .. } => match definitions.get(name) {
            Some(NamedDefinition::Enum(_)) => Ok(()),
            _ => Err(error(ErrorCode::MissingRequiredField, "unresolved enum reference")),
        },
        EndpointType::Struct { fields, .. } => { for field in fields { validate_references(&field.ty, definitions)?; } Ok(()) }
        EndpointType::Vec(inner) | EndpointType::Optional(inner) => validate_references(inner, definitions),
        _ => Ok(()),
    }
}

pub fn validate_model(model: &ValidatorModel, limits: Limits) -> ValidatorResult<()> {
    if model.services.len() > limits.max_array_items as usize || model.presets.len() > limits.max_array_items as usize {
        return Err(error(ErrorCode::PayloadTooLarge, "model item count exceeds limit"));
    }
    let mut service_ids = BTreeSet::new(); let mut service_names = BTreeSet::new();
    let mut definitions = BTreeMap::new();
    for service in &model.services {
        if service.endpoints.len() > limits.max_array_items as usize { return Err(error(ErrorCode::PayloadTooLarge, "endpoint count exceeds limit")); }
        if service.name.is_empty() || service.name.len() > limits.max_string_bytes as usize {
            return Err(error(ErrorCode::ConstraintViolation, "service name must be nonempty and within limits"));
        }
        if !service_ids.insert(service.id) || !service_names.insert(service.name.as_str()) {
            return Err(error(ErrorCode::DuplicateObjectKey, "service identifiers and names must be unique and nonempty"));
        }
        let mut codes = BTreeSet::new(); let mut endpoint_names = BTreeSet::new();
        for endpoint in &service.endpoints {
            if endpoint.name.is_empty() || endpoint.name.len() > limits.max_string_bytes as usize || endpoint.service.len() > limits.max_string_bytes as usize || endpoint.description.len() > limits.max_string_bytes as usize {
                return Err(error(ErrorCode::ConstraintViolation, "endpoint text fields exceed model requirements"));
            }
            if !codes.insert(endpoint.code) || !endpoint_names.insert(endpoint.name.as_str()) {
                return Err(error(ErrorCode::DuplicateObjectKey, "endpoint identifiers and names must be unique and nonempty"));
            }
            if endpoint.service != service.name { return Err(error(ErrorCode::ConstraintViolation, "endpoint service does not match containing service")); }
            let mut roles = BTreeSet::new();
            if endpoint.roles.len() > limits.max_array_items as usize { return Err(error(ErrorCode::PayloadTooLarge, "role count exceeds limit")); }
            for role in &endpoint.roles {
                if role.is_empty() || role.len() > limits.max_string_bytes as usize || !roles.insert(role.as_str()) {
                    return Err(error(ErrorCode::DuplicateObjectKey, "endpoint roles must be unique and nonempty"));
                }
            }
            validate_fields(&endpoint.params, limits, 0)?;
            validate_fields(&endpoint.response, limits, 0)?;
            if let Some(stream) = &endpoint.stream_response { validate_type(stream, limits, 0)?; }
            for field in endpoint.params.iter().chain(endpoint.response.iter()) { collect_definitions(&field.ty, &mut definitions, limits, 0)?; }
            if let Some(stream) = &endpoint.stream_response { collect_definitions(stream, &mut definitions, limits, 0)?; }
        }
    }
    for service in &model.services {
        for endpoint in &service.endpoints {
            for field in endpoint.params.iter().chain(endpoint.response.iter()) { validate_references(&field.ty, &definitions)?; }
            if let Some(stream) = &endpoint.stream_response { validate_references(stream, &definitions)?; }
        }
    }
    let mut preset_names = BTreeSet::new();
    for preset in &model.presets {
        if preset.name.is_empty() || preset.name.len() > limits.max_string_bytes as usize || !preset_names.insert(preset.name.as_str()) {
            return Err(error(ErrorCode::DuplicateObjectKey, "preset names must be unique and nonempty"));
        }
        let endpoint = model.endpoint(preset.endpoint).ok_or_else(|| error(ErrorCode::MissingRequiredField, "preset endpoint is missing or ambiguous"))?;
        for (name, input) in &preset.parameters {
            let field = endpoint.params.iter().find(|field| field.name == *name)
                .ok_or_else(|| error(ErrorCode::UnknownObjectKey, "preset parameter is not a declared input"))?;
            crate::params::validate_input(model, field, input, limits)?;
        }
    }
    Ok(())
}

pub fn select_endpoint(model: &ValidatorModel, service: &str, endpoint: &str) -> ValidatorResult<EndpointKey> {
    let mut matches = model.services.iter().filter(|item| item.name == service);
    let service_spec = matches.next().ok_or_else(|| error(ErrorCode::MissingRequiredField, "service not found"))?;
    if matches.next().is_some() { return Err(error(ErrorCode::DuplicateObjectKey, "service name is ambiguous")); }
    let mut endpoint_matches = service_spec.endpoints.iter().filter(|item| item.name == endpoint);
    let selected = endpoint_matches.next().ok_or_else(|| error(ErrorCode::MissingRequiredField, "endpoint not found"))?;
    if endpoint_matches.next().is_some() { return Err(error(ErrorCode::DuplicateObjectKey, "endpoint name is ambiguous")); }
    Ok(EndpointKey { service_id: service_spec.id, code: selected.code })
}

pub fn select_preset<'a>(model: &'a ValidatorModel, name: &str) -> ValidatorResult<&'a Preset> {
    let mut matches = model.presets.iter().filter(|preset| preset.name == name);
    let preset = matches.next().ok_or_else(|| error(ErrorCode::MissingRequiredField, "preset not found"))?;
    if matches.next().is_some() { return Err(error(ErrorCode::DuplicateObjectKey, "preset name is ambiguous")); }
    Ok(preset)
}
