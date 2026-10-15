//! `JsonDecode` expansion using the canonical, owner-checked AOT readers.

use super::*;

/// Emit only the implementation. Parsing and attribute validation belong to
/// `attributes`; this module also checks the slot layout it consumes.
pub(crate) fn expand(input: &DeriveInput) -> DeriveResult<TokenStream> {
    let fields = match &input.shape {
        ItemShape::NamedStruct(fields) => Some(fields.iter().map(|field| SourceField {
            ident: field.ident.to_string(),
            rust_type: field.rust_type.to_string(),
            ty: field.ty.clone(),
            slot: field.slot,
            default: field.attrs.default,
        }).collect()),
        ItemShape::UnitStruct => None,
        ItemShape::Enum(_) => return Err(DeriveError::new(
            input.span, "JsonDecode supports only named structs and unit structs",
        )),
    };
    let source = SourceInput {
        ident: input.ident.to_string(),
        impl_generics: input.impl_generics.to_string(),
        type_generics: input.type_generics.to_string(),
        where_clause: input.where_clause.to_string(),
        abi: input.attrs.abi_path.to_string(),
        aot: input.attrs.aot_path.to_string(),
        fields,
    };
    let source = render(&source).map_err(|error| {
        let span = match (&input.shape, error.field) {
            (ItemShape::NamedStruct(fields), Some(index)) =>
                fields.get(index).map_or(input.span, |field| field.span),
            _ => input.span,
        };
        DeriveError::new(span, error.message)
    })?;
    parse_generated(&source, input.span)
}

// Keep rendering independent of proc_macro's compiler-only bridge. Ordinary
// unit tests can exercise the same renderer without constructing compiler spans.
#[derive(Clone, Debug)]
struct SourceField {
    ident: String,
    rust_type: String,
    ty: FieldType,
    slot: Option<u32>,
    default: bool,
}

#[derive(Clone, Debug)]
struct SourceInput {
    ident: String,
    impl_generics: String,
    type_generics: String,
    where_clause: String,
    abi: String,
    aot: String,
    /// None denotes a unit struct; Some(empty) denotes a braced empty struct.
    fields: Option<Vec<SourceField>>,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceError {
    field: Option<usize>,
    message: String,
}

/// Check dense declaration-order slots without truncating a platform-sized
/// field count. The checked increment also rejects more than u32::MAX slots.
fn slot_count(fields: &[SourceField]) -> Result<u32, SourceError> {
    let mut count = 0u32;
    for (index, field) in fields.iter().enumerate() {
        if matches!(field.ty, FieldType::Skipped) {
            if field.slot.is_some() {
                return Err(SourceError {
                    field: Some(index),
                    message: "a skipped JsonDecode field must not consume a slot".into(),
                });
            }
        } else {
            if field.slot != Some(count) {
                return Err(SourceError {
                    field: Some(index),
                    message: "JsonDecode slots must be dense and follow field declaration order".into(),
                });
            }
            count = count.checked_add(1).ok_or_else(|| SourceError {
                field: Some(index),
                message: "JsonDecode slot count exceeds u32::MAX".into(),
            })?;
        }
    }
    Ok(count)
}

/// The narrow decoders perform checked conversions, and the string decoder
/// resolves and validates the StrRef through its owning SlotReader.
fn scalar_decoder(scalar: ScalarType) -> (&'static str, &'static str) {
    match scalar {
        ScalarType::Bool => ("Bool", "decode_bool"),
        ScalarType::U32 => ("U64", "decode_u32"),
        ScalarType::U64 => ("U64", "decode_u64"),
        ScalarType::I32 => ("I64", "decode_i32"),
        ScalarType::I64 => ("I64", "decode_i64"),
        ScalarType::F64 => ("F64", "decode_f64"),
        ScalarType::String => ("Str", "decode_str"),
        ScalarType::Decimal128 => ("Decimal", "decode_decimal"),
    }
}

fn implementation_where(input: &SourceInput) -> String {
    let existing = input.where_clause.trim();
    let mut clause = if existing.is_empty() {
        String::from("where ")
    } else {
        let separator = if existing.ends_with(',') { " " } else { ", " };
        format!("{existing}{separator}")
    };
    clause.push_str(&format!("Self: {}::traits::SchemaProvider", input.abi));
    for field in input.fields.as_deref().unwrap_or(&[]) {
        if field.default || matches!(field.ty, FieldType::Skipped) {
            clause.push_str(&format!(", {}: ::core::default::Default", field.rust_type));
        }
    }
    clause
}

/// Procedural macro output has call-site hygiene. Avoid names present anywhere
/// in the input, including const generic parameters and array length expressions.
fn local_names(input: &SourceInput) -> (String, String, String) {
    let mut occupied = format!("{} {} {} {} {} {}", input.ident,
        input.impl_generics, input.type_generics, input.where_clause, input.abi, input.aot);
    for field in input.fields.as_deref().unwrap_or(&[]) {
        occupied.push_str(&field.ident);
        occupied.push_str(&field.rust_type);
    }
    // Appending underscores must eventually exceed the length of this finite
    // input, so there is no numeric counter that could overflow.
    let mut prefix = String::from("__jmax_decode");
    while occupied.contains(&prefix) {
        prefix.push('_');
    }
    (format!("{prefix}_slots"), format!("{prefix}_reader"), format!("{prefix}_value"))
}

fn missing_required(abi: &str, slot: u32) -> String {
    format!("return ::core::result::Result::Err({abi}::error::Error::new(\
        {abi}::error::ErrorCode::MissingRequiredField, \
        {abi}::error::ErrorSource::Decode, 0u32, \
        \"required decode slot is empty\").with_schema(0u32, {slot}u32))")
}

fn render(input: &SourceInput) -> Result<String, SourceError> {
    let fields = input.fields.as_deref().unwrap_or(&[]);
    let count = slot_count(fields)?;
    let (slots, reader, value) = local_names(input);
    let abi = &input.abi;
    let aot = &input.aot;
    let clause = implementation_where(input);
    let mut source = format!(
        "impl {} {abi}::traits::JsonDecode for {} {} {clause} {{\n\
         fn decode_slots({slots}: &{abi}::schema::ValidatedSlots) \
         -> {abi}::error::Result<Self> {{\n\
         let {reader} = {aot}::SlotReader::new({slots}, \
         <Self as {abi}::traits::SchemaProvider>::schema_fingerprint())?;\n\
         if ::core::convert::TryFrom::try_from({reader}.values().slots.len()).ok() \
         != ::core::option::Option::Some({count}u32) {{\n\
         return ::core::result::Result::Err({abi}::error::Error::new(\
         {abi}::error::ErrorCode::SchemaImageInvalid, \
         {abi}::error::ErrorSource::Decode, 0u32, \
         \"decode slot count differs from struct field count\"));\n\
         }}\n",
        input.impl_generics, input.ident, input.type_generics,
    );
    if input.fields.is_none() {
        source.push_str("::core::result::Result::Ok(Self)\n");
    } else {
        source.push_str("::core::result::Result::Ok(Self {\n");
        // Rust evaluates these field expressions in this order. Each decoder
        // propagates errors before a later field or its Default is evaluated.
        for field in fields {
            source.push_str(&field.ident);
            source.push_str(": ");
            let (scalar, optional) = match &field.ty {
                FieldType::Skipped => {
                    source.push_str(&format!(
                        "<{} as ::core::default::Default>::default(),\n", field.rust_type,
                    ));
                    continue;
                }
                FieldType::Scalar(scalar) => (*scalar, false),
                FieldType::Optional(scalar) => (*scalar, true),
            };
            // slot_count has already certified this exact index. Keep a
            // diagnostic here as well rather than assuming with an unwrap.
            let slot = field.slot.ok_or_else(|| SourceError {
                field: None,
                message: "a selected JsonDecode field is missing its slot index".into(),
            })?;
            let required = !optional && !field.default;
            let (kind, decoder) = scalar_decoder(scalar);
            let call = format!(
                "{aot}::decode::{decoder}(&{reader}, &{aot}::SlotSpec {{ \
                 index: {slot}u32, field_id: {abi}::schema::FieldId({slot}u32), \
                 kind: {aot}::SlotKind::{kind}, required: {required} }})?",
            );
            if optional {
                source.push_str(&call);
                if scalar == ScalarType::String {
                    source.push_str(".map(::alloc::string::String::from)");
                }
            } else {
                let present = if scalar == ScalarType::String {
                    format!("::alloc::string::String::from({value})")
                } else {
                    value.clone()
                };
                let absent = if field.default {
                    format!("<{} as ::core::default::Default>::default()", field.rust_type)
                } else {
                    missing_required(abi, slot)
                };
                source.push_str(&format!(
                    "match {call} {{ ::core::option::Option::Some({value}) => {present}, \
                     ::core::option::Option::None => {{ {absent} }} }}",
                ));
            }
            source.push_str(",\n");
        }
        source.push_str("})\n");
    }
    source.push_str("}\n}\n");
    Ok(source)
}
