//! `JsonSchema` expansion with an expansion-time canonical schema identity.

use super::*;
use jmax_abi::schema::{FieldId, SchemaFieldIr, SchemaId, SchemaIr};
use jmax_abi::syntax::ScalarKind;
use std::collections::BTreeSet;

/// Emit only SchemaProvider. Both methods describe the exact same flat ABI IR;
/// unsupported semantics fail before any implementation is returned.
pub(crate) fn expand(input: &DeriveInput) -> DeriveResult<TokenStream> {
    let fields = match &input.shape {
        ItemShape::NamedStruct(fields) => fields.iter().map(|field| SourceField {
            ty: field.ty.clone(),
            wire_name: field.wire_name.clone(),
            slot: field.slot,
            attrs: field.attrs.clone(),
        }).collect(),
        ItemShape::UnitStruct => Vec::new(),
        ItemShape::Enum(_) => return Err(DeriveError::new(
            input.span, "JsonSchema supports only named structs and unit structs",
        )),
    };
    let model = SourceInput {
        ident: input.ident.to_string(),
        impl_generics: input.impl_generics.to_string(),
        type_generics: input.type_generics.to_string(),
        where_clause: input.where_clause.to_string(),
        abi: input.attrs.abi_path.to_string(),
        schema_name: input.attrs.schema_name.clone(),
        schema_id: input.attrs.schema_id,
        reject_unknown: input.attrs.reject_unknown,
        fields,
    };
    let source = render(&model).map_err(|error| {
        let span = match (&input.shape, error.field) {
            (ItemShape::NamedStruct(fields), Some(index)) =>
                fields.get(index).map_or(input.span, |field| field.span),
            _ => input.span,
        };
        DeriveError::new(span, error.message)
    })?;
    parse_generated(&source, input.span)
}

// The compiler bridge is deliberately confined to expand. These owned models
// exercise the production builder and renderer in ordinary unit tests, where
// constructing proc_macro spans or literals would require a compiler invocation.
#[derive(Clone, Debug)]
struct SourceField {
    ty: FieldType,
    wire_name: String,
    slot: Option<u32>,
    attrs: FieldAttrs,
}

#[derive(Clone, Debug)]
struct SourceInput {
    ident: String,
    impl_generics: String,
    type_generics: String,
    where_clause: String,
    abi: String,
    schema_name: Option<String>,
    schema_id: Option<u32>,
    reject_unknown: bool,
    fields: Vec<SourceField>,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceError {
    field: Option<usize>,
    message: String,
}

impl SourceError {
    fn new(field: Option<usize>, message: impl Into<String>) -> Self {
        Self { field, message: message.into() }
    }
}

fn next_slot(current: u32, field: usize) -> Result<u32, SourceError> {
    current.checked_add(1).ok_or_else(|| SourceError::new(
        Some(field), "JsonSchema slot count exceeds u32::MAX",
    ))
}

fn next_required(current: u16, field: usize) -> Result<u16, SourceError> {
    current.checked_add(1).ok_or_else(|| SourceError::new(
        Some(field), "JsonSchema required_count exceeds u16::MAX",
    ))
}

/// Intersect constraints rather than replacing the intrinsic Rust range. An
/// empty intersection is an error even when each supplied endpoint fits i64.
fn integer_bounds(scalar: ScalarType, attrs: &FieldAttrs, index: usize)
    -> Result<(Option<i64>, Option<i64>), SourceError>
{
    let (minimum, maximum) = match scalar {
        ScalarType::U32 => (0, i64::from(u32::MAX)),
        ScalarType::I32 => (i64::from(i32::MIN), i64::from(i32::MAX)),
        ScalarType::I64 => (i64::MIN, i64::MAX),
        _ => return Err(SourceError::new(
            Some(index), "JsonSchema integer bounds require u32, i32, or i64",
        )),
    };
    let minimum = attrs.min_i64.map_or(minimum, |bound| minimum.max(bound));
    let maximum = attrs.max_i64.map_or(maximum, |bound| maximum.min(bound));
    if minimum > maximum {
        return Err(SourceError::new(
            Some(index), "JsonSchema integer bounds have an empty intersection with the Rust type range",
        ));
    }
    // I64 must retain both endpoints even without attributes: Number with no
    // integer bounds selects the floating Number category in the ABI adapter.
    Ok((Some(minimum), Some(maximum)))
}

fn build_field(field: &SourceField, scalar: ScalarType, optional: bool,
    slot: u32, index: usize) -> Result<SchemaFieldIr, SourceError>
{
    let attrs = &field.attrs;
    let scalar_kind = match scalar {
        ScalarType::Bool => ScalarKind::True,
        ScalarType::String => ScalarKind::String,
        ScalarType::U32 | ScalarType::I32 | ScalarType::I64 | ScalarType::F64 =>
            ScalarKind::Number,
        ScalarType::U64 => return Err(SourceError::new(Some(index),
            "JsonSchema cannot represent the full u64 unsigned-integer category in the flat ABI IR")),
        ScalarType::Decimal128 => return Err(SourceError::new(Some(index),
            "JsonSchema cannot represent Decimal128 without an exact decimal policy in the flat ABI IR")),
    };
    let has_string_constraints = attrs.format.is_some()
        || attrs.min_len.is_some() || attrs.max_len.is_some();
    if scalar != ScalarType::String && has_string_constraints {
        return Err(SourceError::new(Some(index),
            "JsonSchema format and length attributes require String"));
    }
    if let (Some(minimum), Some(maximum)) = (attrs.min_len, attrs.max_len) {
        if minimum > maximum {
            return Err(SourceError::new(Some(index), "JsonSchema length bounds are inverted"));
        }
    }
    let integer = matches!(scalar, ScalarType::U32 | ScalarType::I32 | ScalarType::I64);
    if !integer && (attrs.min_i64.is_some() || attrs.max_i64.is_some()) {
        return Err(SourceError::new(Some(index),
            "JsonSchema integer-bound attributes require u32, i32, or i64; f64 must remain Number"));
    }
    let (min_i64, max_i64) = if integer {
        integer_bounds(scalar, attrs, index)?
    } else {
        (None, None)
    };
    Ok(SchemaFieldIr {
        field_id: FieldId(slot),
        name: field.wire_name.clone(),
        required: !optional && !attrs.default,
        scalar_kind: Some(scalar_kind),
        format: attrs.format.clone(),
        min_len: attrs.min_len,
        max_len: attrs.max_len,
        min_i64,
        max_i64,
    })
}

/// Attribute syntax is parsed once by attributes. This builder checks the
/// semantic layout it consumes and never silently discards a constraint.
fn build_schema(input: &SourceInput) -> Result<SchemaIr, SourceError> {
    let name = input.schema_name.as_deref().unwrap_or_else(||
        input.ident.strip_prefix("r#").unwrap_or(&input.ident));
    if name.is_empty() {
        return Err(SourceError::new(None, "JsonSchema schema name must not be empty"));
    }
    let mut schema = SchemaIr {
        name: name.into(),
        schema_id: SchemaId(input.schema_id.unwrap_or(0)),
        required_count: 0,
        reject_unknown: input.reject_unknown,
        fields: Vec::new(),
    };
    let mut slot = 0u32;
    let mut names = BTreeSet::new();
    for (index, field) in input.fields.iter().enumerate() {
        let (scalar, optional) = match field.ty {
            FieldType::Skipped => {
                if field.slot.is_some() {
                    return Err(SourceError::new(Some(index),
                        "a skipped JsonSchema field must not consume a slot"));
                }
                if field.attrs.rename.is_some() || field.attrs.format.is_some()
                    || field.attrs.min_len.is_some() || field.attrs.max_len.is_some()
                    || field.attrs.min_i64.is_some() || field.attrs.max_i64.is_some()
                {
                    return Err(SourceError::new(Some(index),
                        "a skipped JsonSchema field cannot carry wire-name or constraint attributes"));
                }
                continue;
            }
            FieldType::Scalar(scalar) => (scalar, false),
            FieldType::Optional(scalar) => (scalar, true),
        };
        if field.slot != Some(slot) {
            return Err(SourceError::new(Some(index),
                "JsonSchema slots must be dense and follow field declaration order"));
        }
        if field.wire_name.is_empty() {
            return Err(SourceError::new(Some(index), "JsonSchema wire names must not be empty"));
        }
        if !names.insert(field.wire_name.as_str()) {
            return Err(SourceError::new(Some(index), "JsonSchema has duplicate wire names"));
        }
        let emitted = build_field(field, scalar, optional, slot, index)?;
        if emitted.required {
            schema.required_count = next_required(schema.required_count, index)?;
        }
        slot = next_slot(slot, index)?;
        schema.fields.push(emitted);
    }
    Ok(schema)
}

/// Preserve the complete authoritative diagnostic, including its code and
/// schema/field context. Translate a dense field id back past skipped fields.
fn canonical_fingerprint(input: &SourceInput, schema: &SchemaIr) -> Result<u64, SourceError> {
    jmax_schema_aot::fingerprint_schema(schema).map_err(|error| {
        let field = input.fields.iter().position(|field|
            !matches!(field.ty, FieldType::Skipped) && field.slot == Some(error.field_id));
        SourceError::new(field, format!("JsonSchema canonical fingerprint failed: {error}"))
    })
}

fn option_number<T: fmt::Display>(value: Option<T>, suffix: &str) -> String {
    match value {
        Some(value) => format!("::core::option::Option::Some({value}{suffix})"),
        None => String::from("::core::option::Option::None"),
    }
}

fn owned_string(value: &str) -> String {
    format!("::alloc::string::String::from({})", jmax_schema_aot::rust_string_literal(value))
}

fn option_string(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("::core::option::Option::Some({})", owned_string(value)),
        None => String::from("::core::option::Option::None"),
    }
}

fn scalar_kind_source(value: Option<ScalarKind>, abi: &str) -> String {
    let name = match value {
        None => return String::from("::core::option::Option::None"),
        Some(ScalarKind::String) => "String",
        Some(ScalarKind::Number) => "Number",
        Some(ScalarKind::True) => "True",
        Some(ScalarKind::False) => "False",
        Some(ScalarKind::Null) => "Null",
    };
    format!("::core::option::Option::Some({abi}::syntax::ScalarKind::{name})")
}

fn render(input: &SourceInput) -> Result<String, SourceError> {
    let schema = build_schema(input)?;
    // Fallible canonicalization happens only during macro expansion. Runtime
    // access to the infallible fingerprint method is just a typed literal.
    let fingerprint = canonical_fingerprint(input, &schema)?;
    let abi = &input.abi;
    let mut source = format!(
        "impl {} {abi}::traits::SchemaProvider for {} {} {} {{\n\
         fn schema_ir() -> {abi}::schema::SchemaIr {{\n\
         {abi}::schema::SchemaIr {{\n\
         name: {},\n\
         schema_id: {abi}::schema::SchemaId({}u32),\n\
         required_count: {}u16,\n\
         reject_unknown: {},\n\
         fields: ::alloc::vec![\n",
        input.impl_generics, input.ident, input.type_generics, input.where_clause,
        owned_string(&schema.name), schema.schema_id.0, schema.required_count, schema.reject_unknown,
    );
    for field in &schema.fields {
        source.push_str(&format!(
            "{abi}::schema::SchemaFieldIr {{\n\
             field_id: {abi}::schema::FieldId({}u32),\n\
             name: {},\n\
             required: {},\n\
             scalar_kind: {},\n\
             format: {},\n\
             min_len: {},\n\
             max_len: {},\n\
             min_i64: {},\n\
             max_i64: {},\n\
             }},\n",
            field.field_id.0, owned_string(&field.name), field.required,
            scalar_kind_source(field.scalar_kind, abi), option_string(field.format.as_deref()),
            option_number(field.min_len, "u32"), option_number(field.max_len, "u32"),
            option_number(field.min_i64, "i64"), option_number(field.max_i64, "i64"),
        ));
    }
    source.push_str(&format!(
        "]\n}}\n}}\nfn schema_fingerprint() -> u64 {{ {fingerprint}u64 }}\n}}\n",
    ));
    Ok(source)
}
