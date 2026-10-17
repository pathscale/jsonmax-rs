use jmax_schema_compile::{CompilerOptions, StateId, image, keys, scalar_kind_bit, swar_head};
use jmax_schema_ir::{
    FieldId, ObjectShape, Property, ScalarConstraints, ScalarKind, Schema, SchemaId, TypeId,
    TypeNode, ValueKind,
};

type Result<T = ()> = jmax_schema_compile::Result<T>;

fn compile(keys: u32) -> Result<jmax_schema_compile::ValidatedProgram> {
    let properties = (0..keys)
        .map(|id| Property {
            field_id: FieldId(id),
            name: format!("key-{id:04}"),
            ty: TypeId(1),
            required: id % 2 == 0,
            slot: Some(id),
        })
        .collect();
    let schema = Schema {
        name: "dispatch".into(),
        schema_id: SchemaId(6),
        root: TypeId(0),
        nodes: vec![
            TypeNode::Object(ObjectShape { nullable: false,
                properties,
                reject_unknown: false,
                min_properties: None,
                max_properties: None,
            }),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
        ],
        definitions: vec![],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

#[test]
fn swar_and_hash_dispatch_reuse_heads_and_preserve_exact_comparisons() -> Result {
    for key_count in [0, 1, 8, 16, 80] {
        let program = compile(key_count)?;
        for id in 0..key_count {
            let name = format!("key-{id:04}");
            let found = keys::lookup_with_swar(
                program.program(),
                0,
                name.as_bytes(),
                swar_head(name.as_bytes()),
            )?
            .unwrap();
            assert_eq!(u32::from(found.entry.field_id), id);
            assert_eq!(
                keys::lookup(program.program(), 0, name.as_bytes())?,
                Some(found)
            );
            assert_eq!(found.local_index, id);
            assert_eq!(found.index, id);
        }
        assert!(
            keys::lookup_with_swar(program.program(), 0, b"key-zzzz", swar_head(b"key-zzzz"))?
                .is_none()
        );
        assert!(
            keys::lookup_with_swar(
                program.program(),
                0,
                b"key-0000-tail",
                swar_head(b"key-0000-tail")
            )?
            .is_none()
        );
        assert!(keys::lookup_with_swar(program.program(), u32::MAX, b"", 0).is_err());
    }
    Ok(())
}

#[test]
fn compilation_and_image_load_prepare_identical_dispatch() -> Result {
    let options = CompilerOptions::default();
    let program = compile(16)?;
    let encoded = image::encode(&program, options.max_image_bytes)?;
    let loaded = image::decode(&encoded, &options)?;
    for state in &program.program().states {
        assert_eq!(program.dispatch(state.id), loaded.dispatch(state.id));
        assert_eq!(program.parents(state.id), loaded.parents(state.id));
    }
    let root = program.dispatch(StateId(0)).unwrap();
    assert_eq!(root.scalar_mask, 0);
    assert!(root.subtree_has_slots && root.subtree_has_constraints);
    assert!(root.unknown_fields_unconstrained);
    let leaf = program.dispatch(StateId(1)).unwrap();
    assert_eq!(leaf.scalar_mask, scalar_kind_bit(ScalarKind::String));
    assert!(leaf.has_slots);
    assert_eq!(leaf.slot_index, None); // several incoming property slots
    Ok(())
}

#[test]
fn recursive_dispatch_reaches_fixed_point_without_recursive_walks() -> Result {
    let schema = Schema {
        name: "recursive".into(),
        schema_id: SchemaId(7),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            TypeNode::Object(ObjectShape { nullable: false,
                properties: vec![
                    Property {
                        field_id: FieldId(0),
                        name: "next".into(),
                        ty: TypeId(0),
                        required: false,
                        slot: None,
                    },
                    Property {
                        field_id: FieldId(1),
                        name: "free".into(),
                        ty: TypeId(1),
                        required: false,
                        slot: None,
                    },
                ],
                reject_unknown: false,
                min_properties: None,
                max_properties: None,
            }),
            TypeNode::Any,
        ],
    };
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    let program = jmax_schema_compile::compile(&resolved, &CompilerOptions::default())?;
    let root = program.dispatch(StateId(0)).unwrap();
    assert!(!root.subtree_has_slots);
    assert!(root.subtree_has_constraints);
    let any = program
        .program()
        .states
        .iter()
        .find(|state| state.kind == jmax_schema_compile::StateKind::Any)
        .unwrap();
    let dispatch = program.dispatch(any.id).unwrap();
    assert!(!dispatch.subtree_has_constraints && !dispatch.subtree_has_slots);
    assert_eq!(dispatch.scalar_mask, 31);
    Ok(())
}
