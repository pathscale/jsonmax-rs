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

#[cfg(test)]
mod tests {
    use super::*;
    use jmax_schema_ir::{TypeNode, ValueKind};

    fn field(name: &str, ty: FieldType, slot: Option<u32>) -> SourceField {
        SourceField { ty, wire_name: name.into(), slot, attrs: FieldAttrs::default() }
    }

    fn scalar(name: &str, ty: ScalarType, slot: u32) -> SourceField {
        field(name, FieldType::Scalar(ty), Some(slot))
    }

    fn optional(name: &str, ty: ScalarType, slot: u32) -> SourceField {
        field(name, FieldType::Optional(ty), Some(slot))
    }

    fn input(fields: Vec<SourceField>) -> SourceInput {
        SourceInput {
            ident: "Record".into(),
            impl_generics: String::new(),
            type_generics: String::new(),
            where_clause: String::new(),
            abi: "::jmax_abi".into(),
            schema_name: None,
            schema_id: None,
            reject_unknown: true,
            fields,
        }
    }

    fn assert_field_equal(actual: &SchemaFieldIr, expected: &SchemaFieldIr) {
        assert_eq!(actual.field_id, expected.field_id);
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.required, expected.required);
        assert_eq!(actual.scalar_kind, expected.scalar_kind);
        assert_eq!(actual.format, expected.format);
        assert_eq!(actual.min_len, expected.min_len);
        assert_eq!(actual.max_len, expected.max_len);
        assert_eq!(actual.min_i64, expected.min_i64);
        assert_eq!(actual.max_i64, expected.max_i64);
    }

    #[test]
    fn deterministic_empty_provider_is_one_complete_implementation() {
        let model = input(Vec::new());
        let schema = build_schema(&model).unwrap();
        let fingerprint = jmax_schema_aot::fingerprint_schema(&schema).unwrap();
        let expected = format!(concat!(
            "impl  ::jmax_abi::traits::SchemaProvider for Record   {{\n",
            "fn schema_ir() -> ::jmax_abi::schema::SchemaIr {{\n",
            "::jmax_abi::schema::SchemaIr {{\n",
            "name: ::alloc::string::String::from(\"Record\"),\n",
            "schema_id: ::jmax_abi::schema::SchemaId(0u32),\n",
            "required_count: 0u16,\n",
            "reject_unknown: true,\n",
            "fields: ::alloc::vec![\n",
            "]\n}}\n}}\nfn schema_fingerprint() -> u64 {{ {}u64 }}\n}}\n",
        ), fingerprint);
        assert_eq!(render(&model).unwrap(), expected);
        assert_eq!(render(&model).unwrap(), render(&model).unwrap());
        assert_eq!(expected.matches("impl ").count(), 1);
        assert_eq!(expected.matches("fn ").count(), 2);
        assert!(!expected.contains("struct Record"));
    }

    #[test]
    fn empty_schema_matches_the_frozen_canonical_byte_vector() {
        // Canonical v1, default policy, name Record, schema id zero, one
        // rejecting root object with no property-count constraints or fields.
        let expected: &[u8] = b"JMAX-SCHEMA\0\
            \x01\x00\
            \x00\x00\x01\x01\x34\x01\x00\x01\x00\x10\x00\x00\
            \x06\x00\x00\x00Record\
            \x00\x00\x00\x00\
            \x01\x00\x00\x00\
            \x02\x01\x00\x00\x00\x00\x00\x00";
        let model = input(Vec::new());
        let schema = build_schema(&model).unwrap();
        let normalized = jmax_schema_ir::model::normalize(&schema).unwrap();
        let resolved = jmax_schema_ir::resolve::resolve(normalized,
            jmax_schema_ir::ResolveLimits::default()).unwrap();
        let canonical = jmax_schema_ir::canonical::canonicalize(&resolved,
            &jmax_schema_ir::SchemaPolicy::default()).unwrap();
        assert_eq!(canonical.bytes.as_slice(), expected);
        assert_eq!(canonical.fingerprint, jmax_schema_ir::canonical::fingerprint_bytes(expected));
        assert_eq!(canonical_fingerprint(&model, &schema).unwrap(), canonical.fingerprint);
    }

    #[test]
    fn raw_identifier_defaults_and_explicit_container_metadata_are_exact() {
        let mut model = input(Vec::new());
        model.ident = "r#type".into();
        let schema = build_schema(&model).unwrap();
        assert_eq!(schema.name, "type");
        assert_eq!(schema.schema_id, SchemaId(0));
        let default_source = render(&model).unwrap();
        assert!(default_source.contains("SchemaProvider for r#type"));
        assert!(default_source.contains("String::from(\"type\")"));
        model.schema_name = Some("API 東京".into());
        model.schema_id = Some(u32::MAX);
        model.reject_unknown = false;
        let schema = build_schema(&model).unwrap();
        assert_eq!(schema.name, "API 東京");
        assert_eq!(schema.schema_id, SchemaId(u32::MAX));
        assert!(!schema.reject_unknown);
        let source = render(&model).unwrap();
        assert!(source.contains("SchemaId(4294967295u32)"));
        assert!(source.contains("reject_unknown: false"));
        assert!(!source.contains("String::from(\"type\")"));
    }

    #[test]
    fn generic_tokens_where_clauses_and_configured_abi_path_are_preserved() {
        let mut model = input(vec![field("cache", FieldType::Skipped, None)]);
        model.ident = "r#match".into();
        model.impl_generics = "<'a, T: ?Sized, const N: usize>".into();
        model.type_generics = "<'a, T, N>".into();
        model.abi = "crate :: runtime :: abi".into();
        for clause in ["", "where T: Bound<'a>", "where T: Bound<'a>,"] {
            model.where_clause = clause.into();
            let source = render(&model).unwrap();
            assert!(source.starts_with(&format!(
                "impl <'a, T: ?Sized, const N: usize> crate :: runtime :: abi::traits::SchemaProvider for r#match <'a, T, N> {clause} {{\n",
            )));
            assert!(source.contains("-> crate :: runtime :: abi::schema::SchemaIr"));
            assert!(!source.contains("::jmax_abi"));
            assert!(!source.contains("Default"));
            assert!(!source.contains("cache"));
        }
    }

    #[test]
    fn supported_scalar_vectors_have_exact_intrinsic_ranges_and_categories() {
        let vectors = [
            (ScalarType::Bool, ScalarKind::True, None, None, ValueKind::Boolean),
            (ScalarType::U32, ScalarKind::Number, Some(0), Some(4_294_967_295), ValueKind::Integer),
            (ScalarType::I32, ScalarKind::Number, Some(-2_147_483_648), Some(2_147_483_647), ValueKind::Integer),
            (ScalarType::I64, ScalarKind::Number, Some(i64::MIN), Some(i64::MAX), ValueKind::Integer),
            (ScalarType::F64, ScalarKind::Number, None, None, ValueKind::Number),
            (ScalarType::String, ScalarKind::String, None, None, ValueKind::String),
        ];
        for (ty, kind, minimum, maximum, category) in vectors {
            for selected in [scalar("value", ty, 0), optional("value", ty, 0)] {
                let optional = matches!(selected.ty, FieldType::Optional(_));
                let model = input(vec![selected]);
                let schema = build_schema(&model).unwrap();
                assert_field_equal(&schema.fields[0], &SchemaFieldIr {
                    field_id: FieldId(0), name: "value".into(), required: !optional,
                    scalar_kind: Some(kind), format: None, min_len: None, max_len: None,
                    min_i64: minimum, max_i64: maximum,
                });
                assert_eq!(schema.required_count, u16::from(!optional));
                let normalized = jmax_schema_ir::model::normalize(&schema).unwrap();
                match &normalized.nodes[1] {
                    TypeNode::Scalar(constraints) => assert_eq!(constraints.kind, category),
                    other => panic!("unexpected normalized node: {other:?}"),
                }
                assert!(render(&model).is_ok());
            }
        }
    }

    #[test]
    fn optional_properties_keep_the_scalar_type_and_are_never_nullable() {
        for ty in [ScalarType::Bool, ScalarType::U32, ScalarType::I32,
            ScalarType::I64, ScalarType::F64, ScalarType::String] {
            let mut selected = optional("value", ty, 0);
            selected.attrs.default = true;
            let model = input(vec![selected]);
            let schema = build_schema(&model).unwrap();
            assert!(!schema.fields[0].required);
            assert_ne!(schema.fields[0].scalar_kind, Some(ScalarKind::Null));
            let source = render(&model).unwrap();
            assert!(source.contains("required_count: 0u16"));
            assert!(!source.contains("ScalarKind::Null"));
            assert!(!source.contains("union"));
        }
    }

    #[test]
    fn skips_defaults_and_optional_fields_share_dense_declaration_order() {
        let mut defaulted = scalar("a_default", ScalarType::I32, 1);
        defaulted.attrs.default = true;
        let mut skipped = field("cache", FieldType::Skipped, None);
        skipped.attrs.skip = true;
        skipped.attrs.default = true;
        let model = input(vec![
            scalar("z_required", ScalarType::Bool, 0), skipped, defaulted,
            optional("m_optional", ScalarType::String, 2),
            scalar("last_required", ScalarType::F64, 3),
        ]);
        let schema = build_schema(&model).unwrap();
        assert_eq!(schema.required_count, 2);
        let actual: Vec<_> = schema.fields.iter().map(|field|
            (field.field_id.0, field.name.as_str(), field.required)).collect();
        assert_eq!(actual, vec![
            (0, "z_required", true), (1, "a_default", false),
            (2, "m_optional", false), (3, "last_required", true),
        ]);
        let normalized = jmax_schema_ir::model::normalize(&schema).unwrap();
        match &normalized.nodes[0] {
            TypeNode::Object(object) => {
                for (index, property) in object.properties.iter().enumerate() {
                    assert_eq!(property.slot, Some(u32::try_from(index).unwrap()));
                    assert_eq!(property.field_id, schema.fields[index].field_id);
                    assert_eq!(property.required, schema.fields[index].required);
                }
            }
            other => panic!("unexpected root: {other:?}"),
        }
        let source = render(&model).unwrap();
        assert!(!source.contains("cache"));
        let z = source.find("String::from(\"z_required\")").unwrap();
        let a = source.find("String::from(\"a_default\")").unwrap();
        let m = source.find("String::from(\"m_optional\")").unwrap();
        assert!(z < a && a < m);
    }

    #[test]
    fn all_skipped_and_empty_structs_produce_the_same_empty_schema() {
        let empty = input(Vec::new());
        let skipped = input(vec![
            field("borrowed", FieldType::Skipped, None),
            field("generic", FieldType::Skipped, None),
        ]);
        assert!(build_schema(&skipped).unwrap().fields.is_empty());
        assert_eq!(render(&empty).unwrap(), render(&skipped).unwrap());
    }

    #[test]
    fn integer_constraint_intersection_never_widens_the_rust_range() {
        let vectors = [
            (ScalarType::U32, Some(i64::MIN), Some(i64::MAX), 0, 4_294_967_295),
            (ScalarType::U32, Some(17), Some(42), 17, 42),
            (ScalarType::U32, None, Some(0), 0, 0),
            (ScalarType::U32, Some(4_294_967_295), None, 4_294_967_295, 4_294_967_295),
            (ScalarType::I32, Some(i64::MIN), Some(i64::MAX), -2_147_483_648, 2_147_483_647),
            (ScalarType::I32, Some(-10), Some(10), -10, 10),
            (ScalarType::I32, None, Some(-2_147_483_648), -2_147_483_648, -2_147_483_648),
            (ScalarType::I64, None, None, i64::MIN, i64::MAX),
            (ScalarType::I64, Some(i64::MIN), Some(i64::MIN), i64::MIN, i64::MIN),
            (ScalarType::I64, Some(i64::MAX), Some(i64::MAX), i64::MAX, i64::MAX),
        ];
        for (ty, min_i64, max_i64, minimum, maximum) in vectors {
            let attrs = FieldAttrs { min_i64, max_i64, ..FieldAttrs::default() };
            assert_eq!(integer_bounds(ty, &attrs, 7).unwrap(), (Some(minimum), Some(maximum)));
            let mut selected = scalar("n", ty, 0);
            selected.attrs = attrs;
            let model = input(vec![selected]);
            let schema = build_schema(&model).unwrap();
            assert_eq!(schema.fields[0].min_i64, Some(minimum));
            assert_eq!(schema.fields[0].max_i64, Some(maximum));
            let source = render(&model).unwrap();
            assert!(source.contains(&format!("min_i64: ::core::option::Option::Some({minimum}i64)")));
            assert!(source.contains(&format!("max_i64: ::core::option::Option::Some({maximum}i64)")));
        }
    }

    #[test]
    fn disjoint_and_inverted_integer_constraints_are_errors_at_the_field() {
        for (ty, minimum, maximum) in [
            (ScalarType::U32, None, Some(-1)),
            (ScalarType::U32, Some(4_294_967_296), None),
            (ScalarType::I32, None, Some(-2_147_483_649)),
            (ScalarType::I32, Some(2_147_483_648), None),
            (ScalarType::I64, Some(1), Some(0)),
        ] {
            let mut selected = scalar("invalid", ty, 1);
            selected.attrs.min_i64 = minimum;
            selected.attrs.max_i64 = maximum;
            let model = input(vec![scalar("ok", ScalarType::Bool, 0), selected]);
            let error = render(&model).unwrap_err();
            assert_eq!(error.field, Some(1));
            assert!(error.message.contains("empty intersection"));
        }
    }

    #[test]
    fn u64_and_decimal_are_rejected_even_when_optional_defaulted_or_bounded() {
        for ty in [ScalarType::U64, ScalarType::Decimal128] {
            for optional in [false, true] {
                for default in [false, true] {
                    let mut selected = field("unsupported", if optional {
                        FieldType::Optional(ty)
                    } else {
                        FieldType::Scalar(ty)
                    }, Some(0));
                    selected.attrs.default = default;
                    selected.attrs.min_i64 = Some(0);
                    selected.attrs.max_i64 = Some(100);
                    let error = render(&input(vec![selected])).unwrap_err();
                    assert_eq!(error.field, Some(0));
                    assert!(error.message.contains(if ty == ScalarType::U64 { "u64" } else { "Decimal128" }));
                    assert!(error.message.contains("flat ABI IR"));
                }
            }
        }
    }

    #[test]
    fn string_format_and_length_constraints_are_preserved_without_loss() {
        let mut selected = scalar("display-name", ScalarType::String, 0);
        selected.attrs.rename = Some("display-name".into());
        selected.attrs.format = Some("uuid".into());
        selected.attrs.min_len = Some(0);
        selected.attrs.max_len = Some(u32::MAX);
        let model = input(vec![selected]);
        let schema = build_schema(&model).unwrap();
        assert_eq!(schema.fields[0].format.as_deref(), Some("uuid"));
        assert_eq!(schema.fields[0].min_len, Some(0));
        assert_eq!(schema.fields[0].max_len, Some(u32::MAX));
        let source = render(&model).unwrap();
        assert!(source.contains("format: ::core::option::Option::Some(::alloc::string::String::from(\"uuid\"))"));
        assert!(source.contains("min_len: ::core::option::Option::Some(0u32)"));
        assert!(source.contains("max_len: ::core::option::Option::Some(4294967295u32)"));
        assert!(source.contains("String::from(\"display-name\")"));
    }

    #[test]
    fn every_known_format_uses_authoritative_policy_validation() {
        for format in ["uuid", "ipv4", "ipv6", "ip", "base64", "base64url", "hex",
            "nanoid", "blockchain-address", "blockchain-transaction-hash"] {
            let mut selected = scalar("s", ScalarType::String, 0);
            selected.attrs.format = Some(format.into());
            let model = input(vec![selected]);
            let schema = build_schema(&model).unwrap();
            assert_eq!(schema.fields[0].format.as_deref(), Some(format));
            assert!(render(&model).is_ok());
        }
    }

    #[test]
    fn incompatible_constraint_families_are_never_dropped() {
        for ty in [ScalarType::Bool, ScalarType::U32, ScalarType::I32,
            ScalarType::I64, ScalarType::F64] {
            for constraint in 0..3 {
                let mut selected = scalar("value", ty, 0);
                match constraint {
                    0 => selected.attrs.format = Some("uuid".into()),
                    1 => selected.attrs.min_len = Some(0),
                    _ => selected.attrs.max_len = Some(0),
                }
                let error = render(&input(vec![selected])).unwrap_err();
                assert_eq!(error.field, Some(0));
                assert!(error.message.contains("require String"));
            }
        }
        for ty in [ScalarType::Bool, ScalarType::String, ScalarType::F64] {
            for minimum in [true, false] {
                let mut selected = scalar("value", ty, 0);
                if minimum { selected.attrs.min_i64 = Some(0); }
                else { selected.attrs.max_i64 = Some(0); }
                let error = render(&input(vec![selected])).unwrap_err();
                assert_eq!(error.field, Some(0));
                assert!(error.message.contains("integer-bound attributes"));
            }
        }
        let mut selected = scalar("s", ScalarType::String, 0);
        selected.attrs.min_len = Some(1);
        selected.attrs.max_len = Some(0);
        assert!(render(&input(vec![selected])).unwrap_err().message.contains("inverted"));
    }

    #[test]
    fn malformed_slots_report_original_indices_including_skipped_fields() {
        let vectors = [
            (vec![field("missing", FieldType::Scalar(ScalarType::Bool), None)], 0),
            (vec![scalar("gap", ScalarType::Bool, 1)], 0),
            (vec![scalar("max", ScalarType::Bool, u32::MAX)], 0),
            (vec![scalar("a", ScalarType::Bool, 0), scalar("b", ScalarType::Bool, 0)], 1),
            (vec![field("skip", FieldType::Skipped, Some(0))], 0),
            (vec![field("skip", FieldType::Skipped, None), optional("gap", ScalarType::Bool, 1)], 1),
        ];
        for (fields, index) in vectors {
            let error = render(&input(fields)).unwrap_err();
            assert_eq!(error.field, Some(index));
            assert!(error.message.contains("slot"));
        }
    }

    #[test]
    fn required_and_slot_counters_check_their_exact_width_boundaries() {
        assert_eq!(next_required(0, 0), Ok(1));
        assert_eq!(next_required(u16::MAX - 1, 7), Ok(u16::MAX));
        let error = next_required(u16::MAX, 19).unwrap_err();
        assert_eq!(error.field, Some(19));
        assert!(error.message.contains("u16::MAX"));
        assert_eq!(next_slot(0, 0), Ok(1));
        assert_eq!(next_slot(u32::MAX - 1, 7), Ok(u32::MAX));
        let error = next_slot(u32::MAX, 23).unwrap_err();
        assert_eq!(error.field, Some(23));
        assert!(error.message.contains("u32::MAX"));
    }

    #[test]
    fn literal_helpers_cover_absence_extrema_and_every_frozen_scalar_kind() {
        assert_eq!(option_number(None::<i64>, "i64"), "::core::option::Option::None");
        assert_eq!(option_number(Some(i64::MIN), "i64"),
            "::core::option::Option::Some(-9223372036854775808i64)");
        assert_eq!(option_number(Some(i64::MAX), "i64"),
            "::core::option::Option::Some(9223372036854775807i64)");
        assert_eq!(option_number(Some(u32::MAX), "u32"),
            "::core::option::Option::Some(4294967295u32)");
        assert_eq!(option_string(None), "::core::option::Option::None");
        assert_eq!(option_string(Some("")),
            "::core::option::Option::Some(::alloc::string::String::from(\"\"))");
        assert_eq!(scalar_kind_source(None, "::abi"), "::core::option::Option::None");
        for (kind, name) in [
            (ScalarKind::String, "String"), (ScalarKind::Number, "Number"),
            (ScalarKind::True, "True"), (ScalarKind::False, "False"),
            (ScalarKind::Null, "Null"),
        ] {
            assert_eq!(scalar_kind_source(Some(kind), "crate :: abi"),
                format!("::core::option::Option::Some(crate :: abi::syntax::ScalarKind::{name})"));
        }
        for ty in [ScalarType::Bool, ScalarType::String, ScalarType::F64,
            ScalarType::U64, ScalarType::Decimal128] {
            let error = integer_bounds(ty, &FieldAttrs::default(), 13).unwrap_err();
            assert_eq!(error.field, Some(13));
            assert!(error.message.contains("require u32, i32, or i64"));
        }
    }

    #[test]
    fn required_count_accepts_u16_max_and_rejects_the_next_required_field() {
        let fields = (0..u32::from(u16::MAX))
            .map(|slot| scalar(&format!("f{slot}"), ScalarType::Bool, slot)).collect();
        let mut model = input(fields);
        let schema = build_schema(&model).unwrap();
        assert_eq!(schema.required_count, u16::MAX);
        assert_eq!(schema.fields.last().unwrap().field_id, FieldId(u32::from(u16::MAX) - 1));
        model.fields.push(optional("optional_at_boundary", ScalarType::Bool, u32::from(u16::MAX)));
        let schema = build_schema(&model).unwrap();
        assert_eq!(schema.required_count, u16::MAX);
        assert_eq!(schema.fields.last().unwrap().field_id, FieldId(u32::from(u16::MAX)));
        model.fields.push(scalar("too_many_required", ScalarType::Bool, u32::from(u16::MAX) + 1));
        let error = build_schema(&model).unwrap_err();
        assert_eq!(error.field, Some(usize::from(u16::MAX) + 1));
        assert!(error.message.contains("required_count"));
    }

    #[test]
    fn optional_field_ids_can_cross_u16_without_narrowing() {
        let count = u32::from(u16::MAX) + 2;
        let fields = (0..count)
            .map(|slot| optional(&format!("f{slot}"), ScalarType::Bool, slot)).collect();
        let schema = build_schema(&input(fields)).unwrap();
        assert_eq!(schema.required_count, 0);
        assert_eq!(schema.fields.last().unwrap().field_id, FieldId(count - 1));
        assert_eq!(schema.fields.len(), usize::try_from(count).unwrap());
    }

    #[test]
    fn empty_names_duplicate_names_and_skipped_constraints_are_errors() {
        let mut model = input(Vec::new());
        model.schema_name = Some(String::new());
        assert_eq!(render(&model).unwrap_err().field, None);
        let error = render(&input(vec![scalar("", ScalarType::Bool, 0)])).unwrap_err();
        assert_eq!(error.field, Some(0));
        let error = render(&input(vec![
            scalar("é", ScalarType::Bool, 0), field("skip", FieldType::Skipped, None),
            scalar("é", ScalarType::String, 1),
        ])).unwrap_err();
        assert_eq!(error.field, Some(2));
        assert!(error.message.contains("duplicate"));
        for attrs in [
            FieldAttrs { rename: Some("renamed".into()), ..FieldAttrs::default() },
            FieldAttrs { format: Some("uuid".into()), ..FieldAttrs::default() },
            FieldAttrs { min_len: Some(0), ..FieldAttrs::default() },
            FieldAttrs { max_len: Some(0), ..FieldAttrs::default() },
            FieldAttrs { min_i64: Some(0), ..FieldAttrs::default() },
            FieldAttrs { max_i64: Some(0), ..FieldAttrs::default() },
        ] {
            let mut selected = field("skip", FieldType::Skipped, None);
            selected.attrs = attrs;
            assert_eq!(render(&input(vec![selected])).unwrap_err().field, Some(0));
        }
    }

    #[test]
    fn canonical_failures_preserve_diagnostics_and_map_back_past_skips() {
        for format in ["", "UUID", "unregistered-format"] {
            let mut selected = scalar("bad", ScalarType::String, 1);
            selected.attrs.format = Some(format.into());
            let model = input(vec![
                field("cache", FieldType::Skipped, None),
                scalar("ok", ScalarType::Bool, 0), selected,
            ]);
            let schema = build_schema(&model).unwrap();
            let authoritative = jmax_schema_aot::fingerprint_schema(&schema).unwrap_err();
            let error = render(&model).unwrap_err();
            assert_eq!(error.field, Some(2));
            assert_eq!(error.message, format!("JsonSchema canonical fingerprint failed: {authoritative}"));
            assert!(error.message.contains("UnsupportedSchemaFeature"));
        }
    }

    #[test]
    fn authoritative_name_budgets_fail_instead_of_returning_fallback_hashes() {
        let maximum = usize::try_from(jmax_schema_ir::ResolveLimits::default().max_name_bytes).unwrap();
        let mut model = input(Vec::new());
        model.schema_name = Some("a".repeat(maximum));
        assert!(render(&model).is_ok());
        model.schema_name = Some("a".repeat(maximum + 1));
        let error = render(&model).unwrap_err();
        assert!(error.message.contains("BudgetExhausted"));
        assert!(error.message.contains("schema name byte limit exceeded"));
        let model = input(vec![scalar(&"a".repeat(maximum + 1), ScalarType::Bool, 0)]);
        let error = render(&model).unwrap_err();
        assert_eq!(error.field, Some(0));
        assert!(error.message.contains("property name byte limit exceeded"));
    }

    #[test]
    fn hostile_and_unicode_names_are_escaped_as_owned_rust_strings() {
        for name in ["quoted\"name", "back\\slash", "\0\u{1f}\n\r\t", "東京é🦀",
            "\"#; panic!(\"injected\"); //", "line\u{2028}separator"] {
            let mut model = input(vec![scalar(name, ScalarType::String, 0)]);
            model.schema_name = Some(name.into());
            let source = render(&model).unwrap();
            let expected = format!("name: {},", owned_string(name));
            assert_eq!(source.matches(&expected).count(), 2);
            let schema = build_schema(&model).unwrap();
            assert_eq!(schema.name, name);
            assert_eq!(schema.fields[0].name, name);
        }
        assert_eq!(owned_string("\"\\\0\n"),
            "::alloc::string::String::from(\"\\\"\\\\\\u{0}\\n\")");
    }

    #[test]
    fn fingerprint_is_precomputed_from_the_exact_handwritten_abi_vector() {
        let mut title = optional("title", ScalarType::String, 1);
        title.attrs.min_len = Some(1);
        title.attrs.max_len = Some(36);
        title.attrs.format = Some("uuid".into());
        let mut model = input(vec![scalar("count", ScalarType::U32, 0), title]);
        model.schema_name = Some("Inventory".into());
        model.schema_id = Some(21);
        model.reject_unknown = false;
        let expected = SchemaIr {
            name: "Inventory".into(), schema_id: SchemaId(21), required_count: 1,
            reject_unknown: false,
            fields: vec![
                SchemaFieldIr {
                    field_id: FieldId(0), name: "count".into(), required: true,
                    scalar_kind: Some(ScalarKind::Number), format: None,
                    min_len: None, max_len: None, min_i64: Some(0), max_i64: Some(4_294_967_295),
                },
                SchemaFieldIr {
                    field_id: FieldId(1), name: "title".into(), required: false,
                    scalar_kind: Some(ScalarKind::String), format: Some("uuid".into()),
                    min_len: Some(1), max_len: Some(36), min_i64: None, max_i64: None,
                },
            ],
        };
        let actual = build_schema(&model).unwrap();
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.schema_id, expected.schema_id);
        assert_eq!(actual.required_count, expected.required_count);
        assert_eq!(actual.reject_unknown, expected.reject_unknown);
        assert_eq!(actual.fields.len(), expected.fields.len());
        for (actual, expected) in actual.fields.iter().zip(&expected.fields) {
            assert_field_equal(actual, expected);
        }
        let fingerprint = jmax_schema_aot::fingerprint_schema(&expected).unwrap();
        let normalized = jmax_schema_ir::model::normalize(&expected).unwrap();
        let resolved = jmax_schema_ir::resolve::resolve(normalized,
            jmax_schema_ir::ResolveLimits::default()).unwrap();
        assert_eq!(fingerprint, jmax_schema_ir::canonical::fingerprint(&resolved,
            &jmax_schema_ir::SchemaPolicy::default()).unwrap());
        let source = render(&model).unwrap();
        assert!(source.ends_with(&format!("fn schema_fingerprint() -> u64 {{ {fingerprint}u64 }}\n}}\n")));
        let method = source.split("fn schema_fingerprint()").nth(1).unwrap();
        assert!(!method.contains("alloc"));
        assert!(!method.contains("schema_ir"));
        assert!(!method.contains("fingerprint_schema"));
        assert!(!source.contains("::jmax_schema_aot"));
        assert!(!source.contains("unwrap"));
        assert!(!source.contains("panic!"));
        assert!(!source.contains("issue_unchecked"));
    }

    #[test]
    fn identity_tracks_schema_semantics_and_ignores_rust_token_spelling() {
        let base = input(vec![scalar("value", ScalarType::I64, 0)]);
        let identity = canonical_fingerprint(&base, &build_schema(&base).unwrap()).unwrap();
        let mut spelling = base.clone();
        spelling.ident = "DifferentRustName".into();
        spelling.schema_name = Some("Record".into());
        spelling.impl_generics = "<T>".into();
        spelling.type_generics = "<T>".into();
        spelling.where_clause = "where T: Sized".into();
        spelling.abi = "crate :: abi".into();
        assert_eq!(canonical_fingerprint(&spelling, &build_schema(&spelling).unwrap()).unwrap(), identity);
        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.schema_name = Some("Renamed".into());
        variants.push(changed);
        let mut changed = base.clone();
        changed.schema_id = Some(1);
        variants.push(changed);
        let mut changed = base.clone();
        changed.reject_unknown = false;
        variants.push(changed);
        let mut changed = base.clone();
        changed.fields[0].wire_name = "renamed".into();
        variants.push(changed);
        let mut changed = base.clone();
        changed.fields[0].attrs.default = true;
        variants.push(changed);
        let mut changed = base.clone();
        changed.fields[0].attrs.min_i64 = Some(0);
        variants.push(changed);
        for changed in variants {
            let actual = canonical_fingerprint(&changed, &build_schema(&changed).unwrap()).unwrap();
            assert_ne!(actual, identity);
        }
    }
}
