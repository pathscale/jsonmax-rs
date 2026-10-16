//! PostgreSQL storage categories, checked protocol headers, and flat schema IR.
//!
//! These helpers do not execute SQL, serialize headers, or issue validation
//! proofs. Schema conversion preserves only semantics representable by the ABI.

use super::{
    model_error, EndpointField, EndpointSchemaSpec, EndpointType, Error, ErrorCode,
    FieldId, MessageShape, PgType, ProtocolHeader, Registry, Result, ScalarKind,
    SchemaFieldIr, SchemaId, SchemaIr, MAX_REFERENCE_DEPTH,
};
use alloc::{collections::BTreeSet, string::String, vec::Vec};

/// Return the PostgreSQL storage category, resolving nominal references.
///
/// Optional wrappers do not change storage. Compound values use JSONB without
/// expanding their members, including recursive records. A unit value has no
/// storage category. Reference failures are returned unchanged by the registry.
pub fn pg_type(registry: &Registry<'_>, ty: &EndpointType) -> Result<PgType> {
    let mut ty = ty;
    let mut depth = 0;
    while let EndpointType::Optional(inner) = ty {
        advance_depth(&mut depth)?;
        ty = inner;
    }
    if reference_name(ty).is_some() {
        let resolved = super::registry::resolve_type(registry, ty)?;
        // The resolver validates cycles, names, and nominal kinds first. Count
        // its reference edges as well as the optional edges above. Only named
        // definitions can be aliases; inline declarations are concrete types.
        while let Some(name) = reference_name(ty) {
            advance_depth(&mut depth)?;
            ty = super::registry::definition_by_name(registry, name)
                .map_or(resolved, |definition| &definition.ty);
        }
        ty = resolved;
    }
    match ty {
        EndpointType::UInt32 | EndpointType::Int64 => Ok(PgType::BigInt),
        EndpointType::Int32 => Ok(PgType::Integer),
        EndpointType::Float64 => Ok(PgType::DoublePrecision),
        EndpointType::Boolean => Ok(PgType::Boolean),
        EndpointType::String | EndpointType::NanoId { .. }
        | EndpointType::BlockchainAddress | EndpointType::BlockchainTransactionHash => Ok(PgType::Text),
        EndpointType::Bytea => Ok(PgType::Bytea),
        EndpointType::UUID => Ok(PgType::Uuid),
        EndpointType::IpAddr => Ok(PgType::Inet),
        EndpointType::TimeStampMs => Ok(PgType::TimestampMs),
        EndpointType::BlockchainDecimal => Ok(PgType::Numeric),
        EndpointType::Enum { .. } => Ok(PgType::Integer),
        EndpointType::Struct { .. } | EndpointType::Object | EndpointType::Vec(_) => Ok(PgType::Jsonb),
        EndpointType::Unit => Err(model_error(
            ErrorCode::UnsupportedSchemaFeature, "unit has no PostgreSQL storage type",
        )),
        // Successful resolution and optional unwrapping exclude these cases.
        EndpointType::Optional(_) | EndpointType::StructRef(_)
        | EndpointType::StructTable { .. } | EndpointType::EnumRef { .. } => Err(model_error(
            ErrorCode::ConstraintViolation, "unresolved PostgreSQL storage type",
        )),
    }
}

fn reference_name(ty: &EndpointType) -> Option<&str> {
    match ty {
        EndpointType::StructRef(name) | EndpointType::EnumRef { name, .. }
        | EndpointType::StructTable { struct_ref: name } => Some(name),
        _ => None,
    }
}

fn advance_depth(depth: &mut usize) -> Result<()> {
    if *depth == MAX_REFERENCE_DEPTH {
        return Err(model_error(
            ErrorCode::DepthLimitExceeded, "endpoint type traversal depth exceeded",
        ));
    }
    *depth += 1;
    Ok(())
}

/// Validate an HTTP token name and return its ASCII lowercase spelling.
///
/// Whitespace, colons, non-ASCII text, and empty names are rejected. No trimming
/// or Unicode case conversion is performed.
pub fn normalize_header_name(name: &str) -> Result<String> {
    check_header_name(name)?;
    Ok(name.to_ascii_lowercase())
}

fn check_header_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
                | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
    }) {
        return Err(model_error(ErrorCode::ConstraintViolation, "invalid HTTP header name"));
    }
    Ok(())
}

/// Check a header without mutating its spelling or value.
///
/// Values may contain horizontal tabs and printable ASCII, including leading
/// and trailing spaces. CR, LF, NUL, DEL, and non-ASCII bytes are rejected.
pub fn validate_header(header: &ProtocolHeader) -> Result<()> {
    check_header_name(&header.name)?;
    if !header.value.bytes().all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte)) {
        return Err(model_error(ErrorCode::ConstraintViolation, "invalid HTTP header value"));
    }
    Ok(())
}

/// Borrow the declared unary fields in order, independently of stream metadata.
pub fn fields_for(endpoint: &EndpointSchemaSpec, shape: MessageShape) -> &[EndpointField] {
    match shape {
        MessageShape::Params => &endpoint.params,
        MessageShape::Response => &endpoint.response,
    }
}

/// Create flat ABI schema IR in declaration order with checked identifiers.
///
/// A single optional wrapper permits absence, while the underlying scalar kind
/// still rejects explicit null. Nested optionals, records, references, enums,
/// units, and decimals cannot be expressed faithfully by this flat IR.
///
/// Integer bounds preserve the entire intrinsic range, including full i64
/// bounds so the schema IR adapter selects Integer rather than Number. NanoId
/// is supported only at the format registry's default length of 21 bytes.
/// Local failures carry SchemaCompile provenance and schema/field context;
/// format lookup failures retain their original provenance with that context.
pub fn schema_for_fields(
    name: &str,
    schema_id: SchemaId,
    fields: &[EndpointField],
    reject_unknown: bool,
) -> Result<SchemaIr> {
    // Count as well as individual indices must be representable in the ABI.
    checked_field_id(fields.len(), schema_id)?;
    let mut output = Vec::new();
    output.try_reserve_exact(fields.len()).map_err(|_| {
        schema_error(schema_id, FieldId(0), ErrorCode::BudgetExhausted, "schema field allocation failed")
    })?;
    let mut names = BTreeSet::new();
    let mut required_count = 0_u16;
    for (index, field) in fields.iter().enumerate() {
        let field_id = checked_field_id(index, schema_id)?;
        if field.name.is_empty() || !names.insert(field.name.as_str()) {
            return Err(schema_error(
                schema_id, field_id, ErrorCode::ConstraintViolation, "empty or duplicate field name",
            ));
        }
        let mapped = scalar_field(field, schema_id, field_id)?;
        if mapped.required {
            required_count = required_count.checked_add(1).ok_or_else(|| {
                schema_error(schema_id, field_id, ErrorCode::BudgetExhausted, "required count exceeds ABI u16")
            })?;
        }
        output.push(mapped);
    }
    Ok(SchemaIr {
        name: copy_string(name, schema_id, FieldId(0))?,
        schema_id,
        required_count,
        reject_unknown,
        fields: output,
    })
}

fn checked_field_id(index: usize, schema: SchemaId) -> Result<FieldId> {
    u32::try_from(index).map(FieldId).map_err(|_| {
        schema_error(schema, FieldId(0), ErrorCode::BudgetExhausted, "schema field count exceeds ABI u32")
    })
}

fn schema_error(schema: SchemaId, field: FieldId, code: ErrorCode, detail: &'static str) -> Error {
    model_error(code, detail).with_schema(schema.0, field.0)
}

fn copy_string(value: &str, schema: SchemaId, field: FieldId) -> Result<String> {
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| {
        schema_error(schema, field, ErrorCode::BudgetExhausted, "schema string allocation failed")
    })?;
    output.push_str(value);
    Ok(output)
}

fn checked_format(name: &str, schema: SchemaId, field: FieldId) -> Result<String> {
    jmax_format::registry::lookup(name).map_err(|error| error.with_schema(schema.0, field.0))?;
    copy_string(name, schema, field)
}

fn scalar_field(field: &EndpointField, schema: SchemaId, field_id: FieldId) -> Result<SchemaFieldIr> {
    let (ty, required) = match &field.ty {
        EndpointType::Optional(inner) => (inner.as_ref(), false),
        ty => (ty, true),
    };
    let mut output = SchemaFieldIr {
        field_id,
        name: copy_string(&field.name, schema, field_id)?,
        required,
        scalar_kind: None,
        format: None,
        min_len: None,
        max_len: None,
        min_i64: None,
        max_i64: None,
    };
    let format = match ty {
        EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64 | EndpointType::TimeStampMs => {
            output.scalar_kind = Some(ScalarKind::Number);
            let (minimum, maximum) = match ty {
                EndpointType::UInt32 => (0, i64::from(u32::MAX)),
                EndpointType::Int32 => (i64::from(i32::MIN), i64::from(i32::MAX)),
                _ => (i64::MIN, i64::MAX),
            };
            output.min_i64 = Some(minimum);
            output.max_i64 = Some(maximum);
            None
        }
        EndpointType::Float64 => {
            output.scalar_kind = Some(ScalarKind::Number);
            None
        }
        EndpointType::Boolean => {
            output.scalar_kind = Some(ScalarKind::True);
            None
        }
        EndpointType::String => {
            output.scalar_kind = Some(ScalarKind::String);
            None
        }
        EndpointType::UUID => Some("uuid"),
        EndpointType::IpAddr => Some("ip"),
        EndpointType::Bytea => Some("base64"),
        EndpointType::NanoId { len: 21 } => Some("nanoid"),
        EndpointType::BlockchainAddress => Some("blockchain-address"),
        EndpointType::BlockchainTransactionHash => Some("blockchain-transaction-hash"),
        EndpointType::NanoId { .. } | EndpointType::BlockchainDecimal | EndpointType::Unit
        | EndpointType::Enum { .. } | EndpointType::EnumRef { .. }
        | EndpointType::Struct { .. } | EndpointType::StructRef(_)
        | EndpointType::Object | EndpointType::StructTable { .. }
        | EndpointType::Vec(_) | EndpointType::Optional(_) => {
            return Err(schema_error(
                schema, field_id, ErrorCode::UnsupportedSchemaFeature,
                "endpoint type cannot be represented by flat scalar schema IR",
            ));
        }
    };
    if let Some(format) = format {
        output.scalar_kind = Some(ScalarKind::String);
        output.format = Some(checked_format(format, schema, field_id)?);
    }
    Ok(output)
}
