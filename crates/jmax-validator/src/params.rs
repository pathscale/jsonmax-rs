//! Grammar-aware endpoint parameter validation and preparation.

use super::*;
use std::{net::IpAddr, str};
use jmax_dom::{Array, Node, NodeKind, Object, Scalar};

fn err(code: ErrorCode, node: Option<Node<'_>>, detail: &'static str) -> ValidatorError {
    let offset = node.map(|n| jmax_dom::navigate::raw_span(n).offset).unwrap_or(0);
    ValidatorError::Validation(AbiError::new(code, ErrorSource::Decode, offset, detail))
}
fn decode_error(error: AbiError) -> ValidatorError {
    ValidatorError::Validation(AbiError { source: ErrorSource::Decode, ..error })
}

fn kind(node: Node<'_>) -> NodeKind { jmax_dom::navigate::kind(node) }
fn object<'a>(node: Node<'a>) -> ValidatorResult<Object<'a>> { jmax_dom::navigate::object(node).map_err(ValidatorError::from) }
fn array<'a>(node: Node<'a>) -> ValidatorResult<Array<'a>> { jmax_dom::navigate::array(node).map_err(ValidatorError::from) }
fn members<'a>(node: Node<'a>) -> ValidatorResult<Vec<(String, Node<'a>)>> {
    Ok(object(node)?.iter().map(|m| (m.key.to_owned(), m.value)).collect())
}
fn items<'a>(node: Node<'a>) -> ValidatorResult<Vec<Node<'a>>> { Ok(array(node)?.iter().collect()) }
fn text<'a>(node: Node<'a>) -> ValidatorResult<&'a str> {
    match jmax_dom::scalar::value(node).map_err(ValidatorError::from)? {
        Scalar::String(value) => Ok(value),
        _ => Err(err(ErrorCode::TypeMismatch, Some(node), "expected JSON string")),
    }
}
fn ensure_text_limit(value: &str, limits: Limits, node: Option<Node<'_>>) -> ValidatorResult<()> {
    if value.len() > limits.max_string_bytes as usize {
        Err(err(ErrorCode::PayloadTooLarge, node, "string exceeds configured limit"))
    } else { Ok(()) }
}

/// Selects the grammar shown to the user for an endpoint type.
pub fn grammar_for(ty: &EndpointType) -> InputGrammar {
    match ty {
        EndpointType::String | EndpointType::Bytea | EndpointType::UUID | EndpointType::NanoId { .. }
        | EndpointType::IpAddr | EndpointType::BlockchainDecimal | EndpointType::BlockchainAddress
        | EndpointType::BlockchainTransactionHash => InputGrammar::Text,
        _ => InputGrammar::Json,
    }
}

fn find_struct<'a>(model: &'a ValidatorModel, name: &str) -> ValidatorResult<&'a [EndpointField]> {
    let mut found: Option<&[EndpointField]> = None;
    for service in &model.services {
        for endpoint in &service.endpoints {
            for field in endpoint.params.iter().chain(endpoint.response.iter()) {
                find_struct_type(&field.ty, name, &mut found)?;
            }
            if let Some(ty) = endpoint.stream_response.as_ref() { find_struct_type(ty, name, &mut found)?; }
        }
    }
    found.ok_or_else(|| err(ErrorCode::MissingRequiredField, None, "unknown struct reference"))
}

fn find_struct_type<'a>(ty: &'a EndpointType, name: &str, found: &mut Option<&'a [EndpointField]>) -> ValidatorResult<()> {
    match ty {
        EndpointType::Struct { name: n, fields } => {
            if n == name {
                if let Some(previous) = *found {
                    if previous != fields.as_slice() { return Err(err(ErrorCode::ConstraintViolation, None, "inconsistent struct definition")); }
                } else { *found = Some(fields); }
            }
            for field in fields { find_struct_type(&field.ty, name, found)?; }
        }
        EndpointType::Vec(inner) | EndpointType::Optional(inner) => find_struct_type(inner, name, found)?,
        _ => {}
    }
    Ok(())
}

fn find_enum<'a>(model: &'a ValidatorModel, name: &str) -> ValidatorResult<&'a [jmax_abi::endpoint::EnumVariantSpec]> {
    let mut found = None;
    for service in &model.services {
        for endpoint in &service.endpoints {
            for field in endpoint.params.iter().chain(endpoint.response.iter()) { find_enum_type(&field.ty, name, &mut found)?; }
            if let Some(ty) = endpoint.stream_response.as_ref() { find_enum_type(ty, name, &mut found)?; }
        }
    }
    found.ok_or_else(|| err(ErrorCode::MissingRequiredField, None, "unknown enum reference"))
}

fn find_enum_type<'a>(ty: &'a EndpointType, name: &str, found: &mut Option<&'a [jmax_abi::endpoint::EnumVariantSpec]>) -> ValidatorResult<()> {
    match ty {
        EndpointType::Enum { name: n, variants } => {
            if n == name {
                if let Some(previous) = *found {
                    if previous != variants.as_slice() { return Err(err(ErrorCode::ConstraintViolation, None, "inconsistent enum definition")); }
                } else { *found = Some(variants); }
            }
        }
        EndpointType::Struct { fields, .. } => for field in fields { find_enum_type(&field.ty, name, found)?; },
        EndpointType::Vec(inner) | EndpointType::Optional(inner) => find_enum_type(inner, name, found)?,
        _ => {}
    }
    Ok(())
}

fn check_depth(depth: u16, limits: Limits, node: Node<'_>) -> ValidatorResult<()> {
    if depth > limits.max_depth || depth == u16::MAX { Err(err(ErrorCode::DepthLimitExceeded, Some(node), "parameter nesting exceeds limit")) } else { Ok(()) }
}
fn scalar_number<'a>(node: Node<'a>) -> ValidatorResult<&'a [u8]> {
    match jmax_dom::scalar::value(node).map_err(ValidatorError::from)? {
        Scalar::Number(bytes) => Ok(bytes),
        _ => Err(err(ErrorCode::TypeMismatch, Some(node), "expected number")),
    }
}
fn integer_parts<'a>(bytes: &'a [u8], limits: Limits, node: Node<'_>) -> ValidatorResult<(bool, &'a [u8])> {
    if bytes.len() > limits.max_number_digits as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "number digit count exceeds limit")); }
    let (negative, digits) = if bytes.first() == Some(&b'-') { (true, &bytes[1..]) } else { (false, bytes) };
    if digits.is_empty() || digits.iter().any(|b| !b.is_ascii_digit()) {
        return Err(err(ErrorCode::InvalidNumber, Some(node), "expected an integer token"));
    }
    Ok((negative, digits))
}
fn check_unsigned(bytes: &[u8], max: u64, limits: Limits, node: Node<'_>) -> ValidatorResult<()> {
    let (negative, digits) = integer_parts(bytes, limits, node)?;
    if negative { return Err(err(ErrorCode::NumberOverflow, Some(node), "unsigned integer cannot be negative")); }
    let mut value = 0u64;
    for digit in digits {
        value = value.checked_mul(10).and_then(|v| v.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| err(ErrorCode::NumberOverflow, Some(node), "integer overflow"))?;
        if value > max { return Err(err(ErrorCode::NumberOverflow, Some(node), "integer out of range")); }
    }
    Ok(())
}
fn check_signed(bytes: &[u8], min: i64, max: i64, limits: Limits, node: Node<'_>) -> ValidatorResult<()> {
    let (negative, digits) = integer_parts(bytes, limits, node)?;
    let bound = if negative { min.unsigned_abs() } else { max as u64 };
    let mut value = 0u64;
    for digit in digits {
        value = value.checked_mul(10).and_then(|v| v.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| err(ErrorCode::NumberOverflow, Some(node), "integer overflow"))?;
        if value > bound { return Err(err(ErrorCode::NumberOverflow, Some(node), "integer out of range")); }
    }
    Ok(())
}

fn validate_any(node: Node<'_>, limits: Limits, depth: u16) -> ValidatorResult<()> {
    check_depth(depth, limits, node)?;
    match kind(node) {
        NodeKind::String => ensure_text_limit(text(node)?, limits, Some(node)),
        NodeKind::Number => {
            let b = scalar_number(node)?;
            if b.len() > limits.max_number_digits as usize { Err(err(ErrorCode::PayloadTooLarge, Some(node), "number digit count exceeds limit")) } else { Ok(()) }
        }
        NodeKind::Array => {
            let children = items(node)?;
            if children.len() > limits.max_array_items as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "array item count exceeds limit")); }
            for child in children { validate_any(child, limits, depth.saturating_add(1))?; }
            Ok(())
        }
        NodeKind::Object => {
            let fields = members(node)?;
            if fields.len() > limits.max_fields_per_object as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "object field count exceeds limit")); }
            for (key, value) in fields {
                ensure_text_limit(&key, limits, Some(value))?;
                validate_any(value, limits, depth.saturating_add(1))?;
            }
            Ok(())
        }
        NodeKind::Null | NodeKind::Bool => Ok(()),
    }
}

fn validate_object_fields(model: &ValidatorModel, node: Node<'_>, fields: &[EndpointField], limits: Limits, depth: u16) -> ValidatorResult<()> {
    check_depth(depth, limits, node)?;
    let values = members(node)?;
    if values.len() > limits.max_fields_per_object as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "object field count exceeds limit")); }
    let mut seen = std::collections::BTreeSet::new();
    for (name, value) in &values {
        ensure_text_limit(name, limits, Some(*value))?;
        if !seen.insert(name.as_str()) { return Err(err(ErrorCode::DuplicateObjectKey, Some(*value), "duplicate object field")); }
        let field = fields.iter().find(|field| field.name == *name)
            .ok_or_else(|| err(ErrorCode::UnknownObjectKey, Some(*value), "unknown object field"))?;
        validate_node(model, *value, &field.ty, limits, depth.saturating_add(1))?;
    }
    for field in fields {
        if !seen.contains(field.name.as_str()) && !matches!(&field.ty, EndpointType::Optional(_)) {
            return Err(err(ErrorCode::MissingRequiredField, Some(node), "missing required object field"));
        }
    }
    Ok(())
}

fn validate_node(model: &ValidatorModel, node: Node<'_>, ty: &EndpointType, limits: Limits, depth: u16) -> ValidatorResult<()> {
    check_depth(depth, limits, node)?;
    match ty {
        EndpointType::Optional(inner) => if kind(node) == NodeKind::Null { Ok(()) } else { validate_node(model, node, inner, limits, depth.saturating_add(1)) },
        EndpointType::UInt32 => check_unsigned(scalar_number(node)?, u32::MAX as u64, limits, node),
        EndpointType::Int32 => check_signed(scalar_number(node)?, i32::MIN as i64, i32::MAX as i64, limits, node),
        EndpointType::Int64 => check_signed(scalar_number(node)?, i64::MIN, i64::MAX, limits, node),
        EndpointType::TimeStampMs => check_unsigned(scalar_number(node)?, i64::MAX as u64, limits, node),
        EndpointType::Float64 => {
            let bytes = scalar_number(node)?;
            if bytes.len() > limits.max_number_digits as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "number digit count exceeds limit")); }
            let s = str::from_utf8(bytes).map_err(|_| err(ErrorCode::InvalidNumber, Some(node), "invalid number"))?;
            match s.parse::<f64>() { Ok(v) if v.is_finite() => Ok(()), Ok(_) => Err(err(ErrorCode::NumberOverflow, Some(node), "floating point value is not finite")), Err(_) => Err(err(ErrorCode::InvalidNumber, Some(node), "invalid floating point number")) }
        }
        EndpointType::Boolean => if kind(node) == NodeKind::Bool { Ok(()) } else { Err(err(ErrorCode::TypeMismatch, Some(node), "expected boolean")) },
        EndpointType::String => { let s = text(node)?; ensure_text_limit(s, limits, Some(node)) }
        EndpointType::Bytea => { let s = text(node)?; ensure_text_limit(s, limits, Some(node))?; validate_base64(s, node) }
        EndpointType::UUID => { let s = text(node)?; ensure_text_limit(s, limits, Some(node))?; validate_uuid(s, node) }
        EndpointType::NanoId { len } => {
            let s = text(node)?; ensure_text_limit(s, limits, Some(node))?;
            if s.len() != usize::from(*len) || !s.bytes().all(is_nanoid_byte) { Err(err(ErrorCode::FormatMismatch, Some(node), "invalid NanoId")) } else { Ok(()) }
        }
        EndpointType::IpAddr => { let s = text(node)?; ensure_text_limit(s, limits, Some(node))?; if s.parse::<IpAddr>().is_ok() { Ok(()) } else { Err(err(ErrorCode::FormatMismatch, Some(node), "invalid IP address")) } }
        EndpointType::BlockchainAddress => { let s = text(node)?; ensure_text_limit(s, limits, Some(node))?; validate_hex_prefixed(s, 40, node, "invalid blockchain address") }
        EndpointType::BlockchainTransactionHash => { let s = text(node)?; ensure_text_limit(s, limits, Some(node))?; validate_hex_prefixed(s, 64, node, "invalid transaction hash") }
        EndpointType::BlockchainDecimal => { let s = text(node)?; ensure_text_limit(s, limits, Some(node))?; validate_decimal(s, limits, node) }
        EndpointType::Unit => if kind(node) == NodeKind::Null { Ok(()) } else { Err(err(ErrorCode::TypeMismatch, Some(node), "expected null")) },
        EndpointType::Object => if kind(node) == NodeKind::Object { validate_any(node, limits, depth) } else { Err(err(ErrorCode::TypeMismatch, Some(node), "expected object")) },
        EndpointType::Vec(inner) => {
            if kind(node) != NodeKind::Array { return Err(err(ErrorCode::TypeMismatch, Some(node), "expected array")); }
            let values = items(node)?;
            if values.len() > limits.max_array_items as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "array item count exceeds limit")); }
            for value in values { validate_node(model, value, inner, limits, depth.saturating_add(1))?; }
            Ok(())
        }
        EndpointType::Struct { fields, .. } => if kind(node) == NodeKind::Object { validate_object_fields(model, node, fields, limits, depth) } else { Err(err(ErrorCode::TypeMismatch, Some(node), "expected struct object")) },
        EndpointType::StructRef(name) => {
            if kind(node) != NodeKind::Object { return Err(err(ErrorCode::TypeMismatch, Some(node), "expected struct object")); }
            let fields = find_struct(model, name)?;
            validate_object_fields(model, node, fields, limits, depth)
        }
        EndpointType::StructTable { struct_ref } => {
            if kind(node) != NodeKind::Array { return Err(err(ErrorCode::TypeMismatch, Some(node), "expected struct table array")); }
            let values = items(node)?;
            if values.len() > limits.max_array_items as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "array item count exceeds limit")); }
            let fields = find_struct(model, struct_ref)?;
            for value in values { validate_object_fields(model, value, fields, limits, depth.saturating_add(1))?; }
            Ok(())
        }
        EndpointType::Enum { name, variants } => validate_enum(node, name, false, variants, limits),
        EndpointType::EnumRef { name, prefixed_name } => validate_enum(node, name, *prefixed_name, find_enum(model, name)?, limits),
    }
}

fn validate_enum(node: Node<'_>, name: &str, prefixed: bool, variants: &[jmax_abi::endpoint::EnumVariantSpec], limits: Limits) -> ValidatorResult<()> {
    let value = text(node)?;
    ensure_text_limit(value, limits, Some(node))?;
    let found = variants.iter().any(|variant| {
        if prefixed { value == format!("{name}::{}", variant.name) } else { value == variant.name }
    });
    if found { Ok(()) } else { Err(err(ErrorCode::ConstraintViolation, Some(node), "unknown enum variant")) }
}

fn is_nanoid_byte(b: u8) -> bool { b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-') }
fn validate_hex_prefixed(s: &str, digits: usize, node: Node<'_>, detail: &'static str) -> ValidatorResult<()> {
    if s.len() == digits + 2 && s.starts_with("0x") && s.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit) { Ok(()) }
    else { Err(err(ErrorCode::FormatMismatch, Some(node), detail)) }
}
fn validate_uuid(s: &str, node: Node<'_>) -> ValidatorResult<()> {
    let b = s.as_bytes();
    if b.len() == 36 && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
        && b.iter().enumerate().all(|(i, c)| [8,13,18,23].contains(&i) || c.is_ascii_hexdigit()) { Ok(()) }
    else { Err(err(ErrorCode::FormatMismatch, Some(node), "invalid UUID")) }
}
fn validate_base64(s: &str, node: Node<'_>) -> ValidatorResult<()> {
    let b = s.as_bytes();
    if b.len() % 4 != 0 { return Err(err(ErrorCode::FormatMismatch, Some(node), "base64 must use padded RFC 4648 form")); }
    let pad = if b.ends_with(b"==") { 2 } else if b.ends_with(b"=") { 1 } else { 0 };
    if b[..b.len().saturating_sub(pad)].iter().any(|c| !c.is_ascii_alphanumeric() && !matches!(c, b'+' | b'/'))
        || b[b.len().saturating_sub(pad)..].iter().any(|c| *c != b'=')
        || (pad == 2 && b.len() >= 4 && (base64_value(b[b.len()-3]) & 0x0f) != 0)
        || (pad == 1 && b.len() >= 4 && (base64_value(b[b.len()-2]) & 0x03) != 0) {
        return Err(err(ErrorCode::FormatMismatch, Some(node), "invalid canonical base64"));
    }
    Ok(())
}
fn base64_value(c: u8) -> u8 { match c { b'A'..=b'Z' => c-b'A', b'a'..=b'z' => c-b'a'+26, b'0'..=b'9' => c-b'0'+52, b'+' => 62, b'/' => 63, _ => 0 } }
fn validate_decimal(s: &str, limits: Limits, node: Node<'_>) -> ValidatorResult<()> {
    let b = s.as_bytes();
    if b.is_empty() { return Err(err(ErrorCode::FormatMismatch, Some(node), "invalid decimal text")); }
    let digits_start = usize::from(b[0] == b'-');
    if digits_start == b.len() { return Err(err(ErrorCode::FormatMismatch, Some(node), "invalid decimal text")); }
    let mut dot = None;
    let mut digits = 0usize;
    let mut scale = 0usize;
    let mut negative_coefficient = 0i128;
    for (i, c) in b.iter().copied().enumerate().skip(digits_start) {
        if c == b'.' && dot.is_none() { dot = Some(i); continue; }
        if !c.is_ascii_digit() { return Err(err(ErrorCode::FormatMismatch, Some(node), "invalid decimal text")); }
        digits += 1;
        if dot.is_some() { scale += 1; }
        if digits > limits.max_number_digits as usize { return Err(err(ErrorCode::PayloadTooLarge, Some(node), "decimal digit count exceeds limit")); }
        negative_coefficient = negative_coefficient.checked_mul(10).and_then(|v| v.checked_sub(i128::from(c - b'0')))
            .ok_or_else(|| err(ErrorCode::NumberOverflow, Some(node), "decimal coefficient exceeds i128"))?;
    }
    if digits == 0 || dot == Some(digits_start) || dot == Some(b.len() - 1) { return Err(err(ErrorCode::FormatMismatch, Some(node), "invalid decimal text")); }
    if scale > 38 { return Err(err(ErrorCode::DecimalScaleMismatch, Some(node), "decimal scale exceeds 38")); }
    if b.len() > 1 && b[digits_start] == b'0' && b.get(digits_start + 1).copied().is_some_and(|x| x.is_ascii_digit()) {
        return Err(err(ErrorCode::FormatMismatch, Some(node), "decimal has a leading zero"));
    }
    if digits_start == 0 && negative_coefficient == i128::MIN {
        return Err(err(ErrorCode::NumberOverflow, Some(node), "positive decimal coefficient exceeds i128"));
    }
    Ok(())
}

/// Validates one user-provided parameter according to its declared grammar.
pub fn validate_input(model: &ValidatorModel, field: &EndpointField, input: &str, limits: Limits) -> ValidatorResult<()> {
    if input.len() > limits.max_payload_bytes as usize { return Err(err(ErrorCode::PayloadTooLarge, None, "parameter input exceeds payload limit")); }
    match grammar_for(&field.ty) {
        InputGrammar::Text => {
            if input.len() > limits.max_string_bytes as usize { return Err(err(ErrorCode::PayloadTooLarge, None, "text parameter exceeds string limit")); }
            if input.len() > limits.max_decoded_bytes as usize { return Err(err(ErrorCode::DecodedArenaOverflow, None, "decoded text exceeds arena limit")); }
            let encoded_len = jmax_encode::escaped_string_len(input).map_err(decode_error)?;
            if encoded_len > limits.max_payload_bytes as usize { return Err(err(ErrorCode::PayloadTooLarge, None, "encoded text exceeds payload limit")); }
            validate_text_value(model, input, &field.ty, limits, 0)
        }
        InputGrammar::Json => {
            let mut strict = limits; strict.reject_duplicate_keys = true; strict.reject_unknown_keys = true;
            let doc = jmax_dom::build::parse(Payload::from_slice(input.as_bytes()), strict).map_err(decode_error)?;
            let root = jmax_dom::navigate::root(&doc);
            validate_node(model, root, &field.ty, limits, 0)
        }
    }
}

fn validate_text_value(model: &ValidatorModel, input: &str, ty: &EndpointType, limits: Limits, depth: u16) -> ValidatorResult<()> {
    if input.len() > limits.max_string_bytes as usize { return Err(err(ErrorCode::PayloadTooLarge, None, "text parameter exceeds string limit")); }
    match ty {
        EndpointType::String => Ok(()),
        EndpointType::Bytea => { let mut quoted = Vec::new(); quote(input, &mut quoted); let doc = jmax_dom::build::parse(Payload::from_vec(quoted), limits).map_err(decode_error)?; validate_node(model, jmax_dom::navigate::root(&doc), ty, limits, depth) }
        EndpointType::UUID | EndpointType::NanoId { .. } | EndpointType::IpAddr | EndpointType::BlockchainDecimal | EndpointType::BlockchainAddress | EndpointType::BlockchainTransactionHash => {
            let mut quoted = Vec::new(); quote(input, &mut quoted);
            let strict = limits;
            let doc = jmax_dom::build::parse(Payload::from_vec(quoted), strict).map_err(decode_error)?;
            validate_node(model, jmax_dom::navigate::root(&doc), ty, limits, depth)
        }
        _ => Err(err(ErrorCode::InternalError, None, "text grammar is not defined for this type")),
    }
}

fn quote(input: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in input.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""), '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"), '\r' => out.extend_from_slice(b"\\r"), '\t' => out.extend_from_slice(b"\\t"),
            '\u{08}' => out.extend_from_slice(b"\\b"), '\u{0c}' => out.extend_from_slice(b"\\f"),
            c if c <= '\u{1f}' => { out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes()); }
            c => { let mut buf = [0; 4]; out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()); }
        }
    }
    out.push(b'"');
}

/// Validates inputs and builds a complete, ordered JSON parameter object.
pub fn prepare_params(model: &ValidatorModel, endpoint: EndpointKey, inputs: &[ParameterInput], limits: Limits) -> ValidatorResult<PreparedParams> {
    let schema = model.endpoint(endpoint).ok_or_else(|| err(ErrorCode::MissingRequiredField, None, "endpoint key is missing or ambiguous"))?;
    let mut seen = std::collections::BTreeSet::new();
    for input in inputs {
        ensure_text_limit(&input.name, limits, None)?;
        if input.name.is_empty() { return Err(err(ErrorCode::MissingRequiredField, None, "parameter name is empty")); }
        if !seen.insert(input.name.as_str()) { return Err(err(ErrorCode::DuplicateObjectKey, None, "duplicate parameter input")); }
        let field = schema.params.iter().find(|field| field.name == input.name)
            .ok_or_else(|| err(ErrorCode::UnknownObjectKey, None, "unknown endpoint parameter"))?;
        validate_input(model, field, &input.text, limits)?;
    }
    for field in &schema.params {
        if !seen.contains(field.name.as_str()) && !matches!(&field.ty, EndpointType::Optional(_)) {
            return Err(err(ErrorCode::MissingRequiredField, None, "required endpoint parameter is missing"));
        }
    }
    let encode_limits = jmax_encode::EncodeLimits {
        max_output_bytes: limits.max_payload_bytes as usize,
        max_depth: limits.max_depth,
        max_container_items: limits.max_fields_per_object.max(limits.max_array_items),
        max_string_bytes: limits.max_string_bytes,
        max_number_bytes: u32::from(limits.max_number_digits),
    };
    let mut sink = jmax_encode::VecSink::new(limits.max_payload_bytes as usize);
    {
        let mut writer = jmax_encode::JsonWriter::new(&mut sink, encode_limits);
        writer.begin_object().map_err(decode_error)?;
        for field in &schema.params {
            let Some(input) = inputs.iter().find(|input| input.name == field.name) else { continue };
            writer.key(&field.name).map_err(decode_error)?;
            match grammar_for(&field.ty) {
                InputGrammar::Text => writer.string(&input.text).map_err(decode_error)?,
                InputGrammar::Json => {
                    let fragment = jmax_encode::raw::validate(input.text.as_bytes(), encode_limits).map_err(decode_error)?;
                    writer.raw(fragment).map_err(decode_error)?;
                }
            }
        }
        writer.end_object().map_err(decode_error)?;
        writer.finish().map_err(decode_error)?;
    }
    Ok(PreparedParams { endpoint, payload: Payload::from_vec(sink.into_vec()) })
}

/// Revalidates a public PreparedParams payload against its endpoint schema.
pub fn validate_prepared(model: &ValidatorModel, params: &PreparedParams, limits: Limits) -> ValidatorResult<()> {
    let schema = model.endpoint(params.endpoint).ok_or_else(|| err(ErrorCode::MissingRequiredField, None, "endpoint key is missing or ambiguous"))?;
    if params.payload.len() > limits.max_payload_bytes as usize { return Err(err(ErrorCode::PayloadTooLarge, None, "prepared payload exceeds limit")); }
    let mut strict = limits; strict.reject_duplicate_keys = true; strict.reject_unknown_keys = true;
    let doc = jmax_dom::build::parse(params.payload.clone(), strict).map_err(decode_error)?;
    let root = jmax_dom::navigate::root(&doc);
    validate_object_fields(model, root, &schema.params, limits, 0)
}
