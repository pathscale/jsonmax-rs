//! `JsonEncode` expansion using the canonical AOT JSON writers.

use super::*;

/// Emit a single implementation without inspecting values, creating proofs, or
/// adding bounds to the consuming type. Attribute parsing belongs to the root's
/// input parser; the renderer checks the shared declaration-order slot layout.
pub(crate) fn expand(input: &DeriveInput) -> DeriveResult<TokenStream> {
    let fields = match &input.shape {
        ItemShape::NamedStruct(fields) => fields.iter().map(|field| SourceField {
            ident: field.ident.to_string(),
            rust_type: field.rust_type.to_string(),
            ty: field.ty.clone(),
            slot: field.slot,
            wire_literal: rust_string(&field.wire_name),
        }).collect(),
        ItemShape::UnitStruct => Vec::new(),
        ItemShape::Enum(_) => return Err(DeriveError::new(
            input.span, "JsonEncode supports only named structs and unit structs",
        )),
    };
    let model = SourceInput {
        ident: input.ident.to_string(),
        impl_generics: input.impl_generics.to_string(),
        type_generics: input.type_generics.to_string(),
        where_clause: input.where_clause.to_string(),
        abi: input.attrs.abi_path.to_string(),
        aot: input.attrs.aot_path.to_string(),
        fields,
    };
    let source = render(&model).map_err(|error| {
        let span = match &input.shape {
            ItemShape::NamedStruct(fields) =>
                fields.get(error.field).map_or(input.span, |field| field.span),
            _ => input.span,
        };
        DeriveError::new(span, error.message)
    })?;
    parse_generated(&source, input.span)
}

// Rendering does not use proc_macro's compiler-only bridge. Tests below can
// exercise this exact generation path without manufacturing compiler spans.
#[derive(Clone, Debug)]
struct SourceField {
    ident: String,
    rust_type: String,
    ty: FieldType,
    slot: Option<u32>,
    /// A Rust string literal, produced by rust_string at the compiler boundary.
    /// JSON quoting is deferred to write_json_string at runtime.
    wire_literal: String,
}

#[derive(Clone, Debug)]
struct SourceInput {
    ident: String,
    impl_generics: String,
    type_generics: String,
    where_clause: String,
    abi: String,
    aot: String,
    fields: Vec<SourceField>,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceError {
    field: usize,
    message: String,
}

/// Encoding does not read slots, but must agree with decoding and schema
/// generation about which fields participate and their declaration order.
fn slot_count(fields: &[SourceField]) -> Result<u32, SourceError> {
    let mut count = 0u32;
    for (index, field) in fields.iter().enumerate() {
        if matches!(field.ty, FieldType::Skipped) {
            if field.slot.is_some() {
                return Err(SourceError {
                    field: index,
                    message: "a skipped JsonEncode field must not consume a slot".into(),
                });
            }
        } else {
            if field.slot != Some(count) {
                return Err(SourceError {
                    field: index,
                    message: "JsonEncode slots must be dense and follow field declaration order".into(),
                });
            }
            count = count.checked_add(1).ok_or_else(|| SourceError {
                field: index,
                message: "JsonEncode slot count exceeds u32::MAX".into(),
            })?;
        }
    }
    Ok(count)
}

/// Call-site tokens share the caller's value namespace, including const
/// generics. Find a prefix absent from all interpolated Rust tokens before
/// choosing parameters or locals. Underscores avoid a bounded numeric counter.
fn local_names(input: &SourceInput) -> (String, String, String) {
    let mut occupied = format!("{} {} {} {} {} {}", input.ident,
        input.impl_generics, input.type_generics, input.where_clause, input.abi, input.aot);
    for field in &input.fields {
        occupied.push_str(&field.ident);
        occupied.push_str(&field.rust_type);
    }
    let mut prefix = String::from("__jmax_encode");
    while occupied.contains(&prefix) {
        prefix.push('_');
    }
    (format!("{prefix}_sink"), format!("{prefix}_first"), format!("{prefix}_value"))
}

/// Every primitive goes through its typed writer, without relying on primitive
/// JsonEncode implementations. Narrow integers widen exactly through From.
/// Strings remain borrowed; the ABI decimal is Copy and is never rescaled.
fn scalar_write(scalar: ScalarType, aot: &str, value: &str, sink: &str) -> String {
    let (writer, argument) = match scalar {
        ScalarType::Bool => ("write_bool", format!("*{value}")),
        ScalarType::U32 => ("write_u64", format!("::core::convert::From::from(*{value})")),
        ScalarType::U64 => ("write_u64", format!("*{value}")),
        ScalarType::I32 => ("write_i64", format!("::core::convert::From::from(*{value})")),
        ScalarType::I64 => ("write_i64", format!("*{value}")),
        ScalarType::F64 => ("write_f64", format!("*{value}")),
        ScalarType::String => ("write_json_string", format!("{value}.as_str()")),
        ScalarType::Decimal128 => ("write_decimal", format!("*{value}")),
    };
    format!("{aot}::encode::{writer}({argument}, {sink})?;\n")
}

fn render(input: &SourceInput) -> Result<String, SourceError> {
    let count = slot_count(&input.fields)?;
    let (sink, first, value) = local_names(input);
    let abi = &input.abi;
    let aot = &input.aot;
    let mut source = format!(
        "impl {} {abi}::traits::JsonEncode for {} {} {} {{\n\
         fn encode_json(&self, {sink}: &mut dyn {abi}::traits::ByteSink) \
         -> {abi}::error::Result<()> {{\n\
         {sink}.write_byte(b'{{')?;\n",
        input.impl_generics, input.ident, input.type_generics, input.where_clause,
    );
    if count != 0 {
        source.push_str(&format!("let mut {first} = true;\n"));
    }
    for field in &input.fields {
        let scalar = match &field.ty {
            FieldType::Skipped => continue,
            FieldType::Scalar(scalar) => {
                source.push_str(&format!("{{\nlet {value} = &self.{};\n", field.ident));
                *scalar
            }
            FieldType::Optional(scalar) => {
                source.push_str(&format!(
                    "if let ::core::option::Option::Some({value}) = &self.{} {{\n",
                    field.ident,
                ));
                *scalar
            }
        };
        // This state changes only inside a field's presence guard. replace
        // avoids a final unused assignment in consumers that deny warnings.
        // Every fallible write stops immediately; subsequent fields and the
        // closing brace are unreachable following a sink or scalar error.
        source.push_str(&format!(
            "if !::core::mem::replace(&mut {first}, false) {{\n\
             {sink}.write_byte(b',')?;\n\
             }}\n\
             {aot}::encode::write_json_string({}, {sink})?;\n\
             {sink}.write_byte(b':')?;\n",
            field.wire_literal,
        ));
        source.push_str(&scalar_write(scalar, aot, &value, &sink));
        source.push_str("}\n");
    }
    source.push_str(&format!(
        "{sink}.write_byte(b'}}')?;\n::core::result::Result::Ok(())\n}}\n}}\n",
    ));
    Ok(source)
}
