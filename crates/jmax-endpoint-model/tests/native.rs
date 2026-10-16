use jmax_endpoint_model::native::{NativeIdent, NativeSpec, schema_graph_for, validate_native_spec};
use jmax_endpoint_model::registry::build_registry;
use jmax_endpoint_model::{
    EndpointField, EndpointModel, EndpointType, ErrorCode, SchemaId, TypeDefinition,
};
use jmax_schema_ir::{ResolveLimits, TypeNode, ValueKind, resolve};

fn field(name: &str, ty: EndpointType) -> EndpointField {
    EndpointField { name: name.into(), ty, description: String::new() }
}

fn strukt(name: &str, fields: Vec<EndpointField>) -> TypeDefinition {
    TypeDefinition { name: name.into(), ty: EndpointType::Struct { name: name.into(), fields } }
}

fn level_ref() -> EndpointType {
    EndpointType::Vec(Box::new(EndpointType::StructRef("PriceLevel".into())))
}

fn model(definitions: Vec<TypeDefinition>) -> EndpointModel {
    EndpointModel { definitions, ..EndpointModel::default() }
}

fn binance() -> EndpointModel {
    use EndpointType::*;
    model(vec![
        strukt("PriceLevel", vec![field("price", BlockchainDecimal), field("quantity", BlockchainDecimal)]),
        strukt("AggTrade", vec![
            field("e", String), field("E", TimeStampMs), field("s", String), field("a", Int64),
            field("p", BlockchainDecimal), field("q", BlockchainDecimal), field("f", Int64),
            field("l", Int64), field("T", TimeStampMs), field("m", Boolean),
            field("M", Optional(Box::new(Boolean))),
        ]),
        strukt("DepthUpdate", vec![
            field("e", String), field("E", TimeStampMs), field("s", String), field("U", Int64),
            field("u", Int64), field("b", level_ref()), field("a", level_ref()),
        ]),
    ])
}

fn spec() -> NativeSpec {
    NativeSpec {
        roots: vec!["AggTrade".into(), "DepthUpdate".into()],
        tuples: vec!["PriceLevel".into()],
        idents: vec![NativeIdent {
            type_name: "AggTrade".into(), wire: "e".into(), ident: "event_type".into(),
        }],
        ..NativeSpec::default()
    }
}

#[test]
fn agg_trade_lowers_to_a_flat_object() {
    let model = binance();
    let registry = build_registry(&model).unwrap();
    let schema = schema_graph_for(&registry, &spec(), "AggTrade", SchemaId(7)).unwrap();
    assert_eq!(schema.name, "AggTrade");
    assert_eq!(schema.schema_id, SchemaId(7));
    assert_eq!(schema.nodes.len(), 12);
    let TypeNode::Object(object) = &schema.nodes[0] else { panic!("root must be an object") };
    assert!(object.reject_unknown);
    let names: Vec<_> = object.properties.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["e", "E", "s", "a", "p", "q", "f", "l", "T", "m", "M"]);
    let required: Vec<_> = object.properties.iter().map(|p| p.required).collect();
    assert_eq!(required.iter().filter(|r| !**r).count(), 1);
    assert!(!required[10]);
    let ids: Vec<_> = object.properties.iter().map(|p| p.field_id.0).collect();
    assert_eq!(ids, (1..=11).collect::<Vec<_>>());
    let kind = |index: usize| match &schema.nodes[object.properties[index].ty.0 as usize] {
        TypeNode::Scalar(c) => (c.kind, c.min_i64, c.max_i64),
        other => panic!("scalar expected, got {other:?}"),
    };
    assert_eq!(kind(0).0, ValueKind::String);
    assert_eq!(kind(1), (ValueKind::Integer, Some(i64::MIN), Some(i64::MAX)));
    assert_eq!(kind(4).0, ValueKind::Decimal);
    assert_eq!(kind(9).0, ValueKind::Boolean);
    assert_eq!(kind(10).0, ValueKind::Boolean);
    resolve::resolve(schema, ResolveLimits::default()).unwrap();
}

#[test]
fn depth_update_levels_are_tuple_arrays() {
    let model = binance();
    let registry = build_registry(&model).unwrap();
    let schema = schema_graph_for(&registry, &spec(), "DepthUpdate", SchemaId(8)).unwrap();
    let TypeNode::Object(root) = &schema.nodes[0] else { panic!() };
    let bids = &schema.nodes[root.properties[5].ty.0 as usize];
    let TypeNode::Array(outer) = bids else { panic!("bids must be an array") };
    assert_eq!((outer.min_items, outer.max_items), (None, None));
    let TypeNode::Array(level) = &schema.nodes[outer.items.0 as usize] else { panic!() };
    assert_eq!((level.min_items, level.max_items), (Some(2), Some(2)));
    let TypeNode::Scalar(item) = &schema.nodes[level.items.0 as usize] else { panic!() };
    assert_eq!(item.kind, ValueKind::Decimal);
    assert_eq!(item.decimal.as_ref().unwrap().policy, spec().decimal_policy);
    // Field ids stay unique across the whole graph, even for repeated types.
    let mut ids: Vec<u32> = schema.nodes.iter().filter_map(|n| match n {
        TypeNode::Object(o) => Some(o.properties.iter().map(|p| p.field_id.0).collect::<Vec<_>>()),
        _ => None,
    }).flatten().collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count);
    // No node is left over from the tuple elements that share one items node.
    let reachable = {
        let mut seen = vec![false; schema.nodes.len()];
        let mut stack = vec![0usize];
        while let Some(i) = stack.pop() {
            seen[i] = true;
            match &schema.nodes[i] {
                TypeNode::Object(o) => stack.extend(o.properties.iter().map(|p| p.ty.0 as usize)),
                TypeNode::Array(a) => stack.push(a.items.0 as usize),
                _ => {}
            }
        }
        seen
    };
    assert!(reachable.iter().all(|r| *r));
    resolve::resolve(schema, ResolveLimits::default()).unwrap();
}

fn code<T: std::fmt::Debug>(result: jmax_endpoint_model::Result<T>) -> ErrorCode {
    result.unwrap_err().code
}

#[test]
fn unsupported_shapes_are_refused() {
    use EndpointType::*;
    let cases: std::vec::Vec<(&str, std::vec::Vec<EndpointField>, ErrorCode)> = vec![
        ("object", vec![field("x", Object)], ErrorCode::UnsupportedSchemaFeature),
        ("unit", vec![field("x", Unit)], ErrorCode::UnsupportedSchemaFeature),
        ("enum", vec![field("x", EnumRef { name: "E".into(), prefixed_name: false })], ErrorCode::UnsupportedSchemaFeature),
        ("nano", vec![field("x", NanoId { len: 8 })], ErrorCode::UnsupportedSchemaFeature),
        ("optional element", vec![field("x", Vec(Box::new(Optional(Box::new(Boolean)))))], ErrorCode::UnsupportedSchemaFeature),
        ("recursion", vec![field("x", Vec(Box::new(StructRef("Root".into()))))], ErrorCode::UnsupportedSchemaFeature),
    ];
    for (label, fields, expected) in cases {
        let mut definitions = vec![strukt("Root", fields)];
        if label == "enum" {
            definitions.push(TypeDefinition {
                name: "E".into(),
                ty: Enum { name: "E".into(), variants: vec![jmax_endpoint_model::EnumVariantSpec {
                    name: "A".into(), value: 1, description: std::string::String::new() }] },
            });
        }
        let model = model(definitions);
        let registry = build_registry(&model).unwrap();
        let spec = NativeSpec { roots: vec!["Root".into()], ..NativeSpec::default() };
        assert_eq!(code(schema_graph_for(&registry, &spec, "Root", SchemaId(1))), expected, "{label}");
    }
}

#[test]
fn heterogeneous_tuples_are_refused() {
    use EndpointType::*;
    let model = model(vec![
        strukt("Pair", vec![field("a", String), field("b", Int64)]),
        strukt("Root", vec![field("p", StructRef("Pair".into()))]),
    ]);
    let registry = build_registry(&model).unwrap();
    let spec = NativeSpec { roots: vec!["Root".into()], tuples: vec!["Pair".into()], ..NativeSpec::default() };
    assert_eq!(code(schema_graph_for(&registry, &spec, "Root", SchemaId(1))), ErrorCode::UnsupportedSchemaFeature);
    // The same struct is fine as an object.
    let spec = NativeSpec { roots: vec!["Root".into()], ..NativeSpec::default() };
    schema_graph_for(&registry, &spec, "Root", SchemaId(1)).unwrap();
}

#[test]
fn spec_validation_rejects_bad_names() {
    let model = binance();
    let registry = build_registry(&model).unwrap();
    let bad = |mutate: &dyn Fn(&mut NativeSpec)| {
        let mut spec = spec();
        mutate(&mut spec);
        code(validate_native_spec(&registry, &spec))
    };
    assert_eq!(bad(&|s| s.roots.push("Missing".into())), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.roots.push("AggTrade".into())), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.tuples.push("AggTrade".into())), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.tuples.push("DepthUpdate".into())), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.decimal_policy.max_precision = 0), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.decimal_policy.max_precision = 39), ErrorCode::ConstraintViolation);
    let ident = |ty: &str, wire: &str, ident: &str| NativeIdent {
        type_name: ty.into(), wire: wire.into(), ident: ident.into(),
    };
    assert_eq!(bad(&|s| s.idents.push(ident("AggTrade", "zz", "x"))), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.idents.push(ident("AggTrade", "E", "event_type"))), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.idents.push(ident("AggTrade", "e", "other"))), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.idents.push(ident("AggTrade", "E", "type"))), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.idents.push(ident("AggTrade", "E", "Upper"))), ErrorCode::ConstraintViolation);
    assert_eq!(bad(&|s| s.idents.push(ident("Nope", "E", "ok"))), ErrorCode::ConstraintViolation);
    validate_native_spec(&registry, &spec()).unwrap();
    // A root that is not listed cannot be lowered.
    assert_eq!(code(schema_graph_for(&registry, &spec(), "PriceLevel", SchemaId(1))), ErrorCode::ConstraintViolation);
}

#[test]
fn reject_unknown_follows_the_spec_and_lowering_is_deterministic() {
    let model = binance();
    let registry = build_registry(&model).unwrap();
    let mut open = spec();
    open.reject_unknown = false;
    let schema = schema_graph_for(&registry, &open, "AggTrade", SchemaId(1)).unwrap();
    let TypeNode::Object(object) = &schema.nodes[0] else { panic!() };
    assert!(!object.reject_unknown);
    let a = schema_graph_for(&registry, &spec(), "DepthUpdate", SchemaId(2)).unwrap();
    let b = schema_graph_for(&registry, &spec(), "DepthUpdate", SchemaId(2)).unwrap();
    assert_eq!(a, b);
}
