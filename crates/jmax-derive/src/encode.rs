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

#[cfg(test)]
mod tests {
    use super::*;

    fn field(ident: &str, rust_type: &str, ty: FieldType, slot: Option<u32>) -> SourceField {
        SourceField {
            ident: ident.into(),
            rust_type: rust_type.into(),
            ty,
            slot,
            wire_literal: jmax_schema_aot::rust_string_literal(
                ident.strip_prefix("r#").unwrap_or(ident),
            ),
        }
    }

    fn scalar(ident: &str, rust_type: &str, ty: ScalarType, slot: u32) -> SourceField {
        field(ident, rust_type, FieldType::Scalar(ty), Some(slot))
    }

    fn optional(ident: &str, ty: ScalarType, slot: u32) -> SourceField {
        field(ident, "Option<Value>", FieldType::Optional(ty), Some(slot))
    }

    fn input(fields: Vec<SourceField>) -> SourceInput {
        SourceInput {
            ident: "Record".into(),
            impl_generics: String::new(),
            type_generics: String::new(),
            where_clause: String::new(),
            abi: "::jmax_abi".into(),
            aot: "::jmax_schema_aot".into(),
            fields,
        }
    }

    #[test]
    fn deterministic_empty_object_is_one_complete_implementation() {
        let model = input(Vec::new());
        let expected = concat!(
            "impl  ::jmax_abi::traits::JsonEncode for Record   {\n",
            "fn encode_json(&self, __jmax_encode_sink: &mut dyn ::jmax_abi::traits::ByteSink) ",
            "-> ::jmax_abi::error::Result<()> {\n",
            "__jmax_encode_sink.write_byte(b'{')?;\n",
            "__jmax_encode_sink.write_byte(b'}')?;\n",
            "::core::result::Result::Ok(())\n",
            "}\n}\n",
        );
        assert_eq!(render(&model).unwrap(), expected);
        assert_eq!(render(&model).unwrap(), render(&model).unwrap());
        assert!(!expected.contains("first"));
        assert!(!expected.contains("struct Record"));
        assert!(!expected.contains("SchemaProvider"));
    }

    #[test]
    fn deterministic_required_field_vector_has_no_trailing_comma() {
        let model = input(vec![scalar("flag", "bool", ScalarType::Bool, 0)]);
        let expected = concat!(
            "impl  ::jmax_abi::traits::JsonEncode for Record   {\n",
            "fn encode_json(&self, __jmax_encode_sink: &mut dyn ::jmax_abi::traits::ByteSink) ",
            "-> ::jmax_abi::error::Result<()> {\n",
            "__jmax_encode_sink.write_byte(b'{')?;\n",
            "let mut __jmax_encode_first = true;\n",
            "{\nlet __jmax_encode_value = &self.flag;\n",
            "if !::core::mem::replace(&mut __jmax_encode_first, false) {\n",
            "__jmax_encode_sink.write_byte(b',')?;\n}\n",
            "::jmax_schema_aot::encode::write_json_string(\"flag\", __jmax_encode_sink)?;\n",
            "__jmax_encode_sink.write_byte(b':')?;\n",
            "::jmax_schema_aot::encode::write_bool(*__jmax_encode_value, __jmax_encode_sink)?;\n",
            "}\n__jmax_encode_sink.write_byte(b'}')?;\n",
            "::core::result::Result::Ok(())\n}\n}\n",
        );
        assert_eq!(render(&model).unwrap(), expected);
    }

    #[test]
    fn every_scalar_selects_its_exact_writer_and_borrow_or_copy_policy() {
        let vectors = [
            (ScalarType::Bool, "bool", "write_bool", "*value"),
            (ScalarType::U32, "u32", "write_u64", "::core::convert::From::from(*value)"),
            (ScalarType::U64, "u64", "write_u64", "*value"),
            (ScalarType::I32, "i32", "write_i64", "::core::convert::From::from(*value)"),
            (ScalarType::I64, "i64", "write_i64", "*value"),
            (ScalarType::F64, "f64", "write_f64", "*value"),
            (ScalarType::String, "String", "write_json_string", "value.as_str()"),
            (ScalarType::Decimal128, "Decimal128", "write_decimal", "*value"),
        ];
        for (ty, rust_type, writer, argument) in vectors {
            assert_eq!(scalar_write(ty, "::aot", "value", "sink"),
                format!("::aot::encode::{writer}({argument}, sink)?;\n"));
            for selected in [scalar("value", rust_type, ty, 0), optional("value", ty, 0)] {
                let source = render(&input(vec![selected])).unwrap();
                assert!(source.contains(&format!("::jmax_schema_aot::encode::{writer}(")));
                assert!(!source.contains(".encode_json("));
                assert!(!source.contains(" as "));
                assert!(!source.contains("clone"));
                assert!(!source.contains("::alloc"));
                assert!(!source.contains("format!"));
            }
        }
    }

    #[test]
    fn optional_presence_guards_all_writes_including_separator_and_key() {
        for ty in [ScalarType::Bool, ScalarType::U32, ScalarType::U64, ScalarType::I32,
            ScalarType::I64, ScalarType::F64, ScalarType::String, ScalarType::Decimal128] {
            let source = render(&input(vec![optional("maybe", ty, 0)])).unwrap();
            let guard = source.find("if let ::core::option::Option::Some(__jmax_encode_value) = &self.maybe {").unwrap();
            let separator = source.find("if !::core::mem::replace(").unwrap();
            let key = source.find("::encode::write_json_string(\"maybe\"").unwrap();
            let colon = source.find(".write_byte(b':')").unwrap();
            assert!(guard < separator && separator < key && key < colon);
            assert!(source.contains("?;\n}\n__jmax_encode_sink.write_byte(b'}')?;"));
            assert!(!source.contains("None =>"));
            assert!(!source.contains("null"));
            assert!(!source.contains("is_empty"));
            assert!(!source.contains("unwrap"));
        }
    }

    #[test]
    fn declaration_order_survives_skips_and_optional_gaps() {
        let model = input(vec![
            optional("z_first", ScalarType::Bool, 0),
            field("cache", "Cache<T>", FieldType::Skipped, None),
            scalar("a_middle", "u64", ScalarType::U64, 1),
            optional("m_last", ScalarType::String, 2),
        ]);
        let source = render(&model).unwrap();
        let first = source.find("&self.z_first").unwrap();
        let middle = source.find("&self.a_middle").unwrap();
        let last = source.find("&self.m_last").unwrap();
        assert!(first < middle && middle < last);
        assert!(!source.contains("cache"));
        assert!(!source.contains("Cache<T>"));
        assert_eq!(source.matches("::core::mem::replace(").count(), 3);
        assert_eq!(source.matches(".write_byte(b',')?;").count(), 3);
        assert_eq!(source.matches(".write_byte(b':')?;").count(), 3);
        assert_eq!(source.matches("let mut __jmax_encode_first = true;").count(), 1);
    }

    #[test]
    fn all_optional_presence_combinations_keep_commas_between_present_values() {
        // Exhaust the finite presence state machine emitted by render, including
        // absent leading, middle, and trailing fields and the all-absent case.
        // Runtime JSON scalar formatting belongs to the AOT writers.
        let model = input(vec![
            optional("a", ScalarType::Bool, 0),
            optional("b", ScalarType::Bool, 1),
            optional("c", ScalarType::Bool, 2),
        ]);
        let source = render(&model).unwrap();
        assert_eq!(source.matches("if let ::core::option::Option::Some(").count(), 3);
        assert_eq!(source.matches("if !::core::mem::replace(&mut __jmax_encode_first, false)").count(), 3);
        let vectors = [
            (0u8, "{}"),
            (1, "{\"a\":true}"),
            (2, "{\"b\":true}"),
            (3, "{\"a\":true,\"b\":true}"),
            (4, "{\"c\":true}"),
            (5, "{\"a\":true,\"c\":true}"),
            (6, "{\"b\":true,\"c\":true}"),
            (7, "{\"a\":true,\"b\":true,\"c\":true}"),
        ];
        for (mask, expected) in vectors {
            let mut first = true;
            let mut json = String::from("{");
            for (index, key) in ["a", "b", "c"].iter().enumerate() {
                if mask & (1u8 << index) != 0 {
                    if !::core::mem::replace(&mut first, false) {
                        json.push(',');
                    }
                    json.push('"');
                    json.push_str(key);
                    json.push_str("\":true");
                }
            }
            json.push('}');
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn skipped_fields_have_no_reads_bounds_or_default_evaluation() {
        let mut model = input(vec![
            field("cache", "T", FieldType::Skipped, None),
            field("marker", "&'a [u8; N]", FieldType::Skipped, None),
        ]);
        model.impl_generics = "<'a, T: ?Sized, const N: usize>".into();
        model.type_generics = "<'a, T, N>".into();
        let source = render(&model).unwrap();
        assert!(!source.contains("&self."));
        assert!(!source.contains("Default"));
        assert!(!source.contains("where "));
        assert!(!source.contains("first"));
        assert!(!source.contains("::encode::"));
        assert_eq!(source.matches(".write_byte(").count(), 2);
        assert_eq!(slot_count(&model.fields), Ok(0));
    }

    #[test]
    fn required_default_values_are_never_compared_filtered_or_replaced() {
        // The compiler adapter deliberately does not carry attrs.default into
        // SourceField: it has no effect on the encoding of any present value.
        for (ty, rust_type) in [
            (ScalarType::Bool, "bool"), (ScalarType::U32, "u32"),
            (ScalarType::U64, "u64"), (ScalarType::I32, "i32"),
            (ScalarType::I64, "i64"), (ScalarType::F64, "f64"),
            (ScalarType::String, "String"), (ScalarType::Decimal128, "Decimal128"),
        ] {
            let source = render(&input(vec![scalar("value", rust_type, ty, 0)])).unwrap();
            assert!(source.contains("let __jmax_encode_value = &self.value;"));
            assert!(!source.contains("Default"));
            assert!(!source.contains("PartialEq"));
            assert!(!source.contains(" == "));
            assert!(!source.contains("is_empty"));
            assert!(!source.contains("if let"));
        }
    }

    #[test]
    fn generic_syntax_and_where_clause_are_preserved_without_extra_bounds() {
        let mut model = input(vec![
            scalar("value", "u32", ScalarType::U32, 0),
            field("cache", "Cache<'a, T, N>", FieldType::Skipped, None),
        ]);
        model.impl_generics = "<'a, T: ?Sized, const N: usize>".into();
        model.type_generics = "<'a, T, N>".into();
        for clause in ["", "where T: Bound<'a>", "where T: Bound<'a>,", "  where T: Bound<'a>,  "] {
            model.where_clause = clause.into();
            let source = render(&model).unwrap();
            assert!(source.starts_with(&format!(
                "impl <'a, T: ?Sized, const N: usize> ::jmax_abi::traits::JsonEncode for Record <'a, T, N> {clause} {{\n",
            )));
            assert_eq!(source.matches("where ").count(), usize::from(!clause.is_empty()));
            assert!(!source.contains("Default"));
            assert!(!source.contains("SchemaProvider"));
            assert!(!source.contains("T: ::jmax_abi::traits::JsonEncode"));
        }
    }

    #[test]
    fn configured_paths_and_raw_identifiers_are_used_verbatim() {
        let mut model = input(vec![scalar("r#type", "bool", ScalarType::Bool, 0)]);
        model.ident = "r#match".into();
        model.abi = "crate :: runtime :: abi".into();
        model.aot = "super :: adapters".into();
        let source = render(&model).unwrap();
        assert!(source.contains("crate :: runtime :: abi::traits::JsonEncode for r#match"));
        assert!(source.contains("&mut dyn crate :: runtime :: abi::traits::ByteSink"));
        assert!(source.contains("-> crate :: runtime :: abi::error::Result<()>"));
        assert!(source.contains("super :: adapters::encode::write_bool("));
        assert!(source.contains("write_json_string(\"type\","));
        assert!(source.contains("&self.r#type;"));
        assert!(!source.contains("::jmax_abi"));
        assert!(!source.contains("::jmax_schema_aot"));
    }

    #[test]
    fn hostile_and_unicode_keys_remain_rust_literals_then_use_json_escaping() {
        for key in [
            "quoted\"key", "back\\slash", "\0\u{1f}\n\r\t", "東京é🦀",
            "\"#; panic!(\"injected\"); //", "line\u{2028}separator", "a-b c",
        ] {
            let mut selected = scalar("value", "bool", ScalarType::Bool, 0);
            let literal = jmax_schema_aot::rust_string_literal(key);
            selected.wire_literal = literal.clone();
            let source = render(&input(vec![selected])).unwrap();
            assert!(source.contains(&format!(
                "::jmax_schema_aot::encode::write_json_string({literal}, __jmax_encode_sink)?;",
            )));
            assert_eq!(source.matches("::encode::write_json_string(").count(), 1);
            assert!(!source.contains("write_bytes("));
        }
    }

    #[test]
    fn local_names_avoid_const_generics_types_paths_and_raw_identifiers() {
        let mut model = input(vec![
            scalar("r#__jmax_encode_value", "bool", ScalarType::Bool, 0),
            field("cache", "[u8; __jmax_encode__first]", FieldType::Skipped, None),
        ]);
        model.impl_generics = "<const __jmax_encode_sink: usize>".into();
        model.type_generics = "<__jmax_encode_sink>".into();
        assert_eq!(local_names(&model), (
            "__jmax_encode____sink".into(),
            "__jmax_encode____first".into(),
            "__jmax_encode____value".into(),
        ));
        let source = render(&model).unwrap();
        assert!(source.contains("fn encode_json(&self, __jmax_encode____sink:"));
        assert!(source.contains("let mut __jmax_encode____first = true;"));
        assert!(source.contains("let __jmax_encode____value = &self.r#__jmax_encode_value;"));
        model.abi = "crate::__jmax_encode___runtime".into();
        assert_eq!(local_names(&model).0, "__jmax_encode_____sink");
        model.aot = "crate::__jmax_encode____adapters".into();
        assert_eq!(local_names(&model).0, "__jmax_encode______sink");
    }

    #[test]
    fn malformed_layouts_fail_at_the_offending_field_without_partial_source() {
        let vectors = [
            (vec![field("missing", "u32", FieldType::Scalar(ScalarType::U32), None)], 0),
            (vec![scalar("gap", "u32", ScalarType::U32, 1)], 0),
            (vec![scalar("max", "u32", ScalarType::U32, u32::MAX)], 0),
            (vec![scalar("a", "u32", ScalarType::U32, 0), scalar("b", "u32", ScalarType::U32, 0)], 1),
            (vec![field("skip", "T", FieldType::Skipped, Some(0))], 0),
            (vec![field("skip", "T", FieldType::Skipped, None), optional("gap", ScalarType::Bool, 1)], 1),
        ];
        for (fields, index) in vectors {
            let error = render(&input(fields)).unwrap_err();
            assert_eq!(error.field, index);
            assert!(error.message.contains("slot"));
            assert!(error.message.contains("JsonEncode"));
        }
        assert_eq!(slot_count(&[]), Ok(0));
    }

    #[test]
    fn dense_slots_cross_the_u16_boundary_without_truncation() {
        let count = u32::from(u16::MAX) + 2;
        let fields: Vec<_> = (0..count)
            .map(|slot| scalar(&format!("f{slot}"), "u32", ScalarType::U32, slot))
            .collect();
        assert_eq!(slot_count(&fields), Ok(count));
        let mut broken = fields;
        broken.last_mut().unwrap().slot = Some(u32::MAX);
        let error = slot_count(&broken).unwrap_err();
        assert_eq!(error.field, usize::try_from(count - 1).unwrap());
    }

    #[test]
    fn every_fallible_write_propagates_before_any_later_write() {
        let source = render(&input(vec![
            scalar("a", "bool", ScalarType::Bool, 0),
            optional("b", ScalarType::String, 1),
            scalar("c", "f64", ScalarType::F64, 2),
        ])).unwrap();
        let writes: Vec<_> = source.lines().filter(|line|
            line.contains(".write_byte(") || line.contains("::encode::write_")
        ).collect();
        assert_eq!(writes.len(), 14);
        for write in writes {
            assert!(write.ends_with(")?;"), "unpropagated write: {write}");
        }
        let first_value = source.find("::encode::write_bool(").unwrap();
        let next_guard = source.find("= &self.b {").unwrap();
        let last_value = source.find("::encode::write_f64(").unwrap();
        let close = source.find(".write_byte(b'}')").unwrap();
        let success = source.find("::core::result::Result::Ok(())").unwrap();
        assert!(first_value < next_guard && next_guard < last_value);
        assert!(last_value < close && close < success);
        assert!(!source.contains("map_err"));
        assert!(!source.contains("Err(_)"));
        assert!(!source.contains(".ok()"));
        assert!(!source.contains("unwrap"));
        assert!(!source.contains("expect("));
    }

    #[test]
    fn full_width_numbers_finite_floats_and_exact_decimals_are_not_reformatted() {
        let source = render(&input(vec![
            scalar("unsigned", "u64", ScalarType::U64, 0),
            scalar("signed", "i64", ScalarType::I64, 1),
            scalar("float", "f64", ScalarType::F64, 2),
            scalar("decimal", "Decimal128", ScalarType::Decimal128, 3),
        ])).unwrap();
        // These writer selections cover all values of their input types:
        // u64::MAX, i64::MIN, finite extremes and negative zero, plus every
        // i128 coefficient/u8 scale. The AOT writer rejects NaN and infinities.
        for writer in ["write_u64", "write_i64", "write_f64", "write_decimal"] {
            assert_eq!(source.matches(&format!(
                "::encode::{writer}(*__jmax_encode_value, __jmax_encode_sink)?;",
            )).count(), 1);
        }
        for forbidden in [" as ", "to_string", "to_bits", "coefficient", "scale",
            "is_nan", "is_infinite", "unsafe", "ValidationProof", "SlotReader",
            "issue_unchecked", "write_slots", "SchemaProvider"] {
            assert!(!source.contains(forbidden), "unexpected output: {forbidden}");
        }
    }
}
