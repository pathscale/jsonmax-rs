use jmax_endpoint_model::native::{schema_graph_for, NativeSpec};
use jmax_endpoint_model::registry::build_registry;
use jmax_endpoint_model::{EndpointField, EndpointModel, EndpointType, SchemaId, TypeDefinition};
use jmax_schema_ir::{TypeNode, ValueKind};

fn field(name: &str, ty: EndpointType) -> EndpointField {
    EndpointField { name: name.into(), ty, description: std::string::String::new() }
}

fn strukt(name: &str, fields: Vec<EndpointField>) -> TypeDefinition {
    TypeDefinition { name: name.into(), ty: EndpointType::Struct { name: name.into(), fields } }
}

fn model(definitions: Vec<TypeDefinition>) -> EndpointModel {
    EndpointModel { definitions, ..EndpointModel::default() }
}

#[test]
fn unit_lowers_to_null_at_root_nested_optional_and_array_positions() {
    use EndpointType::*;
    let model = model(vec![
        strukt("Empty", vec![]),
        strukt("Root", vec![
            field("required", Unit),
            field("optional", Optional(Box::new(Unit))),
            field("nested", StructRef("Empty".into())),
            field("items", Vec(Box::new(Unit))),
        ]),
    ]);
    let registry = build_registry(&model).unwrap();
    let spec = NativeSpec { roots: vec!["Root".into(), "Empty".into()], ..NativeSpec::default() };

    let schema = schema_graph_for(&registry, &spec, "Root", SchemaId(70)).unwrap();
    let TypeNode::Object(root) = &schema.nodes[0] else { panic!("root must be an object") };
    assert!(root.properties[0].required);
    assert!(!root.properties[1].required);
    assert!(matches!(
        &schema.nodes[root.properties[0].ty.0 as usize],
        TypeNode::Scalar(c) if c.kind == ValueKind::Null
    ));
    assert!(matches!(
        &schema.nodes[root.properties[1].ty.0 as usize],
        TypeNode::Scalar(c) if c.kind == ValueKind::Null
    ));
    let TypeNode::Object(empty) = &schema.nodes[root.properties[2].ty.0 as usize] else {
        panic!("nested empty definition must be an object")
    };
    assert!(empty.properties.is_empty());
    let TypeNode::Array(items) = &schema.nodes[root.properties[3].ty.0 as usize] else {
        panic!("unit vector must be an array")
    };
    assert!(matches!(
        &schema.nodes[items.items.0 as usize],
        TypeNode::Scalar(c) if c.kind == ValueKind::Null
    ));

    let empty_schema = schema_graph_for(&registry, &spec, "Empty", SchemaId(71)).unwrap();
    let TypeNode::Object(empty_root) = &empty_schema.nodes[0] else {
        panic!("zero-field root must remain an object")
    };
    assert!(empty_root.properties.is_empty());
}

#[test]
fn enum_names_and_discriminants_survive_lowering_and_enum_references() {
    use EndpointType::*;
    use jmax_endpoint_model::EnumVariantSpec;
    let variants = vec![
        EnumVariantSpec { name: "Queued".into(), value: -3, description: std::string::String::new() },
        EnumVariantSpec { name: "Settled".into(), value: 8, description: std::string::String::new() },
    ];
    let model = model(vec![
        TypeDefinition {
            name: "Status".into(),
            ty: Enum { name: "Status".into(), variants: variants.clone() },
        },
        strukt("Root", vec![
            field("direct", Enum { name: "InlineStatus".into(), variants: variants.clone() }),
            field("referenced", EnumRef { name: "Status".into(), prefixed_name: false }),
            field("optional", Optional(Box::new(EnumRef {
                name: "Status".into(), prefixed_name: false,
            }))),
        ]),
    ]);
    let registry = build_registry(&model).unwrap();
    let spec = NativeSpec { roots: vec!["Root".into()], ..NativeSpec::default() };
    let schema = schema_graph_for(&registry, &spec, "Root", SchemaId(72)).unwrap();
    let TypeNode::Object(root) = &schema.nodes[0] else { panic!("root must be an object") };
    for (index, required) in [(0, true), (1, true), (2, false)] {
        assert_eq!(root.properties[index].required, required);
        let TypeNode::Enum(shape) = &schema.nodes[root.properties[index].ty.0 as usize] else {
            panic!("enum field must lower to an enum node")
        };
        assert!(!shape.nullable);
        assert_eq!(shape.variants.iter().map(|v| (v.name.as_str(), v.discriminant)).collect::<std::vec::Vec<_>>(), [
            ("Queued", -3),
            ("Settled", 8),
        ]);
    }
}

#[test]
fn root_enum_and_nullable_optional_enum_lowering() {
    use EndpointType::*;
    let model = model(vec![
        TypeDefinition { name: "State".into(), ty: Enum { name: "State".into(), variants: vec![
            jmax_endpoint_model::EnumVariantSpec { name: "Ready".into(), value: 7, description: Default::default() },
        ] } },
        strukt("Root", vec![field("state", Optional(Box::new(EnumRef { name: "State".into(), prefixed_name: false })))]),
    ]);
    let registry = build_registry(&model).unwrap();
    let spec = NativeSpec { roots: vec!["State".into(), "Root".into()], nullable_optionals: true, ..NativeSpec::default() };
    assert!(matches!(schema_graph_for(&registry, &spec, "State", SchemaId(73)).unwrap().nodes[0], TypeNode::Enum(_)));
    let schema = schema_graph_for(&registry, &spec, "Root", SchemaId(74)).unwrap();
    let TypeNode::Object(root) = &schema.nodes[0] else { panic!("object root") };
    assert!(matches!(&schema.nodes[root.properties[0].ty.0 as usize], TypeNode::Enum(e) if e.nullable));
}
