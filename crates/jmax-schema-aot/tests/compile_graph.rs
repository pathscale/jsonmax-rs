mod common;

use jmax_schema_aot::{compile_graph, compile_schema};
use jmax_schema_ir::{ScalarKind, SchemaFieldIr, SchemaId, SchemaIr, TypeId, TypeNode, model};
use jmax_schema_vm::{VmLimits, validate_borrowed};

#[test]
fn flat_ir_and_its_graph_compile_identically() {
    let ir = SchemaIr {
        name: "flat".into(),
        schema_id: SchemaId(11),
        required_count: 2,
        reject_unknown: true,
        fields: vec![
            SchemaFieldIr {
                field_id: jmax_abi::schema::FieldId(1), name: "a".into(), required: true,
                scalar_kind: Some(ScalarKind::String), format: None, min_len: Some(1), max_len: Some(8),
                min_i64: None, max_i64: None,
            },
            SchemaFieldIr {
                field_id: jmax_abi::schema::FieldId(2), name: "b".into(), required: true,
                scalar_kind: Some(ScalarKind::Number), format: None, min_len: None, max_len: None,
                min_i64: Some(0), max_i64: Some(100),
            },
        ],
    };
    let flat = compile_schema(&ir).unwrap();
    let graph = compile_graph(&model::normalize(&ir).unwrap()).unwrap();
    assert_eq!(flat.program().canonical_schema, graph.program().canonical_schema);
    assert_eq!(flat.image().header.fingerprint, graph.image().header.fingerprint);
    assert_eq!(flat.image().insns, graph.image().insns);
}

#[test]
fn input_is_not_modified_and_errors_propagate() {
    let ir = SchemaIr {
        name: "x".into(), schema_id: SchemaId(1), required_count: 0, reject_unknown: false, fields: vec![],
    };
    let schema = model::normalize(&ir).unwrap();
    let before = schema.clone();
    compile_graph(&schema).unwrap();
    assert_eq!(schema, before);
    let mut broken = schema;
    broken.nodes.push(TypeNode::Ref("missing".into()));
    broken.root = TypeId(1);
    assert!(compile_graph(&broken).is_err());
}

#[test]
fn nested_fixture_graphs_are_deterministic_and_decide_documents() {
    let first = common::fixture_programs();
    let second = common::fixture_programs();
    for ((root, a), (_, b)) in first.iter().zip(&second) {
        assert_eq!(a.program().canonical_schema, b.program().canonical_schema, "{root}");
        assert_eq!(a.image().header.fingerprint, b.image().header.fingerprint, "{root}");
        for seed in common::seeds_for(root) {
            validate_borrowed(a, &seed, VmLimits::default(), jmax_abi::number::NumericPolicy::strict_default())
                .unwrap_or_else(|e| panic!("{root} rejected a valid seed: {e:?}"));
        }
    }
    // A tuple of the wrong arity, a missing required key and an unknown key are all rejected.
    let depth = &first.iter().find(|(r, _)| r == "DepthUpdate").unwrap().1;
    let bad = [
        br#"{"e":"depthUpdate","E":1,"s":"A","U":1,"u":2,"b":[["1.0"]],"a":[]}"#.as_slice(),
        br#"{"e":"depthUpdate","E":1,"s":"A","U":1,"u":2,"b":[]}"#.as_slice(),
        br#"{"e":"depthUpdate","E":1,"s":"A","U":1,"u":2,"b":[],"a":[],"x":1}"#.as_slice(),
        br#"{"e":"depthUpdate","E":1,"s":"A","U":1,"u":2,"b":[["1.0","2.0","3.0"]],"a":[]}"#.as_slice(),
    ];
    for input in bad {
        let policy = jmax_abi::number::NumericPolicy::strict_default();
        assert!(validate_borrowed(depth, input, VmLimits::default(), policy).is_err());
    }
}
