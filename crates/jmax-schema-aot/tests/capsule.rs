//! The flat projection of a compiled program verifies with its schema capsule
//! after the key names, and with nothing else after them.

use jmax_schema_aot::{compile_schema, tables, StaticSchemaImage};
use jmax_schema_ir::{ScalarKind, SchemaFieldIr, SchemaId, SchemaIr};

fn program() -> jmax_schema_aot::ValidatedProgram {
    let ir = SchemaIr {
        name: "capsule".into(),
        schema_id: SchemaId(21),
        required_count: 1,
        reject_unknown: true,
        fields: vec![SchemaFieldIr {
            field_id: jmax_abi::schema::FieldId(1), name: "a".into(), required: true,
            scalar_kind: Some(ScalarKind::String), format: None, min_len: None, max_len: None,
            min_i64: None, max_i64: None,
        }],
    };
    compile_schema(&ir).unwrap()
}

#[test]
fn the_capsule_is_the_only_allowed_suffix() {
    let program = program();
    let image = program.image();
    // As compiled: names, then the capsule.
    assert!(image.key_bytes.len() > 1);
    tables::to_owned(StaticSchemaImage::from(image)).unwrap();
    assert!(tables::emit_program(&program, &jmax_schema_aot::TableOptions {
        symbol: "S", abi_path: "::jmax_abi", aot_path: "::jmax_schema_aot",
    }).is_ok());

    let with = |bytes: Vec<u8>| {
        let borrowed = StaticSchemaImage {
            header: image.header, insns: &image.insns, keys: &image.keys, key_bytes: &bytes,
        };
        tables::to_owned(borrowed).is_ok()
    };
    let good = image.key_bytes.clone();
    assert!(with(good.clone()));
    // Names only, without a capsule, is also a valid projection.
    assert!(with(good[..1].to_vec()));
    // Anything else after the names is not.
    let mut extra = good.clone();
    extra.push(0);
    assert!(!with(extra));
    assert!(!with(good[..good.len() - 1].to_vec()));
    let mut bad_magic = good.clone();
    *bad_magic.last_mut().unwrap() ^= 1;
    assert!(!with(bad_magic));
    let mut bad_length = good.clone();
    let footer = good.len() - 12;
    bad_length[footer] = bad_length[footer].wrapping_add(1);
    assert!(!with(bad_length));
    let mut loose = good[..1].to_vec();
    loose.extend_from_slice(b"JMXSCM02");
    assert!(!with(loose));
}
