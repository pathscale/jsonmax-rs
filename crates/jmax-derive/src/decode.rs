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

#[cfg(test)]
mod tests {
    use super::*;

    fn field(ident: &str, rust_type: &str, ty: FieldType, slot: Option<u32>) -> SourceField {
        SourceField {
            ident: ident.into(), rust_type: rust_type.into(), ty, slot, default: false,
        }
    }

    fn input(fields: Option<Vec<SourceField>>) -> SourceInput {
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

    fn scalar(ident: &str, rust_type: &str, ty: ScalarType, slot: u32) -> SourceField {
        field(ident, rust_type, FieldType::Scalar(ty), Some(slot))
    }

    #[test]
    fn all_scalar_kinds_use_the_exact_frozen_aot_decoder() {
        let vectors = [
            (ScalarType::Bool, "bool", "Bool", "decode_bool"),
            (ScalarType::U32, "u32", "U64", "decode_u32"),
            (ScalarType::U64, "u64", "U64", "decode_u64"),
            (ScalarType::I32, "i32", "I64", "decode_i32"),
            (ScalarType::I64, "i64", "I64", "decode_i64"),
            (ScalarType::F64, "f64", "F64", "decode_f64"),
            (ScalarType::String, "String", "Str", "decode_str"),
            (ScalarType::Decimal128, "Decimal128", "Decimal", "decode_decimal"),
        ];
        for (ty, rust_type, kind, decoder) in vectors {
            assert_eq!(scalar_decoder(ty), (kind, decoder));
            let source = render(&input(Some(vec![scalar("value", rust_type, ty, 0)]))).unwrap();
            assert!(source.contains(&format!("::jmax_schema_aot::decode::{decoder}(")));
            assert!(source.contains(&format!("kind: ::jmax_schema_aot::SlotKind::{kind}")));
            assert!(source.contains("index: 0u32, field_id: ::jmax_abi::schema::FieldId(0u32)"));
            assert!(source.contains("required: true })?"));
            assert!(!source.contains(" as u32"));
            assert!(!source.contains(" as i32"));
        }
    }

    #[test]
    fn unit_and_braced_empty_structs_have_distinct_constructors() {
        let unit = render(&input(None)).unwrap();
        let braced = render(&input(Some(Vec::new()))).unwrap();
        assert!(unit.contains("::core::result::Result::Ok(Self)"));
        assert!(!unit.contains("Ok(Self {"));
        assert!(braced.contains("::core::result::Result::Ok(Self {\n})"));
        for source in [&unit, &braced] {
            assert!(source.contains("::core::option::Option::Some(0u32)"));
            assert!(source.contains("Self: ::jmax_abi::traits::SchemaProvider"));
            assert!(source.contains("<Self as ::jmax_abi::traits::SchemaProvider>::schema_fingerprint())?;"));
            assert_eq!(source.matches("impl ").count(), 1);
            assert!(!source.contains("struct Record"));
            assert!(!source.contains("::alloc"));
        }
    }

    #[test]
    fn deterministic_unit_source_is_a_complete_single_implementation() {
        let model = input(None);
        let expected = concat!(
            "impl  ::jmax_abi::traits::JsonDecode for Record  ",
            "where Self: ::jmax_abi::traits::SchemaProvider {\n",
            "fn decode_slots(__jmax_decode_slots: &::jmax_abi::schema::ValidatedSlots) ",
            "-> ::jmax_abi::error::Result<Self> {\n",
            "let __jmax_decode_reader = ::jmax_schema_aot::SlotReader::new(__jmax_decode_slots, ",
            "<Self as ::jmax_abi::traits::SchemaProvider>::schema_fingerprint())?;\n",
            "if ::core::convert::TryFrom::try_from(__jmax_decode_reader.values().slots.len()).ok() ",
            "!= ::core::option::Option::Some(0u32) {\n",
            "return ::core::result::Result::Err(::jmax_abi::error::Error::new(",
            "::jmax_abi::error::ErrorCode::SchemaImageInvalid, ",
            "::jmax_abi::error::ErrorSource::Decode, 0u32, ",
            "\"decode slot count differs from struct field count\"));\n",
            "}\n",
            "::core::result::Result::Ok(Self)\n",
            "}\n}\n",
        );
        assert_eq!(render(&model).unwrap(), expected);
        assert_eq!(render(&model).unwrap(), render(&model).unwrap());
    }

    #[test]
    fn proof_and_exact_count_are_checked_before_any_field_or_default() {
        let mut defaulted = scalar("count", "u32", ScalarType::U32, 0);
        defaulted.default = true;
        let source = render(&input(Some(vec![defaulted]))).unwrap();
        let proof = source.find("SlotReader::new").unwrap();
        let count = source.find(".values().slots.len()").unwrap();
        let decode = source.find("::decode::decode_u32").unwrap();
        let default = source.find("<u32 as ::core::default::Default>::default()").unwrap();
        assert!(proof < count && count < decode && decode < default);
        assert!(source.contains("!= ::core::option::Option::Some(1u32)"));
        assert!(source.contains("ErrorCode::SchemaImageInvalid"));
        assert!(source.contains("ErrorSource::Decode"));
    }

    #[test]
    fn dense_layout_ignores_skipped_fields_and_preserves_evaluation_order() {
        let model = input(Some(vec![
            scalar("first", "bool", ScalarType::Bool, 0),
            field("cache", "Cache<T>", FieldType::Skipped, None),
            scalar("last", "i64", ScalarType::I64, 1),
        ]));
        assert_eq!(slot_count(model.fields.as_ref().unwrap()), Ok(2));
        let source = render(&model).unwrap();
        assert!(source.contains("::core::option::Option::Some(2u32)"));
        assert!(source.contains("field_id: ::jmax_abi::schema::FieldId(1u32)"));
        assert!(source.contains("cache: <Cache<T> as ::core::default::Default>::default()"));
        assert_eq!(source.matches("::SlotSpec {").count(), 2);
        let first = source.find("first: match").unwrap();
        let skipped = source.find("cache: <").unwrap();
        let last = source.find("last: match").unwrap();
        assert!(first < skipped && skipped < last);
    }

    #[test]
    fn all_skipped_fields_require_defaults_but_no_slots_or_allocator() {
        let model = input(Some(vec![
            field("a", "T", FieldType::Skipped, None),
            field("b", "[u8; N]", FieldType::Skipped, None),
        ]));
        let source = render(&model).unwrap();
        assert!(source.contains("::core::option::Option::Some(0u32)"));
        assert!(source.contains(", T: ::core::default::Default, [u8; N]: ::core::default::Default"));
        assert!(!source.contains("::SlotSpec"));
        assert!(!source.contains("::alloc"));
    }

    #[test]
    fn required_empty_is_an_error_without_panics_or_default_substitution() {
        let source = render(&input(Some(vec![scalar("count", "u32", ScalarType::U32, 0)]))).unwrap();
        assert!(source.contains("required: true })?"));
        assert!(source.contains("::core::option::Option::None => { return ::core::result::Result::Err("));
        assert!(source.contains("ErrorCode::MissingRequiredField"));
        assert!(source.contains(".with_schema(0u32, 0u32)"));
        assert!(!source.contains("::default::Default"));
        assert!(!source.contains("unwrap"));
        assert!(!source.contains("expect("));
        assert!(!source.contains("panic!"));
    }

    #[test]
    fn default_is_only_in_the_empty_arm_after_fallible_decoding() {
        for (ty, rust_type, decoder) in [
            (ScalarType::U32, "u32", "decode_u32"),
            (ScalarType::I32, "i32", "decode_i32"),
            (ScalarType::F64, "f64", "decode_f64"),
            (ScalarType::String, "String", "decode_str"),
            (ScalarType::Decimal128, "Decimal128", "decode_decimal"),
        ] {
            let mut selected = scalar("value", rust_type, ty, 0);
            selected.default = true;
            let source = render(&input(Some(vec![selected]))).unwrap();
            assert!(source.contains(&format!("::decode::{decoder}(")));
            assert!(source.contains("required: false })? { ::core::option::Option::Some("));
            assert!(source.contains(&format!(
                "::core::option::Option::None => {{ <{rust_type} as ::core::default::Default>::default() }}",
            )));
            assert!(!source.contains("unwrap_or"));
            assert!(!source.contains("Err(_)"));
            // Only the checked slot-count conversion discards an error. Scalar
            // errors must propagate through `?` without becoming defaults.
            assert_eq!(source.matches(".ok()").count(), 1);
        }
    }

    #[test]
    fn optional_scalars_keep_none_and_propagate_all_decoder_errors() {
        for ty in [ScalarType::Bool, ScalarType::U32, ScalarType::U64, ScalarType::I32,
            ScalarType::I64, ScalarType::F64, ScalarType::Decimal128] {
            let selected = field("value", "Option<Value>", FieldType::Optional(ty), Some(0));
            let source = render(&input(Some(vec![selected]))).unwrap();
            assert!(source.contains("required: false })?,\n"));
            assert_eq!(source.matches("::SlotSpec {").count(), 1);
            assert!(!source.contains("MissingRequiredField"));
            assert!(!source.contains("::default::Default"));
            assert!(!source.contains("::alloc"));
        }
    }

    #[test]
    fn optional_default_adds_its_concrete_bound_and_still_preserves_none() {
        let mut selected = field("value", "Option<u32>", FieldType::Optional(ScalarType::U32), Some(0));
        selected.default = true;
        let source = render(&input(Some(vec![selected]))).unwrap();
        assert!(source.contains("Option<u32>: ::core::default::Default"));
        assert!(source.contains("required: false })?,\n"));
        assert!(!source.contains("::default()"));
        assert!(!source.contains("MissingRequiredField"));
    }

    #[test]
    fn strings_copy_only_after_the_owner_checked_utf8_decoder_succeeds() {
        let source = render(&input(Some(vec![
            scalar("required", "::alloc::string::String", ScalarType::String, 0),
            field("optional", "Option<String>", FieldType::Optional(ScalarType::String), Some(1)),
        ]))).unwrap();
        assert_eq!(source.matches("::decode::decode_str(").count(), 2);
        assert!(source.contains("::core::option::Option::Some(__jmax_decode_value) => ::alloc::string::String::from(__jmax_decode_value)"));
        assert!(source.contains("required: false })?.map(::alloc::string::String::from)"));
        assert!(!source.contains("StrRef"));
        assert!(!source.contains("from_utf8_unchecked"));
        assert!(!source.contains(".payload"));
        assert!(!source.contains(".decoded"));
    }

    #[test]
    fn generic_parameters_and_where_predicates_are_preserved_without_blanket_defaults() {
        let mut model = input(Some(vec![
            scalar("count", "u32", ScalarType::U32, 0),
            field("cache", "Cache<'a, T, N>", FieldType::Skipped, None),
        ]));
        model.impl_generics = "<'a, T: ?Sized, const N: usize>".into();
        model.type_generics = "<'a, T, N>".into();
        for existing in ["where T: Bound<'a>", "where T: Bound<'a>,", "  where T: Bound<'a>,  "] {
            model.where_clause = existing.into();
            let source = render(&model).unwrap();
            assert!(source.starts_with("impl <'a, T: ?Sized, const N: usize> ::jmax_abi::traits::JsonDecode for Record <'a, T, N>"));
            assert!(source.contains("where T: Bound<'a>, Self: ::jmax_abi::traits::SchemaProvider, Cache<'a, T, N>: ::core::default::Default"));
            assert_eq!(source.matches("where ").count(), 1);
            assert!(!source.contains("T: ::core::default::Default"));
            assert!(!source.contains("u32: ::core::default::Default"));
            assert!(!source.contains(",,"));
        }
    }

    #[test]
    fn configured_paths_and_raw_identifiers_are_used_verbatim() {
        let mut model = input(Some(vec![scalar("r#type", "bool", ScalarType::Bool, 0)]));
        model.ident = "r#match".into();
        model.abi = "crate :: runtime :: abi".into();
        model.aot = "super :: adapters".into();
        let source = render(&model).unwrap();
        assert!(source.contains("crate :: runtime :: abi::traits::JsonDecode for r#match"));
        assert!(source.contains("super :: adapters::SlotReader::new"));
        assert!(source.contains("super :: adapters::decode::decode_bool"));
        assert!(source.contains("r#type: match"));
        assert!(!source.contains("::jmax_abi"));
        assert!(!source.contains("::jmax_schema_aot"));
    }

    #[test]
    fn generated_bindings_cannot_shadow_const_generics_or_field_types() {
        let mut model = input(Some(vec![
            scalar("__jmax_decode_value", "u32", ScalarType::U32, 0),
            field("cache", "[u8; __jmax_decode__reader]", FieldType::Skipped, None),
        ]));
        model.impl_generics = "<const __jmax_decode_slots: usize>".into();
        model.type_generics = "<__jmax_decode_slots>".into();
        let names = local_names(&model);
        assert_eq!(names, ("__jmax_decode____slots".into(), "__jmax_decode____reader".into(), "__jmax_decode____value".into()));
        let source = render(&model).unwrap();
        assert!(source.contains("fn decode_slots(__jmax_decode____slots:"));
        assert!(source.contains("let __jmax_decode____reader ="));
        assert!(source.contains("::core::option::Option::Some(__jmax_decode____value)"));
        assert!(source.contains("<[u8; __jmax_decode__reader] as ::core::default::Default>"));
    }

    #[test]
    fn invalid_slot_layouts_return_precise_field_diagnostics() {
        let vectors = [
            (vec![field("missing", "u32", FieldType::Scalar(ScalarType::U32), None)], 0),
            (vec![scalar("gap", "u32", ScalarType::U32, 1)], 0),
            (vec![scalar("max", "u32", ScalarType::U32, u32::MAX)], 0),
            (vec![scalar("a", "u32", ScalarType::U32, 0), scalar("b", "u32", ScalarType::U32, 0)], 1),
            (vec![field("skipped", "T", FieldType::Skipped, Some(0))], 0),
            (vec![field("skip", "T", FieldType::Skipped, None), scalar("gap", "u32", ScalarType::U32, 1)], 1),
        ];
        for (fields, index) in vectors {
            let error = render(&input(Some(fields))).unwrap_err();
            assert_eq!(error.field, Some(index));
            assert!(!error.message.is_empty());
            assert!(error.message.contains("slot"));
        }
        assert_eq!(slot_count(&[]), Ok(0));
    }

    #[test]
    fn slot_ids_are_not_accidentally_limited_to_u16() {
        let fields: Vec<_> = (0..=u32::from(u16::MAX) + 1)
            .map(|index| scalar(&format!("f{index}"), "u32", ScalarType::U32, index))
            .collect();
        assert_eq!(slot_count(&fields), Ok(u32::from(u16::MAX) + 2));
    }

    #[test]
    fn full_width_numbers_and_finite_float_policy_are_left_to_typed_decoders() {
        let source = render(&input(Some(vec![
            scalar("unsigned", "u64", ScalarType::U64, 0),
            scalar("signed", "i64", ScalarType::I64, 1),
            scalar("float", "f64", ScalarType::F64, 2),
            scalar("decimal", "Decimal128", ScalarType::Decimal128, 3),
        ]))).unwrap();
        for decoder in ["decode_u64", "decode_i64", "decode_f64", "decode_decimal"] {
            assert!(source.contains(&format!("::decode::{decoder}(")));
        }
        assert_eq!(source.matches("required: true })?").count(), 4);
        assert!(!source.contains("from_bits"));
        assert!(!source.contains("coefficient"));
        assert!(!source.contains("scale"));
        assert!(!source.contains("unsafe"));
        assert!(!source.contains("issue_unchecked"));
    }
}
