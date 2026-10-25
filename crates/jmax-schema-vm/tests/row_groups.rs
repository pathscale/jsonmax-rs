//! Row groups stage per-element slots and publish them only after strict EOF.

use jmax_abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
use jmax_schema_compile::{CompilerOptions, RowShape, ValidatedProgram};
use jmax_schema_ir::{
    ArrayShape, DecimalConstraints, FieldId, ObjectShape, Property, ScalarConstraints, Schema,
    SchemaId, TypeId, TypeNode, ValueKind,
};
use jmax_schema_vm::{
    ErrorCode, Selection, SlotValue, ValidationScratch, VmLimits, direct, resolve_str,
};

type Result<T = ()> = jmax_schema_vm::Result<T>;

fn prop(id: u32, name: &str, ty: u32, required: bool, slot: u32) -> Property {
    Property {
        field_id: FieldId(id),
        name: name.into(),
        ty: TypeId(ty),
        required,
        slot: Some(slot),
    }
}

fn object(properties: Vec<Property>) -> TypeNode {
    TypeNode::Object(ObjectShape {
        nullable: false,
        properties,
        reject_unknown: true,
        min_properties: None,
        max_properties: None,
    })
}

fn array(items: u32, count: Option<u32>) -> TypeNode {
    TypeNode::Array(ArrayShape {
        nullable: false,
        items: TypeId(items),
        min_items: count,
        max_items: count,
    })
}

fn decimal() -> ScalarConstraints {
    let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
    decimal.decimal = Some(DecimalConstraints {
        policy: DecimalPolicy::spot_8(),
        minimum: None,
        maximum: None,
    });
    decimal
}

fn scalar(kind: ValueKind) -> TypeNode {
    TypeNode::Scalar(ScalarConstraints::new(kind))
}

/// `{"id":int,"rows":[{"n":int,"s":str,"f"?:bool}],"pairs":[[dec,dec]]}`
/// Slots: id 0, rows 1, n 2, s 3, f 4, pairs 5.
fn rows_schema(properties_of_item: Vec<Property>) -> Schema {
    Schema {
        name: "row_groups".into(),
        schema_id: SchemaId(7_071),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            object(vec![
                prop(1, "id", 9, true, 0),
                prop(2, "rows", 1, true, 1),
                prop(6, "pairs", 6, true, 5),
            ]),
            array(2, None),
            object(properties_of_item),
            scalar(ValueKind::Integer),
            scalar(ValueKind::String),
            scalar(ValueKind::Boolean),
            array(7, None),
            array(8, Some(2)),
            TypeNode::Scalar(decimal()),
            scalar(ValueKind::Integer),
        ],
    }
}

fn item_properties() -> Vec<Property> {
    vec![
        prop(3, "n", 3, true, 2),
        prop(4, "s", 4, true, 3),
        prop(5, "f", 5, false, 4),
    ]
}

fn compile(schema: Schema) -> Result<ValidatedProgram> {
    let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default())?;
    jmax_schema_compile::compile(&resolved, &CompilerOptions::default())
}

fn program() -> Result<ValidatedProgram> {
    compile(rows_schema(item_properties()))
}

fn all() -> Selection {
    Selection {
        field_ids: vec![],
        strict_validate_unselected: true,
    }
}

fn int(value: SlotValue) -> i64 {
    match value {
        SlotValue::I64(value) => value,
        SlotValue::U64(value) => value as i64,
        other => panic!("not an integer: {other:?}"),
    }
}

fn text<'a>(raw: &'a [u8], decoded: &'a [u8], value: SlotValue) -> &'a str {
    let SlotValue::Str(reference) = value else {
        panic!("not a string: {value:?}")
    };
    resolve_str(raw, decoded, reference).unwrap()
}

const GOOD: &[u8] = br#"{"id":7,"rows":[{"n":1,"s":"a"},{"n":2,"s":"b\n","f":true}],"pairs":[["1.5","2"],["3","4.25"]]}"#;

fn rows(
    program: &ValidatedProgram,
    raw: &[u8],
    scratch: &mut ValidationScratch,
) -> Result<Vec<Vec<Vec<SlotValue>>>> {
    let out = direct::extract_rows_borrowed_with_scratch(
        program,
        raw,
        &all(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        scratch,
    )?;
    Ok(out
        .groups
        .iter()
        .map(|block| block.rows().map(<[SlotValue]>::to_vec).collect())
        .collect())
}

#[test]
fn groups_are_derived_from_the_program() -> Result {
    let program = program()?;
    assert!(program.requires_rows());
    let shapes: Vec<_> = program
        .row_groups()
        .iter()
        .map(|g| (g.array_slot, g.shape))
        .collect();
    assert_eq!(
        shapes,
        vec![
            (
                1,
                RowShape::Items {
                    first_slot: 2,
                    width: 3
                }
            ),
            (5, RowShape::Leaf { width: 2 }),
        ]
    );
    assert_eq!(program.row_group_of_slot(1), Some(0));
    assert_eq!(program.row_group_of_slot(5), Some(1));
    assert_eq!(program.row_group_of_slot(0), None);
    Ok(())
}

#[test]
fn item_and_leaf_rows_are_staged_in_document_order() -> Result {
    let program = program()?;
    let mut scratch = ValidationScratch::default();
    let out = direct::extract_rows_borrowed_with_scratch(
        &program,
        GOOD,
        &all(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        &mut scratch,
    )?;
    assert_eq!(int(out.slots[0]), 7);
    assert!(matches!(out.slots[1], SlotValue::RawSpan(_)));
    assert!(matches!(out.slots[5], SlotValue::RawSpan(_)));
    // Item slots are staged, never left in the plain slots.
    assert!(out.slots[2..5].iter().all(|s| *s == SlotValue::Empty));
    let items = &out.groups[0];
    assert_eq!((items.array_slot, items.width, items.len()), (1, 3, 2));
    let rows: Vec<_> = items.rows().collect();
    assert_eq!(int(rows[0][0]), 1);
    assert_eq!(text(out.raw, out.decoded, rows[0][1]), "a");
    assert_eq!(rows[0][2], SlotValue::Empty);
    assert_eq!(int(rows[1][0]), 2);
    assert_eq!(text(out.raw, out.decoded, rows[1][1]), "b\n");
    assert_eq!(rows[1][2], SlotValue::Bool(true));
    let pairs = &out.groups[1];
    assert_eq!((pairs.array_slot, pairs.width, pairs.len()), (5, 2, 2));
    let want = [(150_000_000, 200_000_000), (300_000_000, 425_000_000)];
    for (row, (price, quantity)) in pairs.rows().zip(want) {
        assert_eq!(row[0], SlotValue::Decimal(Decimal128::new(price, 8)));
        assert_eq!(row[1], SlotValue::Decimal(Decimal128::new(quantity, 8)));
    }
    Ok(())
}

#[test]
fn empty_arrays_stage_no_rows() -> Result {
    let program = program()?;
    let mut scratch = ValidationScratch::default();
    let staged = rows(&program, br#"{"id":1,"rows":[],"pairs":[]}"#, &mut scratch)?;
    assert_eq!(staged, vec![Vec::<Vec<SlotValue>>::new(), vec![]]);
    Ok(())
}

#[test]
fn escaped_decimals_and_reused_scratch_match_a_fresh_run() -> Result {
    let program = program()?;
    let raw: &[u8] = br#"{"id":7,"rows":[],"pairs":[["1.5","2"],["3","4.25"],["0.1","0.2"]]}"#;
    let mut scratch = ValidationScratch::default();
    let first = rows(&program, raw, &mut scratch)?;
    assert_eq!(first[1].len(), 3);
    assert_eq!(
        first[1][0][0],
        SlotValue::Decimal(Decimal128::new(150_000_000, 8))
    );
    // A failed run in between must leave nothing behind for the next one.
    assert!(
        rows(
            &program,
            br#"{"id":7,"rows":[{"n":1,"s":"a"},{"n":"x","s":"b"}],"pairs":[]}"#,
            &mut scratch
        )
        .is_err()
    );
    assert_eq!(rows(&program, raw, &mut scratch)?, first);
    assert_eq!(
        rows(&program, raw, &mut ValidationScratch::default())?,
        first
    );
    Ok(())
}

#[test]
fn rows_equal_per_element_extraction() -> Result {
    let program = program()?;
    // The element schema alone, rooted at the item object: slots 0..3.
    let element = compile(Schema {
        name: "element".into(),
        schema_id: SchemaId(7_072),
        root: TypeId(0),
        definitions: vec![],
        nodes: vec![
            object(vec![
                prop(3, "n", 1, true, 0),
                prop(4, "s", 2, true, 1),
                prop(5, "f", 3, false, 2),
            ]),
            scalar(ValueKind::Integer),
            scalar(ValueKind::String),
            scalar(ValueKind::Boolean),
        ],
    })?;
    let mut scratch = ValidationScratch::default();
    let out = direct::extract_rows_borrowed_with_scratch(
        &program,
        GOOD,
        &all(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        &mut scratch,
    )?;
    let staged: Vec<Vec<String>> = out.groups[0]
        .rows()
        .map(|row| {
            row.iter()
                .map(|v| render(out.raw, out.decoded, *v))
                .collect()
        })
        .collect();
    let elements = br#"[{"n":1,"s":"a"},{"n":2,"s":"b\n","f":true}]"#;
    let mut other = ValidationScratch::default();
    let direct_rows = direct::map_root_array_borrowed(
        &element,
        elements,
        &all(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
        &mut other,
        |raw, decoded, slots| {
            slots
                .iter()
                .map(|v| render(raw, decoded, *v))
                .collect::<Vec<_>>()
        },
    )?;
    assert_eq!(staged, direct_rows);
    Ok(())
}

fn render(raw: &[u8], decoded: &[u8], value: SlotValue) -> String {
    match value {
        SlotValue::Str(_) => text(raw, decoded, value).into(),
        other => format!("{other:?}"),
    }
}

#[test]
fn nothing_is_published_before_strict_eof() -> Result {
    let program = program()?;
    let mut scratch = ValidationScratch::default();
    for raw in [
        br#"{"id":7,"rows":[{"n":1,"s":"a"}],"pairs":[]} x"#.as_slice(),
        br#"{"id":7,"rows":[{"n":1,"s":"a"}],"pairs":[["1","2"]"#,
        br#"{"id":7,"rows":[{"n":1,"s":"a"},],"pairs":[]}"#,
        br#"{"id":7,"rows":[{"n":1,"s":"a"}],"pairs":[["1","2","3"]]}"#,
        br#"{"id":7,"rows":[{"n":1}],"pairs":[]}"#,
        br#"{"id":7,"rows":[{"n":1,"s":"a","z":1}],"pairs":[]}"#,
    ] {
        assert!(rows(&program, raw, &mut scratch).is_err(), "{raw:?}");
    }
    Ok(())
}

#[test]
fn plain_extraction_refuses_programs_that_stage_item_rows() -> Result {
    let program = program()?;
    let error = direct::extract_borrowed(
        &program,
        GOOD,
        &all(),
        VmLimits::default(),
        NumericPolicy::strict_default(),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
    Ok(())
}

#[test]
fn item_fields_need_their_array_in_the_selection() -> Result {
    let program = program()?;
    let mut scratch = ValidationScratch::default();
    let selection = |fields: &[u32]| Selection {
        field_ids: fields.iter().map(|&f| FieldId(f)).collect(),
        strict_validate_unselected: true,
    };
    let run = |selection: &Selection, scratch: &mut ValidationScratch| {
        direct::extract_rows_borrowed_with_scratch(
            &program,
            GOOD,
            selection,
            VmLimits::default(),
            NumericPolicy::strict_default(),
            scratch,
        )
        .map(|out| out.groups.iter().map(|g| g.len()).collect::<Vec<_>>())
    };
    assert_eq!(
        run(&selection(&[3]), &mut scratch).unwrap_err().code,
        ErrorCode::UnsupportedSchemaFeature
    );
    // The array alone stages nothing for its items; the leaf group is separate.
    assert_eq!(run(&selection(&[2, 3]), &mut scratch)?, vec![2, 0]);
    assert_eq!(run(&selection(&[6]), &mut scratch)?, vec![0, 2]);
    Ok(())
}

#[test]
fn staged_rows_respect_the_scratch_limit() -> Result {
    let program = program()?;
    let mut limits = VmLimits::default();
    limits.max_scratch_bytes = 4_096;
    let mut body = String::from(r#"{"id":1,"rows":["#);
    for i in 0..400 {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(r#"{{"n":{i},"s":"x"}}"#));
    }
    body.push_str(r#"],"pairs":[]}"#);
    let mut scratch = ValidationScratch::default();
    let error = direct::extract_rows_borrowed_with_scratch(
        &program,
        body.as_bytes(),
        &all(),
        limits,
        NumericPolicy::strict_default(),
        &mut scratch,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::BudgetExhausted);
    Ok(())
}

#[test]
fn item_slots_must_be_contiguous_and_unshared() {
    // A gap between the item slots.
    let mut gap = item_properties();
    gap[2].slot = Some(5);
    let mut schema = rows_schema(gap);
    let TypeNode::Object(root) = &mut schema.nodes[0] else {
        unreachable!()
    };
    root.properties[2].slot = Some(4);
    assert!(compile(schema).is_err());
    // One item object under two arrays.
    let mut schema = rows_schema(item_properties());
    let TypeNode::Object(root) = &mut schema.nodes[0] else {
        unreachable!()
    };
    root.properties.push(prop(7, "more", 1, false, 6));
    assert!(compile(schema).is_err());
}

#[test]
fn warm_leaf_fast_path_stages_the_same_values_as_the_exact_engine() -> Result {
    let program = program()?;
    let plain: &[u8] = br#"{"id":1,"rows":[],"pairs":[["-1.5","0"],["12","3.25000000"],["0.00000001","99999999999.99999999"],["-0.5","7.1"],["123456789012345678901234.12","1234567.12345678"],["12345678","0.1234567"],["1234567890123456.1","7"]]}"#;
    let escaped: Vec<u8> = String::from_utf8(plain.to_vec())
        .unwrap()
        .replace('.', "\\u002e")
        .into_bytes();
    let mut scratch = ValidationScratch::default();
    // The first run warms the frame stack; the second may take the fast path.
    let cold = rows(&program, plain, &mut scratch)?;
    let warm = rows(&program, plain, &mut scratch)?;
    let exact = rows(&program, &escaped, &mut scratch)?;
    assert_eq!(cold[1].len(), 7);
    assert_eq!(cold, warm);
    assert_eq!(warm, exact);
    assert_eq!(
        warm[1][0][0],
        SlotValue::Decimal(Decimal128::new(-150_000_000, 8))
    );
    assert_eq!(
        warm[1][2][1],
        SlotValue::Decimal(Decimal128::new(9_999_999_999_999_999_999, 8))
    );
    // A bad decimal after good ones leaves no staged values behind.
    let bad: &[u8] = br#"{"id":1,"rows":[],"pairs":[["1.5","2"],["01","2"]]}"#;
    assert!(rows(&program, bad, &mut scratch).is_err());
    assert_eq!(rows(&program, plain, &mut scratch)?, warm);
    Ok(())
}
