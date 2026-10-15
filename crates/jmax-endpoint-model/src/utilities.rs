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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EndpointModel, EnumVariantSpec, ErrorSource, HeaderSpec, ServiceSpec, TypeDefinition};
    use alloc::{boxed::Box, collections::BTreeMap, format, vec};

    const SCHEMA: SchemaId = SchemaId(0x89ab_cdef);

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField {
            name: String::from(name), ty, description: String::from("metadata\n\0説明"),
        }
    }

    fn optional(ty: EndpointType, depth: usize) -> EndpointType {
        (0..depth).fold(ty, |inner, _| EndpointType::Optional(Box::new(inner)))
    }

    fn endpoint() -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code: u32::MAX, name: String::from("Read"), service: String::from("Store"),
            roles: Vec::new(), params: vec![field("query", EndpointType::String)],
            response: vec![field("found", EndpointType::Boolean)],
            stream_response: Some(EndpointType::Unit), description: String::new(),
        }
    }

    fn assert_error(error: Error, code: ErrorCode, schema: SchemaId, field: FieldId) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        assert_eq!((error.schema_id, error.field_id), (schema.0, field.0));
        assert!(!error.detail.is_empty());
    }

    fn alias_model(hops: usize) -> (EndpointModel, EndpointType) {
        assert!(hops > 0);
        let mut model = EndpointModel::default();
        model.definitions.push(TypeDefinition {
            name: String::from("Record"),
            ty: EndpointType::Struct { name: String::from("Record"), fields: Vec::new() },
        });
        let mut target = String::from("Record");
        for index in 1..hops {
            let name = format!("Alias{index}");
            model.definitions.push(TypeDefinition {
                name: name.clone(), ty: EndpointType::StructRef(target),
            });
            target = name;
        }
        (model, EndpointType::StructRef(target))
    }

    #[test]
    fn pg_scalar_vectors_and_sql_spellings_are_exact() {
        let model = EndpointModel::default();
        let registry = crate::registry::build_registry(&model).unwrap();
        let vectors = [
            (EndpointType::UInt32, PgType::BigInt, "bigint"),
            (EndpointType::Int32, PgType::Integer, "integer"),
            (EndpointType::Int64, PgType::BigInt, "bigint"),
            (EndpointType::Float64, PgType::DoublePrecision, "double precision"),
            (EndpointType::Boolean, PgType::Boolean, "boolean"),
            (EndpointType::String, PgType::Text, "text"),
            (EndpointType::Bytea, PgType::Bytea, "bytea"),
            (EndpointType::UUID, PgType::Uuid, "uuid"),
            (EndpointType::IpAddr, PgType::Inet, "inet"),
            (EndpointType::TimeStampMs, PgType::TimestampMs, "bigint"),
            (EndpointType::BlockchainDecimal, PgType::Numeric, "numeric"),
            (EndpointType::BlockchainAddress, PgType::Text, "text"),
            (EndpointType::BlockchainTransactionHash, PgType::Text, "text"),
        ];
        for (ty, expected, sql) in vectors {
            for depth in [0, 1, 2, MAX_REFERENCE_DEPTH] {
                let actual = pg_type(&registry, &optional(ty.clone(), depth)).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(actual.sql_name(), sql);
            }
        }
        for len in 0..=u8::MAX {
            assert_eq!(pg_type(&registry, &EndpointType::NanoId { len }).unwrap(), PgType::Text);
        }
    }

    #[test]
    fn pg_compounds_and_enum_values_do_not_expand() {
        let model = EndpointModel::default();
        let registry = crate::registry::build_registry(&model).unwrap();
        for ty in [
            EndpointType::Object,
            EndpointType::Vec(Box::new(EndpointType::Unit)),
            EndpointType::Struct {
                name: String::from("Node"),
                fields: vec![field("next", EndpointType::StructRef(String::from("Node")))],
            },
        ] {
            assert_eq!(pg_type(&registry, &ty).unwrap(), PgType::Jsonb);
        }
        let ty = EndpointType::Enum {
            name: String::from("State"), variants: vec![
                EnumVariantSpec { name: String::from("Low"), value: i32::MIN, description: String::new() },
                EnumVariantSpec { name: String::from("High"), value: i32::MAX, description: String::new() },
            ],
        };
        assert_eq!(pg_type(&registry, &ty).unwrap(), PgType::Integer);
    }

    #[test]
    fn pg_unit_and_optional_depth_errors_are_context_free() {
        let model = EndpointModel::default();
        let registry = crate::registry::build_registry(&model).unwrap();
        for depth in [0, 1, MAX_REFERENCE_DEPTH] {
            assert_error(pg_type(&registry, &optional(EndpointType::Unit, depth)).unwrap_err(),
                ErrorCode::UnsupportedSchemaFeature, SchemaId(0), FieldId(0));
        }
        assert_error(pg_type(&registry, &optional(EndpointType::Int32, MAX_REFERENCE_DEPTH + 1)).unwrap_err(),
            ErrorCode::DepthLimitExceeded, SchemaId(0), FieldId(0));
    }

    #[test]
    fn pg_alias_and_optional_edges_share_the_depth_budget() {
        for hops in [1, 2, MAX_REFERENCE_DEPTH] {
            let (model, reference) = alias_model(hops);
            let registry = crate::registry::build_registry(&model).unwrap();
            assert_eq!(pg_type(&registry, &reference).unwrap(), PgType::Jsonb);
            assert_eq!(pg_type(&registry, &optional(reference.clone(), MAX_REFERENCE_DEPTH - hops)).unwrap(), PgType::Jsonb);
            assert_error(pg_type(&registry, &optional(reference, MAX_REFERENCE_DEPTH - hops + 1)).unwrap_err(),
                ErrorCode::DepthLimitExceeded, SchemaId(0), FieldId(0));
        }
        let (model, reference) = alias_model(MAX_REFERENCE_DEPTH + 1);
        let registry = crate::registry::build_registry(&model).unwrap();
        let expected = crate::registry::resolve_type(&registry, &reference).unwrap_err();
        assert_eq!(pg_type(&registry, &reference).unwrap_err(), expected);
    }

    #[test]
    fn pg_resolves_inline_recursive_nominals_enum_aliases_and_tables() {
        let mut model = EndpointModel::default();
        let mut endpoint = endpoint();
        endpoint.params.push(field("node", EndpointType::Struct {
            name: String::from("Node"), fields: vec![field("next", optional(
                EndpointType::StructRef(String::from("Node")), 1,
            ))],
        }));
        endpoint.stream_response = Some(EndpointType::Enum {
            name: String::from("State"), variants: Vec::new(),
        });
        model.services.push(ServiceSpec { id: 0, name: String::from("Store"), endpoints: vec![endpoint] });
        model.definitions.push(TypeDefinition {
            name: String::from("StateAlias"), ty: EndpointType::EnumRef {
                name: String::from("State"), prefixed_name: true,
            },
        });
        let before = model.clone();
        let registry = crate::registry::build_registry(&model).unwrap();
        for ty in [
            EndpointType::StructRef(String::from("Node")),
            EndpointType::StructTable { struct_ref: String::from("Node") },
        ] {
            assert_eq!(pg_type(&registry, &optional(ty, 2)).unwrap(), PgType::Jsonb);
        }
        for prefixed_name in [false, true] {
            assert_eq!(pg_type(&registry, &EndpointType::EnumRef {
                name: String::from("StateAlias"), prefixed_name,
            }).unwrap(), PgType::Integer);
        }
        assert_eq!(model, before);
    }

    #[test]
    fn pg_reference_errors_match_the_registry_exactly() {
        let (mut model, _) = alias_model(2);
        model.definitions.push(TypeDefinition { name: String::from("Text"), ty: EndpointType::String });
        let registry = crate::registry::build_registry(&model).unwrap();
        for ty in [
            EndpointType::StructRef(String::from("Missing")),
            EndpointType::StructRef(String::from("_")),
            EndpointType::StructRef(String::from("Text")),
            EndpointType::EnumRef { name: String::from("Record"), prefixed_name: false },
            EndpointType::StructTable { struct_ref: String::from("Text") },
        ] {
            let expected = crate::registry::resolve_type(&registry, &ty).unwrap_err();
            assert_error(expected.clone(), ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
            assert_eq!(pg_type(&registry, &ty).unwrap_err(), expected);
            assert_eq!(pg_type(&registry, &optional(ty, 1)).unwrap_err(), expected);
        }
    }

    #[test]
    fn pg_defensively_preserves_alias_cycle_errors() {
        // Public construction rejects this model. A local fixture lets this
        // test verify propagation without altering the registry implementation.
        let model = EndpointModel {
            definitions: vec![TypeDefinition {
                name: String::from("Cycle"), ty: EndpointType::StructRef(String::from("Cycle")),
            }], ..EndpointModel::default()
        };
        let registry = Registry {
            model: &model, services_by_id: BTreeMap::new(), services_by_name: BTreeMap::new(),
            endpoints_by_code: BTreeMap::new(), endpoints_by_method: BTreeMap::new(),
            definitions_by_name: [("Cycle", 0)].into_iter().collect(),
            roles_by_name: BTreeMap::new(), errors_by_code: BTreeMap::new(),
        };
        let ty = EndpointType::StructRef(String::from("Cycle"));
        let expected = crate::registry::resolve_type(&registry, &ty).unwrap_err();
        assert_eq!(pg_type(&registry, &ty).unwrap_err(), expected);
        assert_error(expected, ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
    }

    #[test]
    fn header_normalization_preserves_token_punctuation_and_is_idempotent() {
        for (input, expected) in [
            ("Content-Type", "content-type"), ("X_Trace.ID", "x_trace.id"),
            ("!#$%&'*+-.^_`|~", "!#$%&'*+-.^_`|~"), ("0123456789", "0123456789"),
            ("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz"),
        ] {
            let actual = normalize_header_name(input).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(normalize_header_name(&actual).unwrap(), actual);
        }
        let long = "X".repeat(65_536);
        assert_eq!(normalize_header_name(&long).unwrap(), "x".repeat(65_536));
    }

    #[test]
    fn every_ascii_byte_obeys_the_header_token_grammar() {
        const TOKEN: &[u8] = b"!#$%&'*+-.^_`|~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        for byte in 0_u8..=127 {
            let name = format!("X{}Y", char::from(byte));
            let header = ProtocolHeader { name: name.clone(), value: String::new() };
            if TOKEN.contains(&byte) {
                assert_eq!(normalize_header_name(&name).unwrap(), name.to_ascii_lowercase());
                validate_header(&header).unwrap();
            } else {
                assert_error(normalize_header_name(&name).unwrap_err(), ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
                assert_error(validate_header(&header).unwrap_err(), ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
            }
        }
    }

    #[test]
    fn headers_reject_empty_unicode_and_injection_names() {
        for name in ["", ":method", " name", "name ", "a:b", "a/b", "a\r\nb", "a\0b", "é", "東京", "a\u{a0}b"] {
            assert_error(normalize_header_name(name).unwrap_err(), ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
            assert_error(validate_header(&ProtocolHeader { name: String::from(name), value: String::from("ok") }).unwrap_err(),
                ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
        }
    }

    #[test]
    fn every_ascii_value_byte_is_checked_without_mutation() {
        for byte in 0_u8..=127 {
            let header = ProtocolHeader { name: String::from("X-Trace"), value: format!("a{}z", char::from(byte)) };
            let before = header.clone();
            let actual = validate_header(&header);
            if byte == 9 || (32..=126).contains(&byte) {
                actual.unwrap();
            } else {
                assert_error(actual.unwrap_err(), ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
            }
            assert_eq!(header, before);
        }
        for value in ["", " \t ", "~!@#$%^&*()[]{}:;,'\"\\/?=", "a\tb\t", " value "] {
            validate_header(&ProtocolHeader { name: String::from("X"), value: String::from(value) }).unwrap();
        }
        for value in ["ok\r\nInjected: yes", "ok\n", "\r", "\0", "\u{7f}", "é", "東京", "\u{80}", "\u{a0}"] {
            assert_error(validate_header(&ProtocolHeader { name: String::from("X"), value: String::from(value) }).unwrap_err(),
                ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
        }
    }

    #[test]
    fn header_names_agree_with_model_registration_and_legacy_v2() {
        let mut model = EndpointModel::default();
        model.headers.push(HeaderSpec {
            name: String::from("X-Trace_ID"), ty: EndpointType::String,
            required: false, description: String::new(),
        });
        let imported = crate::legacy::import_legacy(crate::legacy::export_legacy(
            &model, crate::LegacyVersion::V2,
        ).unwrap()).unwrap();
        assert_eq!(imported, model);
        assert_eq!(normalize_header_name(&imported.headers[0].name).unwrap(), "x-trace_id");
        let mut duplicate = model.headers[0].clone();
        duplicate.name = normalize_header_name(&duplicate.name).unwrap();
        model.headers.push(duplicate);
        assert_error(crate::registry::build_registry(&model).unwrap_err(), ErrorCode::ConstraintViolation, SchemaId(0), FieldId(0));
    }

    #[test]
    fn field_selection_returns_the_original_unary_slices() {
        let mut endpoint = endpoint();
        let before = endpoint.clone();
        for (shape, expected) in [(MessageShape::Params, &endpoint.params), (MessageShape::Response, &endpoint.response)] {
            let actual = fields_for(&endpoint, shape);
            assert_eq!(actual, expected.as_slice());
            assert!(core::ptr::eq(actual.as_ptr(), expected.as_ptr()));
        }
        assert_eq!(endpoint, before);
        endpoint.params.clear();
        endpoint.response.clear();
        for stream in [None, Some(EndpointType::Boolean)] {
            endpoint.stream_response = stream;
            assert!(fields_for(&endpoint, MessageShape::Params).is_empty());
            assert!(fields_for(&endpoint, MessageShape::Response).is_empty());
        }
    }

    #[test]
    fn empty_schema_metadata_is_preserved_exactly() {
        for name in ["", " ", "schema\0説明"] {
            for id in [SchemaId(0), SCHEMA, SchemaId(u32::MAX)] {
                for reject_unknown in [false, true] {
                    let ir = schema_for_fields(name, id, &[], reject_unknown).unwrap();
                    assert_eq!(ir.name, name);
                    assert_eq!(ir.schema_id, id);
                    assert_eq!(ir.reject_unknown, reject_unknown);
                    assert_eq!(ir.required_count, 0);
                    assert!(ir.fields.is_empty());
                    let normalized = jmax_schema_ir::model::normalize(&ir).unwrap();
                    assert_eq!(normalized.name, name);
                    assert_eq!(normalized.schema_id, id);
                }
            }
        }
    }

    #[test]
    fn numeric_and_boolean_schema_vectors_have_exact_intrinsic_bounds() {
        let vectors = [
            (EndpointType::UInt32, ScalarKind::Number, Some(0), Some(4_294_967_295_i64)),
            (EndpointType::Int32, ScalarKind::Number, Some(-2_147_483_648), Some(2_147_483_647)),
            (EndpointType::Int64, ScalarKind::Number, Some(i64::MIN), Some(i64::MAX)),
            (EndpointType::TimeStampMs, ScalarKind::Number, Some(i64::MIN), Some(i64::MAX)),
            (EndpointType::Float64, ScalarKind::Number, None, None),
            (EndpointType::Boolean, ScalarKind::True, None, None),
            (EndpointType::String, ScalarKind::String, None, None),
        ];
        for (ty, kind, min, max) in vectors {
            for required in [true, false] {
                let ty = if required { ty.clone() } else { optional(ty.clone(), 1) };
                let input = vec![field("value", ty)];
                let ir = schema_for_fields("Vector", SCHEMA, &input, true).unwrap();
                assert_eq!(ir.required_count, u16::from(required));
                let mapped = &ir.fields[0];
                assert_eq!(mapped.field_id, FieldId(0));
                assert_eq!(mapped.name, "value");
                assert_eq!(mapped.required, required);
                assert_eq!(mapped.scalar_kind, Some(kind));
                assert_eq!((mapped.min_i64, mapped.max_i64), (min, max));
                assert_eq!((mapped.min_len, mapped.max_len), (None, None));
                assert!(mapped.format.is_none());
                let normalized = jmax_schema_ir::model::normalize(&ir).unwrap();
                let expected_kind = if min.is_some() { jmax_schema_ir::ValueKind::Integer }
                    else { match kind {
                        ScalarKind::Number => jmax_schema_ir::ValueKind::Number,
                        ScalarKind::True => jmax_schema_ir::ValueKind::Boolean,
                        _ => jmax_schema_ir::ValueKind::String,
                    }};
                match &normalized.nodes[1] {
                    jmax_schema_ir::TypeNode::Scalar(constraints) => {
                        assert_eq!(constraints.kind, expected_kind);
                        assert_eq!((constraints.min_i64, constraints.max_i64), (min, max));
                        assert_ne!(constraints.kind, jmax_schema_ir::ValueKind::Null);
                    }
                    _ => panic!("expected a scalar node"),
                }
            }
        }
    }

    #[test]
    fn format_schema_vectors_use_only_available_registry_names() {
        let vectors = [
            (EndpointType::UUID, "uuid", jmax_format::FormatId::Uuid),
            (EndpointType::IpAddr, "ip", jmax_format::FormatId::IpAddr),
            (EndpointType::Bytea, "base64", jmax_format::FormatId::Base64),
            (EndpointType::NanoId { len: 21 }, "nanoid", jmax_format::FormatId::NanoId),
            (EndpointType::BlockchainAddress, "blockchain-address", jmax_format::FormatId::BlockchainAddress),
            (EndpointType::BlockchainTransactionHash, "blockchain-transaction-hash", jmax_format::FormatId::BlockchainTransactionHash),
        ];
        for (ty, format, id) in vectors {
            for required in [true, false] {
                let ty = if required { ty.clone() } else { optional(ty.clone(), 1) };
                let ir = schema_for_fields("Formats", SCHEMA, &[field("value", ty)], false).unwrap();
                let mapped = &ir.fields[0];
                assert_eq!(mapped.format.as_deref(), Some(format));
                assert_eq!(mapped.scalar_kind, Some(ScalarKind::String));
                assert_eq!(mapped.required, required);
                assert_eq!((mapped.min_len, mapped.max_len, mapped.min_i64, mapped.max_i64), (None, None, None, None));
                assert_eq!(jmax_format::registry::lookup(mapped.format.as_deref().unwrap()).unwrap(), id);
                let normalized = jmax_schema_ir::model::normalize(&ir).unwrap();
                match &normalized.nodes[1] {
                    jmax_schema_ir::TypeNode::Scalar(value) => {
                        assert_eq!(value.kind, jmax_schema_ir::ValueKind::String);
                        assert_eq!(value.format.as_deref(), Some(format));
                    }
                    _ => panic!("expected a formatted string node"),
                }
            }
        }
    }

    #[test]
    fn bytea_wire_vectors_require_standard_canonical_padded_base64() {
        let ir = schema_for_fields("Binary", SCHEMA, &[field("bytes", EndpointType::Bytea)], true).unwrap();
        let format = ir.fields[0].format.as_deref().unwrap();
        let options = jmax_format::FormatOptions::default();
        for encoded in [b"".as_slice(), b"Zg==", b"Zm8=", b"Zm9v", b"+/8="] {
            jmax_format::registry::validate_named(format, encoded, &options).unwrap();
        }
        for encoded in [b"Zg".as_slice(), b"Zm8", b"Zh==", b"Zm9=", b"-_8=", b"Z g==", b"Zg==="] {
            assert_eq!(jmax_format::registry::validate_named(format, encoded, &options).unwrap_err().code, ErrorCode::FormatMismatch);
        }
    }

    #[test]
    fn mapped_formats_accept_and_reject_deterministic_wire_vectors() {
        let vectors = [
            (EndpointType::UUID, String::from("00000000-0000-0000-0000-000000000000"),
                String::from("00000000000000000000000000000000")),
            (EndpointType::IpAddr, String::from("127.0.0.1"), String::from("127.00.0.1")),
            (EndpointType::IpAddr, String::from("::1"), String::from("[::1]")),
            (EndpointType::NanoId { len: 21 }, String::from("0123456789abcdefghijk"),
                String::from("0123456789abcdefghij!")),
            (EndpointType::BlockchainAddress, format!("0x{}", "a".repeat(40)),
                format!("0X{}", "a".repeat(40))),
            (EndpointType::BlockchainTransactionHash, format!("0x{}", "F".repeat(64)),
                format!("0x{}", "F".repeat(63))),
        ];
        for (ty, valid, invalid) in vectors {
            let ir = schema_for_fields("Wire", SCHEMA, &[field("value", ty)], true).unwrap();
            let name = ir.fields[0].format.as_deref().unwrap();
            let options = jmax_format::FormatOptions::default();
            jmax_format::registry::validate_named(name, valid.as_bytes(), &options).unwrap();
            let error = jmax_format::registry::validate_named(name, invalid.as_bytes(), &options).unwrap_err();
            assert_eq!(error.code, ErrorCode::FormatMismatch);
            assert_eq!(error.source, ErrorSource::SchemaVm);
            assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        }
    }

    #[test]
    fn nanoid_default_is_the_only_representable_length() {
        assert_eq!(jmax_format::FormatOptions::default().nanoid_len, 21);
        for len in 0..=u8::MAX {
            for required in [true, false] {
                let ty = EndpointType::NanoId { len };
                let ty = if required { ty } else { optional(ty, 1) };
                let result = schema_for_fields("Nano", SCHEMA, &[field("id", ty)], true);
                if len == 21 {
                    assert_eq!(result.unwrap().fields[0].format.as_deref(), Some("nanoid"));
                } else {
                    assert_error(result.unwrap_err(), ErrorCode::UnsupportedSchemaFeature, SCHEMA, FieldId(0));
                }
            }
        }
    }

    #[test]
    fn unrepresentable_types_and_nested_optionals_fail_with_field_context() {
        let types = [
            EndpointType::Unit, EndpointType::BlockchainDecimal, EndpointType::Object,
            EndpointType::Struct { name: String::from("Record"), fields: Vec::new() },
            EndpointType::StructRef(String::from("Record")),
            EndpointType::StructTable { struct_ref: String::from("Record") },
            EndpointType::Enum { name: String::from("State"), variants: Vec::new() },
            EndpointType::EnumRef { name: String::from("State"), prefixed_name: true },
            EndpointType::Vec(Box::new(EndpointType::String)),
            optional(EndpointType::Int64, 2), optional(EndpointType::String, 3),
        ];
        for ty in types {
            for depth in [0, 1] {
                let fields = vec![field("ok", EndpointType::String), field("unsupported", optional(ty.clone(), depth))];
                let before = fields.clone();
                assert_error(schema_for_fields("Reject", SCHEMA, &fields, false).unwrap_err(),
                    ErrorCode::UnsupportedSchemaFeature, SCHEMA, FieldId(1));
                assert_eq!(fields, before);
            }
        }
    }

    #[test]
    fn field_names_preserve_utf8_bytes_and_declaration_order() {
        let names = ["z-last", "A", "a", "é", "e\u{301}", "東京", " ", "key\0value"];
        let fields: Vec<_> = names.iter().enumerate().map(|(index, name)| {
            field(name, optional(EndpointType::Boolean, index % 2))
        }).collect();
        let before = fields.clone();
        let ir = schema_for_fields("Order", SCHEMA, &fields, true).unwrap();
        assert_eq!(ir.required_count, 4);
        for (index, (mapped, expected)) in ir.fields.iter().zip(names).enumerate() {
            assert_eq!(mapped.field_id, FieldId(u32::try_from(index).unwrap()));
            assert_eq!(mapped.name.as_bytes(), expected.as_bytes());
            assert_eq!(mapped.required, index % 2 == 0);
        }
        assert_eq!(fields, before);
    }

    #[test]
    fn empty_and_duplicate_field_names_fail_in_declaration_order() {
        for fields in [
            vec![field("", EndpointType::String)],
            vec![field("ok", EndpointType::String), field("ok", EndpointType::Boolean)],
            vec![field("ok", EndpointType::String), field("", EndpointType::Boolean)],
        ] {
            let index = fields.len() - 1;
            assert_error(schema_for_fields("Names", SCHEMA, &fields, true).unwrap_err(),
                ErrorCode::ConstraintViolation, SCHEMA, FieldId(u32::try_from(index).unwrap()));
        }
        let duplicate_before_unsupported = [field("x", EndpointType::String), field("x", EndpointType::Unit)];
        assert_error(schema_for_fields("Names", SCHEMA, &duplicate_before_unsupported, true).unwrap_err(),
            ErrorCode::ConstraintViolation, SCHEMA, FieldId(1));
        let unsupported_before_empty = [field("x", EndpointType::Unit), field("", EndpointType::String)];
        assert_error(schema_for_fields("Names", SCHEMA, &unsupported_before_empty, true).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, SCHEMA, FieldId(0));
    }

    #[test]
    fn required_count_checks_u16_boundary_without_limiting_optional_fields() {
        let mut fields: Vec<_> = (0..usize::from(u16::MAX)).map(|index| {
            field(&format!("f{index}"), EndpointType::Boolean)
        }).collect();
        let ir = schema_for_fields("Maximum", SCHEMA, &fields, false).unwrap();
        assert_eq!(ir.required_count, u16::MAX);
        assert_eq!(ir.fields.last().unwrap().field_id, FieldId(u32::from(u16::MAX) - 1));
        drop(ir);
        fields.push(field("overflow", EndpointType::Boolean));
        assert_error(schema_for_fields("Overflow", SCHEMA, &fields, false).unwrap_err(),
            ErrorCode::BudgetExhausted, SCHEMA, FieldId(u32::from(u16::MAX)));
        fields.last_mut().unwrap().ty = optional(EndpointType::Boolean, 1);
        let ir = schema_for_fields("OneOptional", SCHEMA, &fields, false).unwrap();
        assert_eq!(ir.required_count, u16::MAX);
        assert_eq!(ir.fields.len(), usize::from(u16::MAX) + 1);
        drop(ir);
        for field in &mut fields {
            field.ty = optional(EndpointType::Boolean, 1);
        }
        let ir = schema_for_fields("AllOptional", SCHEMA, &fields, false).unwrap();
        assert_eq!(ir.required_count, 0);
        assert_eq!(ir.fields.last().unwrap().field_id, FieldId(65_535));
    }

    #[test]
    fn field_indices_check_u32_conversion_without_large_allocations() {
        assert_eq!(checked_field_id(0, SCHEMA).unwrap(), FieldId(0));
        assert_eq!(checked_field_id(u32::MAX as usize, SCHEMA).unwrap(), FieldId(u32::MAX));
        #[cfg(target_pointer_width = "64")]
        assert_error(checked_field_id(u32::MAX as usize + 1, SCHEMA).unwrap_err(),
            ErrorCode::BudgetExhausted, SCHEMA, FieldId(0));
    }

    #[test]
    fn format_lookup_errors_keep_registry_provenance_and_gain_schema_context() {
        for name in [String::new(), String::from("UUID"), "x".repeat(jmax_format::MAX_FORMAT_NAME_BYTES + 1)] {
            let expected = jmax_format::registry::lookup(&name).unwrap_err().with_schema(SCHEMA.0, 7);
            assert_eq!(checked_format(&name, SCHEMA, FieldId(7)).unwrap_err(), expected);
            assert_eq!(expected.source, ErrorSource::SchemaVm);
            assert_eq!(expected.byte_offset, 0);
        }
    }

    #[test]
    fn selected_endpoint_fields_integrate_with_registry_legacy_and_schema_ir() {
        let mut endpoint = endpoint();
        endpoint.params = vec![
            field("zCount", EndpointType::UInt32),
            field("aTrace", optional(EndpointType::UUID, 1)),
            field("mFlag", EndpointType::Boolean),
        ];
        endpoint.response = vec![field("epoch", EndpointType::TimeStampMs)];
        let model = EndpointModel {
            services: vec![ServiceSpec { id: u16::MAX, name: String::from("Store"), endpoints: vec![endpoint] }],
            ..EndpointModel::default()
        };
        let document = crate::legacy::export_legacy(&model, crate::LegacyVersion::V1).unwrap();
        let imported = crate::legacy::import_legacy(document).unwrap();
        let registry = crate::registry::build_registry(&imported).unwrap();
        let method = crate::wire_names::method_name("Store", "Read").unwrap();
        let endpoint = crate::registry::endpoint_by_method(&registry, &method).unwrap();
        for (shape, required) in [(MessageShape::Params, vec![FieldId(0), FieldId(2)]), (MessageShape::Response, vec![FieldId(0)])] {
            let fields = fields_for(endpoint, shape);
            let ir = schema_for_fields(&method, SCHEMA, fields, true).unwrap();
            let resolved = jmax_schema_ir::resolve::resolve(
                jmax_schema_ir::model::normalize(&ir).unwrap(), jmax_schema_ir::ResolveLimits::default(),
            ).unwrap();
            let root = resolved.root();
            assert_eq!(resolved.schema().name, method);
            assert_eq!(resolved.schema().schema_id, SCHEMA);
            assert_eq!(jmax_schema_ir::model::required_fields(&resolved, root).unwrap(), required);
            match resolved.node(root).unwrap() {
                jmax_schema_ir::TypeNode::Object(object) => assert!(object.reject_unknown),
                _ => panic!("expected the schema root object"),
            }
            for (index, field) in fields.iter().enumerate() {
                let property = jmax_schema_ir::model::property(&resolved, root, &field.name).unwrap().unwrap();
                assert_eq!(property.field_id, FieldId(u32::try_from(index).unwrap()));
                assert_eq!(property.slot, Some(u32::try_from(index).unwrap()));
                let constraints = jmax_schema_ir::model::constraints(&resolved, property.ty).unwrap().unwrap();
                assert_ne!(constraints.kind, jmax_schema_ir::ValueKind::Null);
            }
            jmax_schema_ir::policy::validate_schema(&resolved, &jmax_schema_ir::SchemaPolicy::default()).unwrap();
        }
        assert_eq!(imported, model);
    }

    #[test]
    fn schema_strings_are_owned_copies_of_the_input() {
        let mut name = String::from("Original\0説明");
        let mut fields = vec![field("value\0説明", EndpointType::UUID)];
        let ir = schema_for_fields(&name, SCHEMA, &fields, false).unwrap();
        name.clear();
        fields[0].name.clear();
        fields[0].description.clear();
        fields.clear();
        assert_eq!(ir.name, "Original\0説明");
        assert_eq!(ir.fields[0].name, "value\0説明");
        assert_eq!(ir.fields[0].format.as_deref(), Some("uuid"));
    }
}
