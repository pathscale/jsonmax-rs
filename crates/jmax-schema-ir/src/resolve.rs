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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    const ID: SchemaId = SchemaId(216);

    fn graph(nodes: Vec<TypeNode>) -> Schema {
        Schema {
            name: String::from("test"), schema_id: ID, root: TypeId(0),
            nodes, definitions: Vec::new(),
        }
    }

    fn reference(name: &str) -> TypeNode {
        TypeNode::Ref(String::from(name))
    }

    fn definition(name: &str, ty: u32) -> NamedType {
        NamedType { name: String::from(name), ty: TypeId(ty) }
    }

    fn property(id: u32, name: &str, ty: u32) -> Property {
        Property {
            field_id: FieldId(id), name: String::from(name), ty: TypeId(ty),
            required: false, slot: None,
        }
    }

    fn object(properties: Vec<Property>) -> TypeNode {
        TypeNode::Object(ObjectShape {
            properties, reject_unknown: true, min_properties: None, max_properties: None,
        })
    }

    fn array(items: u32) -> TypeNode {
        TypeNode::Array(ArrayShape { items: TypeId(items), min_items: None, max_items: None })
    }

    fn resolved(schema: Schema) -> ResolvedSchema {
        resolve(schema, ResolveLimits::default()).unwrap()
    }

    fn assert_error(failure: Error, code: ErrorCode, field: u32) {
        assert_eq!(failure.code, code);
        assert_eq!(failure.source, ErrorSource::SchemaCompile);
        assert_eq!(failure.byte_offset, 0);
        assert_eq!(failure.schema_id, ID.0);
        assert_eq!(failure.field_id, field);
        assert!(!failure.detail.is_empty());
    }

    fn assert_witness(schema: &ResolvedSchema, expected: &[u32]) {
        assert_eq!(schema.targets.len(), schema.schema.nodes.len());
        assert_eq!(expected.len(), schema.targets.len());
        for (index, expected) in expected.iter().enumerate() {
            let id = TypeId(index as u32);
            let target = TypeId(*expected);
            assert_eq!(resolve_type(schema, id).unwrap(), target);
            assert_eq!(schema.target(id), Some(target));
            assert!(!matches!(schema.node(id), Some(TypeNode::Ref(_)) | None));
            assert_eq!(resolve_type(schema, target).unwrap(), target);
            if !matches!(&schema.schema.nodes[index], TypeNode::Ref(_)) {
                assert_eq!(id, target);
            }
        }
        assert_eq!(schema.root(), TypeId(expected[schema.schema.root.0 as usize]));
    }

    #[test]
    fn constructors_are_identity_targets_and_metadata_is_preserved() {
        let mut input = graph(vec![
            TypeNode::Any,
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
            object(vec![property(7, "", 1)]),
            array(2),
        ]);
        input.root = TypeId(3);
        input.definitions = vec![definition("unused", 0)];
        let before = input.clone();
        let schema = resolved(input);
        assert_eq!(schema.schema(), &before);
        assert_witness(&schema, &[0, 1, 2, 3]);
        assert!(!has_recursion(&schema).unwrap());
    }

    #[test]
    fn aliases_collapse_forward_backward_and_shared_chains() {
        let mut input = graph(vec![
            reference("b"), reference("a"), TypeNode::Any,
            reference("b"), reference("c"), reference("leaf"),
        ]);
        input.definitions = vec![
            definition("leaf", 2), definition("c", 3),
            definition("b", 1), definition("a", 5),
        ];
        let before = input.clone();
        let schema = resolved(input);
        assert_witness(&schema, &[2, 2, 2, 2, 2, 2]);
        assert!(!has_recursion(&schema).unwrap());
        assert_eq!(schema.schema(), &before);
    }

    #[test]
    fn exact_names_include_pointer_like_url_like_nul_and_unicode_strings() {
        let names = ["A", "a", "#/$defs/A", "https://example.invalid/A", "é", "e\u{301}", "\0"];
        let mut input = graph(Vec::new());
        for (index, name) in names.iter().enumerate() {
            input.nodes.push(reference(name));
            input.definitions.push(definition(name, (names.len() + index) as u32));
        }
        for _ in names {
            input.nodes.push(TypeNode::Any);
        }
        input.definitions.reverse();
        let schema = resolved(input);
        for index in 0..names.len() {
            assert_eq!(resolve_type(&schema, TypeId(index as u32)).unwrap(),
                TypeId((names.len() + index) as u32));
        }
        assert!(!has_recursion(&schema).unwrap());
    }

    #[test]
    fn unknown_names_are_not_normalized_or_interpreted() {
        for (name, known) in [("a", "A"), ("é", "e\u{301}"),
            ("#/$defs/A", "A"), ("https://example.invalid/A", "A"), (" A", "A")]
        {
            let mut input = graph(vec![reference(name), TypeNode::Any]);
            input.definitions.push(definition(known, 1));
            assert_error(resolve(input, ResolveLimits::default()).unwrap_err(),
                ErrorCode::UnsupportedSchemaFeature, 0);
        }
    }

    #[test]
    fn unknown_and_cyclic_aliases_are_rejected_even_when_unreachable() {
        let unknown = graph(vec![TypeNode::Any, reference("missing")]);
        assert_error(resolve(unknown, ResolveLimits::default()).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, 0);
        let mut direct = graph(vec![TypeNode::Any, reference("self")]);
        direct.definitions.push(definition("self", 1));
        assert_error(resolve(direct, ResolveLimits::default()).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, 0);
        let mut mutual = graph(vec![TypeNode::Any, reference("b"), reference("a")]);
        mutual.definitions = vec![definition("a", 1), definition("b", 2)];
        assert_error(resolve(mutual, ResolveLimits::default()).unwrap_err(),
            ErrorCode::UnsupportedSchemaFeature, 0);
    }

    #[test]
    fn alias_errors_preserve_direct_and_chain_entry_field_context() {
        for cyclic in [false, true] {
            let mut input = graph(vec![
                object(vec![property(42, "child", 1)]), reference("middle"), reference("last"),
            ]);
            input.definitions.push(definition("middle", 2));
            if cyclic {
                input.definitions.push(definition("last", 2));
            }
            assert_error(resolve(input, ResolveLimits::default()).unwrap_err(),
                ErrorCode::UnsupportedSchemaFeature, 42);
        }
    }

    #[test]
    fn validation_rejects_invalid_ids_before_constructing_a_witness() {
        assert_error(resolve(graph(Vec::new()), ResolveLimits::default()).unwrap_err(),
            ErrorCode::ConstraintViolation, 0);
        for id in [1, u32::MAX] {
            let mut root = graph(vec![TypeNode::Any]);
            root.root = TypeId(id);
            let mut named = graph(vec![TypeNode::Any]);
            named.definitions.push(definition("bad", id));
            for input in [root, named, graph(vec![array(id)])] {
                assert_error(resolve(input, ResolveLimits::default()).unwrap_err(),
                    ErrorCode::ConstraintViolation, 0);
            }
            assert_error(resolve(graph(vec![object(vec![property(42, "bad", id)])]),
                ResolveLimits::default()).unwrap_err(), ErrorCode::ConstraintViolation, 42);
        }
    }

    #[test]
    fn validation_propagates_empty_duplicate_and_unreachable_constraint_errors() {
        let mut duplicate = graph(vec![TypeNode::Any]);
        duplicate.definitions = vec![definition("x", 0), definition("x", 0)];
        let mut empty = graph(vec![TypeNode::Any]);
        empty.definitions.push(definition("", 0));
        let mut bad_scalar = ScalarConstraints::new(ValueKind::Integer);
        bad_scalar.min_i64 = Some(2);
        bad_scalar.max_i64 = Some(1);
        for input in [duplicate, empty, graph(vec![reference("")]),
            graph(vec![TypeNode::Any, TypeNode::Scalar(bad_scalar)])]
        {
            assert_error(resolve(input, ResolveLimits::default()).unwrap_err(),
                ErrorCode::ConstraintViolation, 0);
        }
        let input = graph(vec![object(vec![property(42, "x", 0), property(43, "x", 0)])]);
        assert_error(resolve(input, ResolveLimits::default()).unwrap_err(),
            ErrorCode::ConstraintViolation, 43);
    }

    #[test]
    fn node_and_definition_limits_are_inclusive_and_cover_unreachable_data() {
        let mut input = graph(vec![TypeNode::Any, TypeNode::Any]);
        input.definitions = vec![definition("a", 0), definition("b", 1)];
        let limits = ResolveLimits { max_nodes: 2, max_definitions: 2, ..ResolveLimits::default() };
        resolve(input.clone(), limits).unwrap();
        for limits in [ResolveLimits { max_nodes: 1, ..limits },
            ResolveLimits { max_definitions: 1, ..limits },
            ResolveLimits { max_nodes: 0, ..limits },
            ResolveLimits { max_definitions: 0, ..limits }]
        {
            assert_error(resolve(input.clone(), limits).unwrap_err(), ErrorCode::BudgetExhausted, 0);
        }
    }

    #[test]
    fn zero_name_and_reference_budgets_allow_empty_names_and_no_aliases() {
        let mut input = graph(vec![object(vec![property(42, "", 0)])]);
        input.name.clear();
        let limits = ResolveLimits {
            max_nodes: 1, max_definitions: 0, max_reference_hops: 0, max_name_bytes: 0,
        };
        let schema = resolve(input, limits).unwrap();
        assert_witness(&schema, &[0]);
        assert!(has_recursion(&schema).unwrap());
    }

    #[test]
    fn every_name_category_obeys_an_individual_utf8_byte_limit() {
        // U+00E9 occupies two UTF-8 bytes; repeated names do not share a budget.
        let mut input = graph(vec![object(vec![property(42, "é", 1)]), reference("é"), TypeNode::Any]);
        input.name = String::from("é");
        input.definitions.push(definition("é", 2));
        let limits = ResolveLimits { max_name_bytes: 2, ..ResolveLimits::default() };
        resolve(input.clone(), limits).unwrap();
        for category in 0..4 {
            let mut oversized = input.clone();
            let field = match category {
                0 => { oversized.name.push('x'); 0 }
                1 => { oversized.definitions[0].name.push('x'); 0 }
                2 => { oversized.nodes[1] = reference("éx"); 42 }
                _ => {
                    if let TypeNode::Object(object) = &mut oversized.nodes[0] {
                        object.properties[0].name.push('x');
                    }
                    42
                }
            };
            assert_error(resolve(oversized, limits).unwrap_err(), ErrorCode::BudgetExhausted, field);
        }
        input.name.clear();
        input.definitions.clear();
        input.nodes = vec![TypeNode::Any, object(vec![property(42, "é", 0)])];
        assert_error(resolve(input, ResolveLimits { max_name_bytes: 1, ..limits }).unwrap_err(),
            ErrorCode::BudgetExhausted, 42);
    }

    #[test]
    fn pattern_and_format_strings_are_not_reference_names() {
        let mut scalar = ScalarConstraints::new(ValueKind::String);
        scalar.pattern = Some(String::from("a long pattern"));
        scalar.format = Some(String::from("a long format"));
        let mut input = graph(vec![TypeNode::Scalar(scalar)]);
        input.name.clear();
        resolve(input, ResolveLimits { max_name_bytes: 0, ..ResolveLimits::default() }).unwrap();
    }

    // reverse=true resolves short suffixes before their longer predecessors,
    // exercising the hop count carried by already-completed aliases.
    fn chain(length: u32, reverse: bool) -> Schema {
        let mut input = graph(Vec::new());
        if reverse {
            input.nodes.push(TypeNode::Any);
            for index in 1..=length {
                let name = format!("n{index}");
                input.nodes.push(reference(&name));
                input.definitions.push(definition(&name, index - 1));
            }
            input.root = TypeId(length);
        } else {
            for index in 0..length {
                let name = format!("n{index}");
                input.nodes.push(reference(&name));
                input.definitions.push(definition(&name, index + 1));
            }
            input.nodes.push(TypeNode::Any);
        }
        input
    }

    #[test]
    fn hop_limits_include_cached_suffixes_and_are_inclusive() {
        for reverse in [false, true] {
            for length in [1, 2, 7] {
                let input = chain(length, reverse);
                let limits = ResolveLimits { max_reference_hops: length, ..ResolveLimits::default() };
                let schema = resolve(input.clone(), limits).unwrap();
                assert_eq!(schema.root(), TypeId(if reverse { 0 } else { length }));
                assert!(!has_recursion(&schema).unwrap());
                for maximum in [0, length - 1] {
                    assert_error(resolve(input.clone(), ResolveLimits {
                        max_reference_hops: maximum, ..limits
                    }).unwrap_err(), ErrorCode::BudgetExhausted, 0);
                }
            }
        }
    }

    #[test]
    fn unreachable_aliases_still_consume_the_per_chain_hop_budget() {
        let mut input = chain(3, true);
        input.root = TypeId(0);
        assert_error(resolve(input, ResolveLimits {
            max_reference_hops: 2, ..ResolveLimits::default()
        }).unwrap_err(), ErrorCode::BudgetExhausted, 0);
    }

    #[test]
    fn shared_suffixes_do_not_accumulate_a_global_hop_count() {
        let mut input = chain(2, false);
        for _ in 0..32 {
            input.nodes.push(reference("n0"));
        }
        let schema = resolve(input, ResolveLimits {
            max_reference_hops: 2, ..ResolveLimits::default()
        }).unwrap();
        assert!(schema.targets.iter().all(|target| *target == TypeId(2)));
    }

    #[test]
    fn self_recursive_objects_and_arrays_are_legal() {
        for node in [object(vec![property(42, "self", 0)]), array(0)] {
            let schema = resolved(graph(vec![node]));
            assert_witness(&schema, &[0]);
            assert!(has_recursion(&schema).unwrap());
        }
    }

    #[test]
    fn alias_root_and_constructor_cycles_reset_reference_hops() {
        let mut input = graph(vec![
            reference("object"), object(vec![property(42, "children", 2)]),
            reference("array"), array(0),
        ]);
        input.definitions = vec![definition("array", 3), definition("object", 1)];
        let before = input.clone();
        let schema = resolve(input, ResolveLimits {
            max_reference_hops: 1, ..ResolveLimits::default()
        }).unwrap();
        assert_witness(&schema, &[1, 1, 3, 3]);
        assert!(has_recursion(&schema).unwrap());
        assert_eq!(schema.schema(), &before);
    }

    #[test]
    fn recursion_detection_ignores_unreachable_cycles_and_named_definitions() {
        let mut input = graph(vec![TypeNode::Any, array(1), reference("loop")]);
        input.definitions.push(definition("loop", 1));
        let schema = resolved(input.clone());
        assert_witness(&schema, &[0, 1, 1]);
        assert!(!has_recursion(&schema).unwrap());
        input.root = TypeId(2);
        assert!(has_recursion(&resolved(input)).unwrap());
    }

    #[test]
    fn sharing_and_diamonds_are_not_cycles() {
        let input = graph(vec![
            object(vec![property(1, "left", 1), property(2, "right", 2), property(3, "again", 1)]),
            object(vec![property(4, "leaf", 3)]), array(3), TypeNode::Any,
        ]);
        let schema = resolved(input);
        assert!(!has_recursion(&schema).unwrap());
        assert!(!has_recursion(&schema).unwrap());
        assert_witness(&schema, &[0, 1, 2, 3]);
    }

    #[test]
    fn cycle_in_a_later_property_is_not_skipped() {
        let input = graph(vec![
            object(vec![property(1, "leaf", 1), property(2, "branch", 2)]),
            TypeNode::Any, object(vec![property(3, "leaf", 1), property(4, "back", 0)]),
        ]);
        assert!(has_recursion(&resolved(input)).unwrap());
    }

    #[test]
    fn optional_properties_and_zero_item_bounds_still_form_structural_edges() {
        let mut input = graph(vec![object(vec![property(42, "optional", 1)]), array(0)]);
        if let TypeNode::Array(array) = &mut input.nodes[1] {
            array.max_items = Some(0);
        }
        assert!(has_recursion(&resolved(input)).unwrap());
    }

    #[test]
    fn long_alias_chains_use_bounded_explicit_storage() {
        for reverse in [false, true] {
            let schema = resolved(chain(8_192, reverse));
            let target = TypeId(if reverse { 0 } else { 8_192 });
            assert!(schema.targets.iter().all(|actual| *actual == target));
            assert!(!has_recursion(&schema).unwrap());
        }
    }

    #[test]
    fn deep_structural_graphs_do_not_use_recursive_calls() {
        let depth = 12_000;
        let mut input = graph(Vec::new());
        for index in 0..depth {
            input.nodes.push(array(index + 1));
        }
        input.nodes.push(TypeNode::Any);
        assert!(!has_recursion(&resolved(input.clone())).unwrap());
        input.nodes[depth as usize] = array(0);
        assert!(has_recursion(&resolved(input)).unwrap());
    }

    #[test]
    fn all_three_node_graphs_match_an_independent_transitive_closure_oracle() {
        // Every directed graph on three nodes, including loops and diamonds.
        for mask in 0_u32..512 {
            let mut input = graph(Vec::new());
            let mut reach = [[false; 3]; 3];
            for source in 0..3 {
                let mut properties = Vec::new();
                for target in 0..3 {
                    let bit = source * 3 + target;
                    if mask & (1 << bit) != 0 {
                        reach[source][target] = true;
                        properties.push(property(bit as u32, &format!("p{target}"), target as u32));
                    }
                }
                input.nodes.push(object(properties));
            }
            for middle in 0..3 {
                for source in 0..3 {
                    for target in 0..3 {
                        reach[source][target] = reach[source][target]
                            || (reach[source][middle] && reach[middle][target]);
                    }
                }
            }
            for root in 0..3 {
                input.root = TypeId(root as u32);
                let expected = (0..3).any(|node| {
                    (node == root || reach[root][node]) && reach[node][node]
                });
                assert_eq!(has_recursion(&resolved(input.clone())).unwrap(), expected,
                    "graph mask {mask}, root {root}");
            }
        }
    }

    #[test]
    fn invalid_accessor_ids_return_errors_without_mutating_the_witness() {
        let schema = resolved(chain(2, false));
        let before = schema.schema().clone();
        let targets = schema.targets.clone();
        for id in [3, u32::MAX] {
            assert_error(resolve_type(&schema, TypeId(id)).unwrap_err(),
                ErrorCode::ConstraintViolation, 0);
            assert!(schema.target(TypeId(id)).is_none());
        }
        assert_eq!(schema.schema(), &before);
        assert_eq!(schema.targets, targets);
    }

    #[test]
    fn capacity_overflow_and_unrepresentable_counts_are_budget_errors() {
        let input = graph(vec![TypeNode::Any]);
        let mut storage = Vec::<u8>::new();
        assert_error(reserve(&mut storage, usize::MAX, &input).unwrap_err(),
            ErrorCode::BudgetExhausted, 0);
        assert!(storage.is_empty());
        check_size(0, 0, &input, None, "count").unwrap();
        check_size(u32::MAX as usize, u32::MAX, &input, None, "count").unwrap();
        assert_error(check_size(1, 0, &input, Some(FieldId(42)), "count").unwrap_err(),
            ErrorCode::BudgetExhausted, 42);
        #[cfg(target_pointer_width = "64")]
        assert_error(check_size(u32::MAX as usize + 1, u32::MAX, &input,
            Some(FieldId(42)), "count").unwrap_err(), ErrorCode::BudgetExhausted, 42);
    }
}
