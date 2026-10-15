//! Normalization, whole-graph validation, and read-only constraint access.

use super::*;
use core::cmp::Ordering;

fn error(schema: SchemaId, field: Option<FieldId>, code: ErrorCode, detail: &'static str) -> Error {
    schema_error(code, detail).with_schema(schema.0, field.map_or(0, |id| id.0))
}

fn invalid(schema: SchemaId, field: Option<FieldId>, detail: &'static str) -> Error {
    error(schema, field, ErrorCode::ConstraintViolation, detail)
}

fn count(value: usize, schema: SchemaId, field: Option<FieldId>) -> Result<u32> {
    u32::try_from(value).map_err(|_| {
        error(schema, field, ErrorCode::BudgetExhausted, "schema count exceeds u32")
    })
}

fn reserve<T>(values: &mut Vec<T>, additional: usize, schema: SchemaId) -> Result<()> {
    values.try_reserve_exact(additional).map_err(|_| {
        error(schema, None, ErrorCode::BudgetExhausted, "schema allocation failed")
    })
}

fn copy_string(value: &str, schema: SchemaId, field: Option<FieldId>) -> Result<String> {
    count(value.len(), schema, field)?;
    let mut result = String::new();
    result.try_reserve_exact(value.len()).map_err(|_| {
        error(schema, field, ErrorCode::BudgetExhausted, "schema string allocation failed")
    })?;
    result.push_str(value);
    Ok(result)
}

/// Adapt the flat ABI schema to a root object with one separate node per field.
///
/// Field order determines slots, including slots for optional fields. Boolean
/// tags describe the Boolean category rather than literal-only constraints.
/// The input is borrowed and is never repaired or otherwise modified.
pub fn normalize(ir: &SchemaIr) -> Result<Schema> {
    let id = ir.schema_id;
    count(ir.fields.len(), id, None)?;
    let node_count = ir.fields.len().checked_add(1).ok_or_else(|| {
        error(id, None, ErrorCode::BudgetExhausted, "schema node count overflow")
    })?;
    count(node_count, id, None)?;
    let required = ir.fields.iter().filter(|field| field.required).count();
    let required = u16::try_from(required).map_err(|_| {
        error(id, None, ErrorCode::BudgetExhausted, "required count exceeds ABI u16")
    })?;
    if required != ir.required_count {
        return Err(invalid(id, None, "required_count does not match required fields"));
    }

    let name = copy_string(&ir.name, id, None)?;
    let mut nodes = Vec::new();
    let mut properties = Vec::new();
    reserve(&mut nodes, node_count, id)?;
    reserve(&mut properties, ir.fields.len(), id)?;
    for (index, field) in ir.fields.iter().enumerate() {
        let slot = count(index, id, Some(field.field_id))?;
        // node_count was checked above, so the child index is representable.
        let ty = TypeId(slot + 1);
        let node = normalize_field(field, id)?;
        properties.push(Property {
            field_id: field.field_id,
            name: copy_string(&field.name, id, Some(field.field_id))?,
            ty,
            required: field.required,
            slot: Some(slot),
        });
        nodes.push(node);
    }
    // Capacity includes the root; inserting it cannot allocate. Child IDs
    // already account for this shift and are never exposed before validation.
    nodes.insert(0, TypeNode::Object(ObjectShape {
        properties,
        reject_unknown: ir.reject_unknown,
        min_properties: None,
        max_properties: None,
    }));
    let schema = Schema {
        name, schema_id: id, root: TypeId(0), nodes, definitions: Vec::new(),
    };
    validate(&schema)?;
    Ok(schema)
}

fn normalize_field(field: &SchemaFieldIr, schema: SchemaId) -> Result<TypeNode> {
    let kind = match field.scalar_kind {
        None => {
            if field.format.is_some() || field.min_len.is_some() || field.max_len.is_some()
                || field.min_i64.is_some() || field.max_i64.is_some()
            {
                return Err(invalid(schema, Some(field.field_id), "Any field has scalar constraints"));
            }
            return Ok(TypeNode::Any);
        }
        Some(ScalarKind::String) => ValueKind::String,
        Some(ScalarKind::Number) if field.min_i64.is_some() || field.max_i64.is_some() => {
            ValueKind::Integer
        }
        Some(ScalarKind::Number) => ValueKind::Number,
        Some(ScalarKind::True) | Some(ScalarKind::False) => ValueKind::Boolean,
        Some(ScalarKind::Null) => ValueKind::Null,
    };
    let constraints = ScalarConstraints {
        kind,
        min_i64: field.min_i64,
        max_i64: field.max_i64,
        decimal: None,
        min_length: field.min_len,
        max_length: field.max_len,
        pattern: None,
        format: field.format.as_deref()
            .map(|value| copy_string(value, schema, Some(field.field_id))).transpose()?,
    };
    validate_scalar(&constraints, schema, Some(field.field_id))?;
    Ok(TypeNode::Scalar(constraints))
}

/// Validate every stored node, even if it cannot be reached from the root.
///
/// References use exact local names. Reference existence and alias cycles are
/// intentionally left to the resolver. Duplicate detection uses fallibly
/// allocated tables and allocation-free unstable sorts, without recursion over
/// the graph. Structural cycles and shared property targets are permitted.
pub fn validate(schema: &Schema) -> Result<()> {
    let id = schema.schema_id;
    count(schema.name.len(), id, None)?;
    count(schema.nodes.len(), id, None)?;
    count(schema.definitions.len(), id, None)?;
    check_id(schema, schema.root, None)?;

    let mut definitions = Vec::new();
    reserve(&mut definitions, schema.definitions.len(), id)?;
    for definition in &schema.definitions {
        if definition.name.is_empty() {
            return Err(invalid(id, None, "empty definition name"));
        }
        count(definition.name.len(), id, None)?;
        check_id(schema, definition.ty, None)?;
        definitions.push(definition);
    }
    definitions.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    for pair in definitions.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(invalid(id, None, "duplicate definition name"));
        }
    }
    drop(definitions);

    let mut property_count = 0usize;
    for node in &schema.nodes {
        if let TypeNode::Object(object) = node {
            count(object.properties.len(), id, None)?;
            property_count = property_count.checked_add(object.properties.len()).ok_or_else(|| {
                error(id, None, ErrorCode::BudgetExhausted, "property count overflow")
            })?;
            count(property_count, id, None)?;
        }
    }
    let mut properties = Vec::new();
    reserve(&mut properties, property_count, id)?;
    // Keep the first direct incoming field for diagnostics on shared targets.
    let mut incoming = Vec::new();
    reserve(&mut incoming, schema.nodes.len(), id)?;
    incoming.resize(schema.nodes.len(), None);
    for (owner, node) in schema.nodes.iter().enumerate() {
        if let TypeNode::Object(object) = node {
            for property in &object.properties {
                let field = Some(property.field_id);
                count(property.name.len(), id, field)?;
                check_id(schema, property.ty, field)?;
                let context = &mut incoming[property.ty.0 as usize];
                if context.is_none() {
                    *context = field;
                }
                properties.push((owner, property));
            }
        }
    }
    properties.sort_unstable_by(|left, right| {
        left.0.cmp(&right.0)
            .then_with(|| left.1.name.as_bytes().cmp(right.1.name.as_bytes()))
            .then_with(|| left.1.field_id.0.cmp(&right.1.field_id.0))
    });
    for pair in properties.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1.name == pair[1].1.name {
            return Err(invalid(id, Some(pair[1].1.field_id), "duplicate object property name"));
        }
    }
    properties.sort_unstable_by_key(|entry| entry.1.field_id.0);
    for pair in properties.windows(2) {
        if pair[0].1.field_id == pair[1].1.field_id {
            return Err(invalid(id, Some(pair[1].1.field_id), "duplicate global field ID"));
        }
    }
    properties.sort_unstable_by_key(|entry| (entry.1.slot, entry.1.field_id.0));
    for pair in properties.windows(2) {
        if pair[0].1.slot.is_some() && pair[0].1.slot == pair[1].1.slot {
            return Err(invalid(id, Some(pair[1].1.field_id), "duplicate global slot"));
        }
    }
    drop(properties);

    for (index, node) in schema.nodes.iter().enumerate() {
        let field = incoming[index];
        match node {
            TypeNode::Any => {}
            TypeNode::Scalar(constraints) => validate_scalar(constraints, id, field)?,
            TypeNode::Object(object) => {
                check_bounds(object.min_properties, object.max_properties, id, field)?;
            }
            TypeNode::Array(array) => {
                check_id(schema, array.items, field)?;
                check_bounds(array.min_items, array.max_items, id, field)?;
            }
            TypeNode::Ref(name) => {
                count(name.len(), id, field)?;
                if name.is_empty() {
                    return Err(invalid(id, field, "empty reference name"));
                }
            }
        }
    }
    Ok(())
}

fn check_id(schema: &Schema, ty: TypeId, field: Option<FieldId>) -> Result<()> {
    if schema.nodes.get(ty.0 as usize).is_none() {
        return Err(invalid(schema.schema_id, field, "type ID is outside the schema"));
    }
    Ok(())
}

fn check_bounds<T: Ord>(minimum: Option<T>, maximum: Option<T>, schema: SchemaId,
    field: Option<FieldId>) -> Result<()>
{
    if let (Some(minimum), Some(maximum)) = (minimum, maximum) {
        if minimum > maximum {
            return Err(invalid(schema, field, "minimum exceeds maximum"));
        }
    }
    Ok(())
}

fn validate_scalar(value: &ScalarConstraints, schema: SchemaId, field: Option<FieldId>) -> Result<()> {
    if value.kind != ValueKind::Integer && (value.min_i64.is_some() || value.max_i64.is_some()) {
        return Err(invalid(schema, field, "integer bounds require Integer"));
    }
    if (value.kind == ValueKind::Decimal) != value.decimal.is_some() {
        return Err(invalid(schema, field, "decimal constraints are required exactly for Decimal"));
    }
    if value.kind != ValueKind::String && (value.min_length.is_some() || value.max_length.is_some()
        || value.pattern.is_some() || value.format.is_some())
    {
        return Err(invalid(schema, field, "string constraints require String"));
    }
    check_bounds(value.min_i64, value.max_i64, schema, field)?;
    check_bounds(value.min_length, value.max_length, schema, field)?;
    if let Some(pattern) = &value.pattern {
        count(pattern.len(), schema, field)?;
    }
    if let Some(format) = &value.format {
        count(format.len(), schema, field)?;
    }
    if let Some(decimal) = &value.decimal {
        let policy = &decimal.policy;
        if !(1..=38).contains(&policy.max_precision) || policy.target_scale > 38 || policy.tick_step < 0 {
            return Err(invalid(schema, field, "invalid decimal policy"));
        }
        for bound in [decimal.minimum, decimal.maximum].into_iter().flatten() {
            if bound.scale > 38 {
                return Err(invalid(schema, field, "decimal bound scale exceeds 38"));
            }
        }
        if let (Some(minimum), Some(maximum)) = (decimal.minimum, decimal.maximum) {
            if compare_decimal(minimum, maximum) == Ordering::Greater {
                return Err(invalid(schema, field, "decimal minimum exceeds maximum"));
            }
        }
    }
    Ok(())
}

/// Compare validated scales exactly without multiplying either coefficient.
/// The divisor is at most 10^38, which fits in i128. Signed division rounds
/// toward zero; the remainder preserves every discarded digit and its sign.
/// No absolute value or negation of i128::MIN is needed.
fn compare_decimal(left: Decimal128, right: Decimal128) -> Ordering {
    match left.scale.cmp(&right.scale) {
        Ordering::Equal => left.coefficient.cmp(&right.coefficient),
        Ordering::Less => {
            let divisor = 10_i128.pow(u32::from(right.scale - left.scale));
            left.coefficient.cmp(&(right.coefficient / divisor))
                .then_with(|| 0_i128.cmp(&(right.coefficient % divisor)))
        }
        Ordering::Greater => compare_decimal(right, left).reverse(),
    }
}

fn incoming_field(schema: &Schema, ty: TypeId) -> Option<FieldId> {
    schema.nodes.iter().find_map(|node| match node {
        TypeNode::Object(object) => object.properties.iter()
            .find(|property| property.ty == ty).map(|property| property.field_id),
        _ => None,
    })
}

fn resolved_node(schema: &ResolvedSchema, ty: TypeId) -> Result<&TypeNode> {
    // The resolver supplies a checked witness. Also reject an invalid caller ID
    // explicitly rather than allowing indexing or an unchecked target access.
    let node = schema.schema.nodes.get(ty.0 as usize).and_then(|_| schema.node(ty));
    match node {
        Some(TypeNode::Ref(_)) | None => Err(invalid(
            schema.schema.schema_id, incoming_field(&schema.schema, ty),
            "invalid resolved type ID or alias target",
        )),
        Some(node) => Ok(node),
    }
}

/// Return scalar constraints after following the resolver's collapsed alias.
/// Containers and Any have no scalar constraints and return `None`.
pub fn constraints(schema: &ResolvedSchema, ty: TypeId) -> Result<Option<&ScalarConstraints>> {
    match resolved_node(schema, ty)? {
        TypeNode::Scalar(constraints) => Ok(Some(constraints)),
        _ => Ok(None),
    }
}

fn resolved_object(schema: &ResolvedSchema, ty: TypeId) -> Result<&ObjectShape> {
    match resolved_node(schema, ty)? {
        TypeNode::Object(object) => Ok(object),
        _ => {
            let field = incoming_field(&schema.schema, ty).or_else(|| {
                schema.target(ty).and_then(|target| incoming_field(&schema.schema, target))
            });
            Err(error(schema.schema.schema_id, field, ErrorCode::TypeMismatch, "expected an object type"))
        }
    }
}

/// Look up an exact UTF-8 property name, including the valid empty name.
pub fn property<'a>(schema: &'a ResolvedSchema, object: TypeId, name: &str) -> Result<Option<&'a Property>> {
    Ok(resolved_object(schema, object)?.properties.iter().find(|property| property.name == name))
}

/// Return required FieldIds in stored declaration order, without sorting them.
pub fn required_fields(schema: &ResolvedSchema, object: TypeId) -> Result<Vec<FieldId>> {
    let shape = resolved_object(schema, object)?;
    let required = shape.properties.iter().filter(|property| property.required).count();
    let contextualize = |error: Error| {
        let field = incoming_field(&schema.schema, object).or_else(|| {
            schema.target(object).and_then(|target| incoming_field(&schema.schema, target))
        });
        error.with_schema(schema.schema.schema_id.0, field.map_or(0, |id| id.0))
    };
    count(required, schema.schema.schema_id, None).map_err(contextualize)?;
    let mut fields = Vec::new();
    reserve(&mut fields, required, schema.schema.schema_id).map_err(contextualize)?;
    fields.extend(shape.properties.iter().filter(|property| property.required).map(|property| property.field_id));
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const ID: SchemaId = SchemaId(73);

    fn graph(nodes: Vec<TypeNode>) -> Schema {
        Schema {
            name: String::from("model tests"), schema_id: ID, root: TypeId(0),
            nodes, definitions: Vec::new(),
        }
    }

    fn field(id: u32, name: &str, kind: Option<ScalarKind>, required: bool) -> SchemaFieldIr {
        SchemaFieldIr {
            field_id: FieldId(id), name: String::from(name), required,
            scalar_kind: kind, format: None, min_len: None, max_len: None,
            min_i64: None, max_i64: None,
        }
    }

    fn ir(fields: Vec<SchemaFieldIr>, required_count: u16) -> SchemaIr {
        SchemaIr {
            name: String::from("trade"), schema_id: ID, required_count,
            reject_unknown: true, fields,
        }
    }

    fn prop(id: u32, name: &str, ty: u32, required: bool, slot: Option<u32>) -> Property {
        Property { field_id: FieldId(id), name: String::from(name), ty: TypeId(ty), required, slot }
    }

    fn object(properties: Vec<Property>) -> TypeNode {
        TypeNode::Object(ObjectShape {
            properties, reject_unknown: true, min_properties: None, max_properties: None,
        })
    }

    fn decimal(minimum: Option<Decimal128>, maximum: Option<Decimal128>) -> ScalarConstraints {
        let mut result = ScalarConstraints::new(ValueKind::Decimal);
        result.decimal = Some(DecimalConstraints { policy: DecimalPolicy::spot_8(), minimum, maximum });
        result
    }

    fn assert_error(error: Error, code: ErrorCode, field: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, ID.0);
        assert_eq!(error.field_id, field);
        assert!(!error.detail.is_empty());
    }

    fn assert_invalid(schema: &Schema, field: u32) {
        let original = schema.clone();
        assert_error(validate(schema).unwrap_err(), ErrorCode::ConstraintViolation, field);
        assert_eq!(schema, &original);
    }

    fn resolve_fixture(schema: Schema) -> ResolvedSchema {
        // Use the frozen resolver API so only the resolver constructs witnesses.
        crate::resolve::resolve(schema, ResolveLimits::default()).unwrap()
    }

    #[test]
    fn empty_ir_has_one_root_object_and_preserves_metadata() {
        let input = ir(Vec::new(), 0);
        let output = normalize(&input).unwrap();
        assert_eq!(output.name, "trade");
        assert_eq!(output.schema_id, ID);
        assert_eq!(output.root, TypeId(0));
        assert!(output.definitions.is_empty());
        assert_eq!(output.nodes, vec![object(Vec::new())]);
        let mut permissive = input.clone();
        permissive.reject_unknown = false;
        let output = normalize(&permissive).unwrap();
        match &output.nodes[0] {
            TypeNode::Object(shape) => assert!(!shape.reject_unknown),
            _ => panic!("normalization root must be an object"),
        }
    }

    #[test]
    fn normalization_maps_all_abi_categories_and_integer_bound_directions() {
        let mut fields = vec![
            field(9, "any", None, false),
            field(8, "string", Some(ScalarKind::String), true),
            field(7, "number", Some(ScalarKind::Number), false),
            field(6, "lower", Some(ScalarKind::Number), false),
            field(5, "upper", Some(ScalarKind::Number), true),
            field(4, "true", Some(ScalarKind::True), false),
            field(3, "false", Some(ScalarKind::False), false),
            field(2, "null", Some(ScalarKind::Null), false),
        ];
        fields[3].min_i64 = Some(i64::MIN);
        fields[4].max_i64 = Some(i64::MAX);
        fields[1].min_len = Some(0);
        fields[1].max_len = Some(u32::MAX);
        fields[1].format = Some(String::from("uuid"));
        let input = ir(fields, 2);
        let output = normalize(&input).unwrap();
        assert_eq!(output.nodes.len(), 9);
        assert_eq!(output.nodes[1], TypeNode::Any);
        let kinds = [ValueKind::String, ValueKind::Number, ValueKind::Integer,
            ValueKind::Integer, ValueKind::Boolean, ValueKind::Boolean, ValueKind::Null];
        for (index, kind) in kinds.into_iter().enumerate() {
            match &output.nodes[index + 2] {
                TypeNode::Scalar(value) => assert_eq!(value.kind, kind),
                _ => panic!("expected scalar child"),
            }
        }
        match &output.nodes[0] {
            TypeNode::Object(shape) => {
                assert_eq!(shape.properties.len(), input.fields.len());
                for (index, property) in shape.properties.iter().enumerate() {
                    let source = &input.fields[index];
                    assert_eq!(property.field_id, source.field_id);
                    assert_eq!(property.name, source.name);
                    assert_eq!(property.required, source.required);
                    assert_eq!(property.slot, Some(index as u32));
                    assert_eq!(property.ty, TypeId(index as u32 + 1));
                }
            }
            _ => panic!("expected object root"),
        }
        match &output.nodes[2] {
            TypeNode::Scalar(value) => {
                assert_eq!(value.min_length, Some(0));
                assert_eq!(value.max_length, Some(u32::MAX));
                assert_eq!(value.format.as_deref(), Some("uuid"));
                assert_eq!(value.pattern, None);
            }
            _ => panic!("expected string"),
        }
        let resolved = resolve_fixture(output);
        assert_eq!(required_fields(&resolved, TypeId(0)).unwrap(), vec![FieldId(8), FieldId(5)]);
        assert_eq!(constraints(&resolved, TypeId(4)).unwrap().unwrap().min_i64, Some(i64::MIN));
        assert_eq!(constraints(&resolved, TypeId(5)).unwrap().unwrap().max_i64, Some(i64::MAX));
        assert_eq!(input.name, "trade");
        assert_eq!(input.required_count, 2);
        assert_eq!(input.fields[1].format.as_deref(), Some("uuid"));
    }

    #[test]
    fn required_count_mismatch_is_not_repaired() {
        for (required, supplied) in [(false, 1), (true, 0), (true, 2)] {
            let input = ir(vec![field(12, "x", None, required)], supplied);
            assert_error(normalize(&input).unwrap_err(), ErrorCode::ConstraintViolation, 0);
            assert_eq!(input.required_count, supplied);
            assert_eq!(input.fields[0].required, required);
        }
    }

    #[test]
    fn required_count_must_fit_the_abi() {
        let input = ir(vec![field(1, "", None, true); usize::from(u16::MAX) + 1], u16::MAX);
        assert_error(normalize(&input).unwrap_err(), ErrorCode::BudgetExhausted, 0);
    }

    #[test]
    fn any_rejects_each_scalar_constraint_separately() {
        for case in 0..5 {
            let mut value = field(42, "", None, false);
            match case {
                0 => value.format = Some(String::new()),
                1 => value.min_len = Some(0),
                2 => value.max_len = Some(u32::MAX),
                3 => value.min_i64 = Some(i64::MIN),
                _ => value.max_i64 = Some(i64::MAX),
            }
            assert_error(normalize(&ir(vec![value], 0)).unwrap_err(), ErrorCode::ConstraintViolation, 42);
        }
    }

    #[test]
    fn normalization_rejects_incompatible_and_reversed_constraints() {
        let mut string_integer = field(42, "x", Some(ScalarKind::String), false);
        string_integer.min_i64 = Some(0);
        let mut number_format = field(42, "x", Some(ScalarKind::Number), false);
        number_format.format = Some(String::from("hex"));
        let mut boolean_length = field(42, "x", Some(ScalarKind::False), false);
        boolean_length.max_len = Some(0);
        let mut reversed_integer = field(42, "x", Some(ScalarKind::Number), false);
        reversed_integer.min_i64 = Some(1);
        reversed_integer.max_i64 = Some(0);
        let mut reversed_length = field(42, "x", Some(ScalarKind::String), false);
        reversed_length.min_len = Some(1);
        reversed_length.max_len = Some(0);
        for value in [string_integer, number_format, boolean_length, reversed_integer, reversed_length] {
            assert_error(normalize(&ir(vec![value], 0)).unwrap_err(), ErrorCode::ConstraintViolation, 42);
        }
    }

    #[test]
    fn normalization_rejects_duplicate_names_and_ids() {
        let duplicate_names = ir(vec![field(1, "", None, false), field(2, "", None, false)], 0);
        assert_error(normalize(&duplicate_names).unwrap_err(), ErrorCode::ConstraintViolation, 2);
        let duplicate_ids = ir(vec![field(8, "a", None, false), field(8, "b", None, false)], 0);
        assert_error(normalize(&duplicate_ids).unwrap_err(), ErrorCode::ConstraintViolation, 8);
    }

    #[test]
    fn property_names_are_exact_bytes_and_may_be_empty() {
        let names = ["", "A", "a", "é", "e\u{301}", "a\0b", "数字"];
        let fields = names.iter().enumerate()
            .map(|(index, name)| field(index as u32, name, None, false)).collect();
        let output = resolve_fixture(normalize(&ir(fields, 0)).unwrap());
        for (index, name) in names.iter().enumerate() {
            assert_eq!(property(&output, TypeId(0), name).unwrap().unwrap().field_id, FieldId(index as u32));
        }
        assert!(property(&output, TypeId(0), "missing").unwrap().is_none());
        assert!(required_fields(&output, TypeId(0)).unwrap().is_empty());
    }

    #[test]
    fn root_and_all_edge_kinds_require_valid_ids() {
        assert_invalid(&graph(Vec::new()), 0);
        let mut invalid_root = graph(vec![TypeNode::Any]);
        for ty in [1, u32::MAX] {
            invalid_root.root = TypeId(ty);
            assert_invalid(&invalid_root, 0);
            let object_schema = graph(vec![object(vec![prop(42, "x", ty, false, None)])]);
            assert_invalid(&object_schema, 42);
            let array_schema = graph(vec![TypeNode::Any, TypeNode::Array(ArrayShape {
                items: TypeId(if ty == 1 { 2 } else { ty }), min_items: None, max_items: None,
            })]);
            assert_invalid(&array_schema, 0);
            let mut definition_schema = graph(vec![TypeNode::Any]);
            definition_schema.definitions.push(NamedType { name: String::from("local"), ty: TypeId(ty) });
            assert_invalid(&definition_schema, 0);
        }
    }

    #[test]
    fn definitions_must_be_nonempty_unique_exact_local_names() {
        let mut schema = graph(vec![TypeNode::Any]);
        schema.definitions.push(NamedType { name: String::new(), ty: TypeId(0) });
        assert_invalid(&schema, 0);
        schema.definitions[0].name = String::from("Thing");
        schema.definitions.push(schema.definitions[0].clone());
        assert_invalid(&schema, 0);
        schema.definitions[1].name = String::from("thing");
        for name in ["#/$defs/x", "https://example.invalid/type", "é", "e\u{301}"] {
            schema.definitions.push(NamedType { name: String::from(name), ty: TypeId(0) });
        }
        validate(&schema).unwrap();
    }

    #[test]
    fn reference_resolution_is_not_validation() {
        let mut schema = graph(vec![TypeNode::Any, TypeNode::Ref(String::new())]);
        assert_invalid(&schema, 0);
        schema.nodes[1] = TypeNode::Ref(String::from("unknown"));
        validate(&schema).unwrap();
        schema.definitions.push(NamedType { name: String::from("unknown"), ty: TypeId(1) });
        // This alias-only cycle is rejected by resolve, not by validate.
        validate(&schema).unwrap();
    }

    #[test]
    fn whole_graph_includes_unreachable_constraint_errors() {
        let mut value = ScalarConstraints::new(ValueKind::Integer);
        value.min_i64 = Some(2);
        value.max_i64 = Some(1);
        assert_invalid(&graph(vec![TypeNode::Any, TypeNode::Scalar(value)]), 0);
        assert_invalid(&graph(vec![TypeNode::Any, object(vec![
            prop(42, "x", 0, false, None), prop(43, "x", 0, false, None),
        ])]), 43);
    }

    #[test]
    fn field_ids_and_slots_are_global_but_names_are_object_local() {
        let mut schema = graph(vec![
            object(vec![prop(1, "shared", 2, false, Some(0))]),
            object(vec![prop(2, "shared", 2, false, Some(u32::MAX))]),
            TypeNode::Any,
        ]);
        validate(&schema).unwrap();
        if let TypeNode::Object(shape) = &mut schema.nodes[1] {
            shape.properties[0].field_id = FieldId(1);
        }
        assert_invalid(&schema, 1);
        if let TypeNode::Object(shape) = &mut schema.nodes[1] {
            shape.properties[0].field_id = FieldId(2);
            shape.properties[0].slot = Some(0);
        }
        assert_invalid(&schema, 2);
        for node in &mut schema.nodes {
            if let TypeNode::Object(shape) = node {
                shape.properties[0].slot = None;
            }
        }
        validate(&schema).unwrap();
    }

    #[test]
    fn self_edges_shared_targets_and_structural_cycles_are_valid() {
        let schema = graph(vec![
            object(vec![prop(0, "self", 0, true, None), prop(u32::MAX, "list", 1, false, Some(u32::MAX))]),
            TypeNode::Array(ArrayShape { items: TypeId(0), min_items: Some(0), max_items: Some(u32::MAX) }),
        ]);
        let before = schema.clone();
        validate(&schema).unwrap();
        assert_eq!(schema, before);
        validate(&graph(vec![TypeNode::Array(ArrayShape {
            items: TypeId(0), min_items: None, max_items: None,
        })])).unwrap();
    }

    #[test]
    fn scalar_errors_carry_the_first_incoming_field() {
        let mut invalid = ScalarConstraints::new(ValueKind::String);
        invalid.min_i64 = Some(0);
        let schema = graph(vec![
            object(vec![prop(42, "first", 1, false, None), prop(43, "second", 1, false, None)]),
            TypeNode::Scalar(invalid),
        ]);
        assert_invalid(&schema, 42);
    }

    #[test]
    fn constraint_family_matrix_rejects_every_wrong_kind() {
        let kinds = [ValueKind::Null, ValueKind::Boolean, ValueKind::Integer,
            ValueKind::Number, ValueKind::Decimal, ValueKind::String];
        for kind in kinds {
            let base = if kind == ValueKind::Decimal { decimal(None, None) }
                else { ScalarConstraints::new(kind) };
            validate_scalar(&base, ID, Some(FieldId(42))).unwrap();
            for constraint in 0..6 {
                let mut value = base.clone();
                match constraint {
                    0 => value.min_i64 = Some(i64::MIN),
                    1 => value.max_i64 = Some(i64::MAX),
                    2 => value.min_length = Some(0),
                    3 => value.max_length = Some(u32::MAX),
                    4 => value.pattern = Some(String::new()),
                    _ => value.format = Some(String::from("not-a-known-format")),
                }
                let allowed = if constraint < 2 { kind == ValueKind::Integer } else { kind == ValueKind::String };
                if allowed {
                    validate_scalar(&value, ID, Some(FieldId(42))).unwrap();
                } else {
                    assert_error(validate_scalar(&value, ID, Some(FieldId(42))).unwrap_err(),
                        ErrorCode::ConstraintViolation, 42);
                }
            }
            let mut value = base;
            if kind == ValueKind::Decimal {
                value.decimal = None;
            } else {
                value.decimal = decimal(None, None).decimal;
            }
            assert_error(validate_scalar(&value, ID, Some(FieldId(42))).unwrap_err(),
                ErrorCode::ConstraintViolation, 42);
        }
    }

    #[test]
    fn integer_and_length_bounds_are_inclusive_and_support_extremes() {
        for (minimum, maximum) in [(i64::MIN, i64::MAX), (i64::MIN, i64::MIN),
            (i64::MAX, i64::MAX), (0, 0)]
        {
            let mut value = ScalarConstraints::new(ValueKind::Integer);
            value.min_i64 = Some(minimum);
            value.max_i64 = Some(maximum);
            validate(&graph(vec![TypeNode::Scalar(value)])).unwrap();
        }
        for (minimum, maximum) in [(0, 0), (0, u32::MAX), (u32::MAX, u32::MAX)] {
            let mut value = ScalarConstraints::new(ValueKind::String);
            value.min_length = Some(minimum);
            value.max_length = Some(maximum);
            validate(&graph(vec![TypeNode::Scalar(value)])).unwrap();
        }
        let mut value = ScalarConstraints::new(ValueKind::Integer);
        value.min_i64 = Some(i64::MAX);
        value.max_i64 = Some(i64::MIN);
        assert_invalid(&graph(vec![TypeNode::Scalar(value)]), 0);
        let mut value = ScalarConstraints::new(ValueKind::String);
        value.min_length = Some(u32::MAX);
        value.max_length = Some(0);
        assert_invalid(&graph(vec![TypeNode::Scalar(value)]), 0);
    }

    #[test]
    fn object_and_array_bounds_handle_absence_equality_and_reversal() {
        let cases = [(None, None, true), (Some(u32::MAX), None, true),
            (None, Some(0), true), (Some(0), Some(0), true),
            (Some(u32::MAX), Some(u32::MAX), true), (Some(1), Some(0), false)];
        for (minimum, maximum, valid) in cases {
            let array = TypeNode::Array(ArrayShape { items: TypeId(0), min_items: minimum, max_items: maximum });
            let object = TypeNode::Object(ObjectShape {
                properties: Vec::new(), reject_unknown: false,
                min_properties: minimum, max_properties: maximum,
            });
            for node in [array, object] {
                let schema = graph(vec![node]);
                if valid { validate(&schema).unwrap(); } else { assert_invalid(&schema, 0); }
            }
        }
    }

    #[test]
    fn container_and_reference_errors_include_incoming_field_context() {
        for node in [
            TypeNode::Array(ArrayShape { items: TypeId(9), min_items: None, max_items: None }),
            TypeNode::Array(ArrayShape { items: TypeId(0), min_items: Some(1), max_items: Some(0) }),
            TypeNode::Object(ObjectShape {
                properties: Vec::new(), reject_unknown: false,
                min_properties: Some(1), max_properties: Some(0),
            }),
            TypeNode::Ref(String::new()),
        ] {
            assert_invalid(&graph(vec![object(vec![prop(42, "child", 1, false, None)]), node]), 42);
        }
    }

    #[test]
    fn decimal_policy_boundaries_and_invalid_values() {
        for precision in [1, 38] {
            for scale in [0, 38] {
                for tick in [0, 1, i128::MAX] {
                    let mut value = decimal(None, None);
                    let policy = &mut value.decimal.as_mut().unwrap().policy;
                    policy.max_precision = precision;
                    policy.target_scale = scale;
                    policy.tick_step = tick;
                    validate(&graph(vec![TypeNode::Scalar(value)])).unwrap();
                }
            }
        }
        for case in 0..7 {
            let mut value = decimal(None, None);
            let policy = &mut value.decimal.as_mut().unwrap().policy;
            match case {
                0 => policy.max_precision = 0,
                1 => policy.max_precision = 39,
                2 => policy.max_precision = u8::MAX,
                3 => policy.target_scale = 39,
                4 => policy.target_scale = u8::MAX,
                5 => policy.tick_step = -1,
                _ => policy.tick_step = i128::MIN,
            }
            assert_invalid(&graph(vec![TypeNode::Scalar(value)]), 0);
        }
    }

    #[test]
    fn decimal_scales_are_checked_even_for_one_sided_zero_bounds() {
        for scale in [39, u8::MAX] {
            for (minimum, maximum) in [(Some(Decimal128::new(0, scale)), None),
                (None, Some(Decimal128::new(0, scale)))]
            {
                assert_invalid(&graph(vec![TypeNode::Scalar(decimal(minimum, maximum))]), 0);
            }
        }
        for coefficient in [i128::MIN, 0, i128::MAX] {
            for scale in [0, 38] {
                let bound = Some(Decimal128::new(coefficient, scale));
                for (minimum, maximum) in [(bound, None), (None, bound), (bound, bound)] {
                    validate(&graph(vec![TypeNode::Scalar(decimal(minimum, maximum))])).unwrap();
                }
            }
        }
    }

    #[test]
    fn exact_decimal_comparison_vectors_cover_signs_remainders_and_extremes() {
        let vectors = [
            ((0, 0), (0, 38), Ordering::Equal),
            ((1, 0), (10, 1), Ordering::Equal),
            ((-1, 0), (-100, 2), Ordering::Equal),
            ((1, 0), (11, 1), Ordering::Less),
            ((1, 0), (9, 1), Ordering::Greater),
            ((-1, 0), (-11, 1), Ordering::Greater),
            ((-1, 0), (-9, 1), Ordering::Less),
            ((0, 0), (1, 38), Ordering::Less),
            ((0, 0), (-1, 38), Ordering::Greater),
            ((-1, 38), (1, 38), Ordering::Less),
            ((i128::MIN, 0), (i128::MAX, 38), Ordering::Less),
            ((i128::MIN, 0), (i128::MIN, 38), Ordering::Less),
            ((i128::MAX, 0), (i128::MAX, 38), Ordering::Greater),
            ((i128::MIN, 0), (i128::MIN + 1, 0), Ordering::Less),
            ((i128::MAX - 1, 38), (i128::MAX, 38), Ordering::Less),
            ((1, 0), (i128::MAX, 38), Ordering::Less),
            ((-1, 0), (i128::MIN, 38), Ordering::Greater),
            ((i128::MAX / 10, 0), (i128::MAX, 1), Ordering::Less),
            ((i128::MIN / 10, 0), (i128::MIN, 1), Ordering::Greater),
        ];
        for ((left, left_scale), (right, right_scale), expected) in vectors {
            let left = Decimal128::new(left, left_scale);
            let right = Decimal128::new(right, right_scale);
            assert_eq!(compare_decimal(left, right), expected);
            assert_eq!(compare_decimal(right, left), expected.reverse());
            let value = decimal(Some(left), Some(right));
            let schema = graph(vec![TypeNode::Scalar(value)]);
            if expected == Ordering::Greater { assert_invalid(&schema, 0); }
            else { validate(&schema).unwrap(); }
        }
    }

    #[test]
    fn decimal_comparison_matches_safe_cross_multiplication_for_small_values() {
        // This independent oracle only uses coefficients whose products fit.
        for left in [-101_i128, -10, -1, 0, 1, 10, 101] {
            for right in [-101_i128, -10, -1, 0, 1, 10, 101] {
                for left_scale in 0..=3 {
                    for right_scale in 0..=3 {
                        let expected = (left * 10_i128.pow(u32::from(right_scale)))
                            .cmp(&(right * 10_i128.pow(u32::from(left_scale))));
                        assert_eq!(compare_decimal(Decimal128::new(left, left_scale),
                            Decimal128::new(right, right_scale)), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn accessors_follow_alias_chains_and_preserve_declaration_order() {
        let scalar = ScalarConstraints::new(ValueKind::String);
        let mut schema = graph(vec![
            TypeNode::Ref(String::from("root")),
            object(vec![prop(90, "z", 3, true, None), prop(2, "a", 4, false, None),
                prop(7, "", 3, true, None)]),
            TypeNode::Ref(String::from("object")),
            TypeNode::Ref(String::from("text")),
            TypeNode::Scalar(scalar.clone()),
            TypeNode::Any,
            TypeNode::Array(ArrayShape { items: TypeId(3), min_items: None, max_items: None }),
        ]);
        schema.definitions = vec![
            NamedType { name: String::from("root"), ty: TypeId(2) },
            NamedType { name: String::from("object"), ty: TypeId(1) },
            NamedType { name: String::from("text"), ty: TypeId(4) },
        ];
        let resolved = resolve_fixture(schema);
        let original = resolved.schema().clone();
        for ty in [TypeId(0), TypeId(1), TypeId(2)] {
            assert!(constraints(&resolved, ty).unwrap().is_none());
            assert_eq!(required_fields(&resolved, ty).unwrap(), vec![FieldId(90), FieldId(7)]);
            let found = property(&resolved, ty, "z").unwrap().unwrap();
            assert_eq!(found.field_id, FieldId(90));
            assert_eq!(found.ty, TypeId(3));
            assert_eq!(property(&resolved, ty, "").unwrap().unwrap().field_id, FieldId(7));
            assert!(property(&resolved, ty, "Z").unwrap().is_none());
        }
        assert_eq!(constraints(&resolved, TypeId(3)).unwrap(), Some(&scalar));
        assert!(core::ptr::eq(constraints(&resolved, TypeId(3)).unwrap().unwrap(),
            constraints(&resolved, TypeId(4)).unwrap().unwrap()));
        for ty in [TypeId(5), TypeId(6)] {
            assert!(constraints(&resolved, ty).unwrap().is_none());
        }
        for (ty, field) in [(3, 90), (4, 2), (5, 0), (6, 0)] {
            assert_error(property(&resolved, TypeId(ty), "missing").unwrap_err(), ErrorCode::TypeMismatch, field);
            assert_error(required_fields(&resolved, TypeId(ty)).unwrap_err(), ErrorCode::TypeMismatch, field);
        }
        for ty in [7, u32::MAX] {
            assert_error(constraints(&resolved, TypeId(ty)).unwrap_err(), ErrorCode::ConstraintViolation, 0);
            assert_error(property(&resolved, TypeId(ty), "").unwrap_err(), ErrorCode::ConstraintViolation, 0);
            assert_error(required_fields(&resolved, TypeId(ty)).unwrap_err(), ErrorCode::ConstraintViolation, 0);
        }
        assert_eq!(resolved.schema(), &original);
    }

    #[test]
    fn count_and_reservation_errors_are_budget_errors() {
        assert_eq!(count(0, ID, None).unwrap(), 0);
        assert_eq!(count(u32::MAX as usize, ID, None).unwrap(), u32::MAX);
        #[cfg(target_pointer_width = "64")]
        assert_error(count(u32::MAX as usize + 1, ID, Some(FieldId(42))).unwrap_err(),
            ErrorCode::BudgetExhausted, 42);
        let mut values = Vec::<u8>::new();
        // Capacity overflow fails before asking the allocator for memory.
        assert_error(reserve(&mut values, usize::MAX, ID).unwrap_err(), ErrorCode::BudgetExhausted, 0);
        assert!(values.is_empty());
        assert_eq!(copy_string("", ID, None).unwrap(), "");
        assert_eq!(copy_string("é\0数字", ID, None).unwrap(), "é\0数字");
    }
}
