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
