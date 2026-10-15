//! Exact local references and stack-independent recursive graph traversal.

use super::*;

const UNSEEN: u8 = 0;
const ACTIVE: u8 = 1;
const COMPLETE: u8 = 2;

fn error(schema: &Schema, field: Option<FieldId>, code: ErrorCode, detail: &'static str) -> Error {
    schema_error(code, detail).with_schema(schema.schema_id.0, field.map_or(0, |id| id.0))
}

fn incoming_field(schema: &Schema, ty: TypeId) -> Option<FieldId> {
    schema.nodes.iter().find_map(|node| match node {
        TypeNode::Object(object) => object.properties.iter()
            .find(|property| property.ty == ty).map(|property| property.field_id),
        _ => None,
    })
}

fn type_error(schema: &Schema, ty: TypeId, code: ErrorCode, detail: &'static str) -> Error {
    error(schema, incoming_field(schema, ty), code, detail)
}

fn reserve<T>(values: &mut Vec<T>, additional: usize, schema: &Schema) -> Result<()> {
    values.try_reserve_exact(additional).map_err(|_| {
        error(schema, None, ErrorCode::BudgetExhausted, "reference resolution allocation failed")
    })
}

fn check_size(value: usize, maximum: u32, schema: &Schema,
    field: Option<FieldId>, detail: &'static str) -> Result<()>
{
    if u32::try_from(value).map_or(true, |value| value > maximum) {
        return Err(error(schema, field, ErrorCode::BudgetExhausted, detail));
    }
    Ok(())
}

// Perform allocation-free budget checks before whole-graph validation allocates
// its duplicate-detection tables. Names are measured individually in UTF-8 bytes.
fn check_limits(schema: &Schema, limits: ResolveLimits) -> Result<()> {
    check_size(schema.nodes.len(), limits.max_nodes, schema, None, "node limit exceeded")?;
    check_size(schema.definitions.len(), limits.max_definitions, schema, None,
        "definition limit exceeded")?;
    check_size(schema.name.len(), limits.max_name_bytes, schema, None,
        "schema name byte limit exceeded")?;
    for definition in &schema.definitions {
        check_size(definition.name.len(), limits.max_name_bytes, schema, None,
            "definition name byte limit exceeded")?;
    }
    for (index, node) in schema.nodes.iter().enumerate() {
        match node {
            TypeNode::Ref(name) => {
                check_size(name.len(), limits.max_name_bytes, schema, None,
                    "reference name byte limit exceeded").map_err(|failure| {
                    failure.with_schema(schema.schema_id.0,
                        incoming_field(schema, TypeId(index as u32)).map_or(0, |id| id.0))
                })?;
            }
            TypeNode::Object(object) => {
                for property in &object.properties {
                    check_size(property.name.len(), limits.max_name_bytes, schema,
                        Some(property.field_id), "property name byte limit exceeded")?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate the entire stored graph and collapse every exact local reference.
///
/// Constructors terminate alias chains, so a reference back to an object or
/// array is legal. An alias-only cycle has no constructor and is unsupported.
/// The hop limit counts Ref nodes in each chain, including cached suffixes;
/// constructor edges reset that count. Unreachable references are checked too.
///
/// Definition lookup uses a sorted borrowed index, without changing declaration
/// order. Each alias is visited once, using O(nodes + definitions) auxiliary
/// storage and fallible reservations. No graph walk uses the machine stack.
pub fn resolve(schema: Schema, limits: ResolveLimits) -> Result<ResolvedSchema> {
    check_limits(&schema, limits)?;
    model::validate(&schema)?;

    let count = schema.nodes.len();
    let mut definitions = Vec::new();
    reserve(&mut definitions, schema.definitions.len(), &schema)?;
    definitions.extend(schema.definitions.iter());
    definitions.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));

    let mut targets = Vec::new();
    let mut colors = Vec::new();
    let mut hops = Vec::new();
    let mut path = Vec::new();
    reserve(&mut targets, count, &schema)?;
    reserve(&mut colors, count, &schema)?;
    reserve(&mut hops, count, &schema)?;
    let references = schema.nodes.iter().filter(|node| matches!(node, TypeNode::Ref(_))).count();
    reserve(&mut path, references, &schema)?;
    for (index, node) in schema.nodes.iter().enumerate() {
        // check_limits and model::validate established representable indices.
        targets.push(TypeId(index as u32));
        colors.push(if matches!(node, TypeNode::Ref(_)) { UNSEEN } else { COMPLETE });
        hops.push(0_u32);
    }

    for start in 0..count {
        if colors[start] == COMPLETE {
            continue;
        }
        let mut current = TypeId(start as u32);
        // Context lookup is deferred until an error, avoiding repeated graph
        // scans on successful resolutions. Prefer the offending node's field.
        let failure = |ty: TypeId, code, detail| {
            let field = incoming_field(&schema, ty)
                .or_else(|| incoming_field(&schema, TypeId(start as u32)));
            error(&schema, field, code, detail)
        };
        loop {
            let index = current.0 as usize;
            if colors[index] == COMPLETE {
                break;
            }
            if colors[index] == ACTIVE {
                return Err(failure(current, ErrorCode::UnsupportedSchemaFeature,
                    "alias-only reference cycle"));
            }
            if path.len() >= limits.max_reference_hops as usize {
                return Err(failure(current, ErrorCode::BudgetExhausted,
                    "reference hop limit exceeded"));
            }
            // Only Ref nodes start UNSEEN; every constructor starts COMPLETE.
            let name = match &schema.nodes[index] {
                TypeNode::Ref(name) => name,
                _ => return Err(failure(current, ErrorCode::ConstraintViolation,
                    "invalid reference traversal state")),
            };
            let definition = definitions.binary_search_by(|definition| {
                definition.name.as_bytes().cmp(name.as_bytes())
            }).map_err(|_| failure(current, ErrorCode::UnsupportedSchemaFeature,
                "unknown local reference name"))?;
            colors[index] = ACTIVE;
            path.push(current);
            current = definitions[definition].ty;
        }

        let target = targets[current.0 as usize];
        let mut distance = hops[current.0 as usize];
        while let Some(alias) = path.pop() {
            distance = distance.checked_add(1).filter(|value| *value <= limits.max_reference_hops)
                .ok_or_else(|| failure(alias, ErrorCode::BudgetExhausted,
                    "reference hop limit exceeded"))?;
            let index = alias.0 as usize;
            targets[index] = target;
            hops[index] = distance;
            colors[index] = COMPLETE;
        }
    }

    // Publish the witness only after every stored alias has a non-Ref target.
    drop(definitions);
    Ok(ResolvedSchema { schema, targets })
}

/// Return the checked, fully collapsed target for a stored type ID.
pub fn resolve_type(schema: &ResolvedSchema, ty: TypeId) -> Result<TypeId> {
    let invalid = || type_error(&schema.schema, ty, ErrorCode::ConstraintViolation,
        "invalid resolved type ID or alias target");
    schema.schema.nodes.get(ty.0 as usize).ok_or_else(invalid)?;
    let target = schema.target(ty).ok_or_else(invalid)?;
    match schema.schema.nodes.get(target.0 as usize) {
        Some(TypeNode::Ref(_)) | None => Err(invalid()),
        Some(_) => Ok(target),
    }
}

struct Frame {
    ty: TypeId,
    next_edge: usize,
}

/// Detect cycles in the root-reachable, alias-collapsed structural graph.
///
/// A gray edge returns to an ancestor and proves recursion. Edges to completed
/// nodes only represent sharing. An explicit DFS frame retains the next edge,
/// so each reachable edge is examined once and stack storage is bounded by the
/// stored node count, even for objects with many properties or repeated targets.
pub fn has_recursion(schema: &ResolvedSchema) -> Result<bool> {
    let root = resolve_type(schema, schema.schema.root)?;
    let count = schema.schema.nodes.len();
    let mut colors = Vec::new();
    let mut stack = Vec::new();
    reserve(&mut colors, count, &schema.schema)?;
    reserve(&mut stack, count, &schema.schema)?;
    colors.resize(count, UNSEEN);
    colors[root.0 as usize] = ACTIVE;
    stack.push(Frame { ty: root, next_edge: 0 });

    while let Some(frame) = stack.last_mut() {
        let edge = match &schema.schema.nodes[frame.ty.0 as usize] {
            TypeNode::Object(object) => object.properties.get(frame.next_edge)
                .map(|property| property.ty),
            TypeNode::Array(array) if frame.next_edge == 0 => Some(array.items),
            _ => None,
        };
        let child = match edge {
            Some(child) => {
                frame.next_edge += 1;
                resolve_type(schema, child)?
            }
            None => {
                colors[frame.ty.0 as usize] = COMPLETE;
                stack.pop();
                continue;
            }
        };
        match colors[child.0 as usize] {
            ACTIVE => return Ok(true),
            COMPLETE => {}
            _ => {
                colors[child.0 as usize] = ACTIVE;
                stack.push(Frame { ty: child, next_edge: 0 });
            }
        }
    }
    Ok(false)
}
