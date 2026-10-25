// Shared by the jmax-harness binary and the cross-parser comparison bench.
#![allow(dead_code, non_snake_case)]
pub use jmax_schema_ir::{
    ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind,
};
pub use jsonmax::{
    CompileLimits, FieldId, ParseOptions, Payload, Selection, SlotValue, ValidatedSlots,
};
pub use serde_derive::{Deserialize, Serialize};
pub type R<T> = Result<T, Box<dyn std::error::Error>>;

#[path = "native.rs"]
pub mod native;

#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Twitter {
    pub(crate) search_metadata: SearchMetadata,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct SearchMetadata {
    pub(crate) query: String,
    pub(crate) count: i64,
    pub(crate) max_id: i64,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Citm {
    #[serde(rename = "venueNames")]
    pub(crate) venueNames: Venues,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Venues {
    pub(crate) PLEYEL_PLEYEL: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Canada {
    #[serde(rename = "type")]
    pub(crate) r#type: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Github {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) r#type: String,
    pub(crate) public: bool,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Apache {
    pub(crate) mode: String,
    pub(crate) nodeName: String,
    pub(crate) numExecutors: i64,
    pub(crate) quietingDown: bool,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct TypedPayload {
    pub(crate) batch_id: String,
    pub(crate) tenant: String,
    pub(crate) schema_version: i64,
    pub(crate) request_count: i64,
    pub(crate) accepted: bool,
}

pub fn options() -> ParseOptions {
    static OPTIONS: std::sync::OnceLock<ParseOptions> = std::sync::OnceLock::new();
    *OPTIONS.get_or_init(|| {
        let mut o = ParseOptions::default();
        o.limits.reject_unknown_keys = false;
        if std::env::var_os("NODUP").is_some() {
            o.limits.reject_duplicate_keys = false;
        }
        o
    })
}
pub fn string(slots: &ValidatedSlots, n: usize) -> String {
    let SlotValue::Str(s) = slots.slots[n] else {
        panic!("wrong string slot")
    };
    let b = match s.source {
        jmax_abi::span::SpanSource::RawPayload => slots.payload.as_slice(),
        jmax_abi::span::SpanSource::DecodedArena => &slots.decoded,
    };
    std::str::from_utf8(s.span.slice(b).unwrap())
        .unwrap()
        .to_owned()
}
pub fn integer(s: &ValidatedSlots, n: usize) -> i64 {
    match s.slots[n] {
        SlotValue::I64(x) => x,
        SlotValue::U64(x) => i64::try_from(x).unwrap(),
        _ => panic!("wrong int slot"),
    }
}
pub fn boolean(s: &ValidatedSlots, n: usize) -> bool {
    match s.slots[n] {
        SlotValue::Bool(x) => x,
        _ => panic!("wrong bool slot"),
    }
}
pub struct Plan {
    pub schema: jsonmax::CompiledSchema,
    pub selection: Selection,
}
pub fn make_plan(name: &str, fields: &[(&[&str], ValueKind)]) -> R<Plan> {
    let mut nodes = vec![TypeNode::Object(ObjectShape { nullable: false,
        properties: vec![],
        reject_unknown: false,
        min_properties: None,
        max_properties: None,
    })];
    for (slot, (path, kind)) in fields.iter().enumerate() {
        let mut parent = 0usize;
        for (depth, key) in path.iter().enumerate() {
            let last = depth + 1 == path.len();
            let existing = match &nodes[parent] {
                TypeNode::Object(o) => o
                    .properties
                    .iter()
                    .find(|p| p.name == *key)
                    .map(|p| p.ty.0 as usize),
                _ => None,
            };
            if let Some(id) = existing {
                parent = id;
                continue;
            }
            let id = nodes.len();
            nodes.push(if last {
                TypeNode::Scalar(ScalarConstraints::new(*kind))
            } else {
                TypeNode::Object(ObjectShape { nullable: false,
                    properties: vec![],
                    reject_unknown: false,
                    min_properties: None,
                    max_properties: None,
                })
            });
            if let TypeNode::Object(o) = &mut nodes[parent] {
                o.properties.push(Property {
                    field_id: FieldId(if last {
                        slot as u32 + 1
                    } else {
                        1000 + id as u32
                    }),
                    name: (*key).into(),
                    ty: TypeId(id as u32),
                    required: true,
                    slot: if last { Some(slot as u32) } else { None },
                });
            }
            parent = id;
        }
    }
    let schema = jsonmax::endpoint::compile_native_schema(
        &Schema {
            name: name.into(),
            schema_id: jsonmax::SchemaId(1),
            root: TypeId(0),
            nodes,
            definitions: vec![],
        },
        CompileLimits::default_limits(),
    )?;
    Ok(Plan {
        schema,
        selection: Selection {
            field_ids: (1..=fields.len() as u32).map(FieldId).collect(),
            strict_validate_unselected: true,
        },
    })
}
pub fn plan(name: &str) -> R<Plan> {
    use ValueKind::*;
    match name {
        "twitter" => make_plan(
            name,
            &[
                (&["search_metadata", "query"], String),
                (&["search_metadata", "count"], Integer),
                (&["search_metadata", "max_id"], Integer),
            ],
        ),
        "citm_catalog" => make_plan(name, &[(&["venueNames", "PLEYEL_PLEYEL"], String)]),
        "canada" => make_plan(name, &[(&["type"], String)]),
        "github_events" => make_plan(
            name,
            &[
                (&["id"], String),
                (&["type"], String),
                (&["public"], Boolean),
            ],
        ),
        "apache_builds" => make_plan(
            name,
            &[
                (&["mode"], String),
                (&["nodeName"], String),
                (&["numExecutors"], Integer),
                (&["quietingDown"], Boolean),
            ],
        ),
        _ => make_plan(
            name,
            &[
                (&["batch_id"], String),
                (&["tenant"], String),
                (&["schema_version"], Integer),
                (&["request_count"], Integer),
                (&["accepted"], Boolean),
            ],
        ),
    }
}
pub fn vm(
    b: &[u8],
    schema: &jsonmax::CompiledSchema,
    _selection: &Selection,
) -> R<jsonmax::ValidationProof> {
    Ok(jsonmax::endpoint::validate_payload(
        Payload::from_slice(b),
        schema,
        options(),
    )?)
}
pub fn extract(b: &[u8], p: &Plan) -> R<ValidatedSlots> {
    Ok(jsonmax::endpoint::extract_payload(
        Payload::from_slice(b),
        &p.schema,
        &p.selection,
        options(),
    )?)
}
pub fn jmax_twitter(b: &[u8], p: &Plan) -> R<Twitter> {
    jmax_twitter_with_scratch(b, p, &mut jsonmax::endpoint::ValidationScratch::default())
}
pub fn jmax_twitter_with_scratch(b: &[u8], p: &Plan, scratch: &mut jsonmax::endpoint::ValidationScratch) -> R<Twitter> {
    let s = jsonmax::endpoint::extract_borrowed_with_scratch(&p.schema, &p.selection, b, options(), scratch)?;
    Ok(Twitter {
        search_metadata: SearchMetadata {
            query: borrowed_string(s.slots, s.raw, s.decoded, 0),
            count: borrowed_integer(s.slots, 1),
            max_id: borrowed_integer(s.slots, 2),
        },
    })
}
pub fn jmax_citm(b: &[u8], p: &Plan) -> R<Citm> {
    jmax_citm_with_scratch(b, p, &mut jsonmax::endpoint::ValidationScratch::default())
}
pub fn jmax_citm_with_scratch(b: &[u8], p: &Plan, scratch: &mut jsonmax::endpoint::ValidationScratch) -> R<Citm> {
    let s = jsonmax::endpoint::extract_borrowed_with_scratch(&p.schema, &p.selection, b, options(), scratch)?;
    Ok(Citm {
        venueNames: Venues {
            PLEYEL_PLEYEL: borrowed_string(s.slots, s.raw, s.decoded, 0),
        },
    })
}
pub fn jmax_canada(b: &[u8], p: &Plan) -> R<Canada> {
    jmax_canada_with_scratch(b, p, &mut jsonmax::endpoint::ValidationScratch::default())
}
pub fn jmax_canada_with_scratch(b: &[u8], p: &Plan, scratch: &mut jsonmax::endpoint::ValidationScratch) -> R<Canada> {
    let s = jsonmax::endpoint::extract_borrowed_with_scratch(&p.schema, &p.selection, b, options(), scratch)?;
    Ok(Canada {
        r#type: borrowed_string(s.slots, s.raw, s.decoded, 0),
    })
}
pub fn jmax_apache(b: &[u8], p: &Plan) -> R<Apache> {
    jmax_apache_with_scratch(b, p, &mut jsonmax::endpoint::ValidationScratch::default())
}
pub fn jmax_apache_with_scratch(b: &[u8], p: &Plan, scratch: &mut jsonmax::endpoint::ValidationScratch) -> R<Apache> {
    let s = jsonmax::endpoint::extract_borrowed_with_scratch(&p.schema, &p.selection, b, options(), scratch)?;
    Ok(Apache {
        mode: borrowed_string(s.slots, s.raw, s.decoded, 0),
        nodeName: borrowed_string(s.slots, s.raw, s.decoded, 1),
        numExecutors: borrowed_integer(s.slots, 2),
        quietingDown: borrowed_boolean(s.slots, 3),
    })
}
pub fn jmax_endpoint(b: &[u8], p: &Plan) -> R<TypedPayload> {
    jmax_endpoint_with_scratch(b, p, &mut jsonmax::endpoint::ValidationScratch::default())
}
pub fn jmax_endpoint_with_scratch(b: &[u8], p: &Plan, scratch: &mut jsonmax::endpoint::ValidationScratch) -> R<TypedPayload> {
    let s = jsonmax::endpoint::extract_borrowed_with_scratch(&p.schema, &p.selection, b, options(), scratch)?;
    Ok(TypedPayload {
        batch_id: borrowed_string(s.slots, s.raw, s.decoded, 0),
        tenant: borrowed_string(s.slots, s.raw, s.decoded, 1),
        schema_version: borrowed_integer(s.slots, 2),
        request_count: borrowed_integer(s.slots, 3),
        accepted: borrowed_boolean(s.slots, 4),
    })
}
// Validate the whole array first, then map its typed rows with retained VM
// scratch so syntax errors keep precedence over row schema errors.
pub fn jmax_github(b: &[u8], p: &Plan) -> R<Vec<Github>> {
    jmax_github_with_scratch(b, p, &mut jsonmax::endpoint::ValidationScratch::default())
}
pub fn jmax_github_with_scratch(
    b: &[u8],
    p: &Plan,
    scratch: &mut jsonmax::endpoint::ValidationScratch,
) -> R<Vec<Github>> {
    let options = options();
    if !b
        .iter()
        .copied()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .next()
        .is_some_and(|byte| byte == b'[')
    {
        let doc = jsonmax::parse::parse_borrowed(b, options)?;
        let _ = doc.root_node().array()?;
        unreachable!("a non-array root returned from BorrowedNode::array")
    }
    Ok(jmax_schema_vm::direct::map_root_array_borrowed(
        p.schema.program(),
        b,
        &p.selection,
        jmax_schema_vm::VmLimits {
            syntax: options.limits,
            max_steps: jmax_schema_vm::DEFAULT_MAX_VM_STEPS,
            max_scratch_bytes: 16 * 1024 * 1024,
        },
        options.numeric,
        scratch,
        |raw, decoded, slots| Github {
            id: borrowed_string(slots, raw, decoded, 0),
            r#type: borrowed_string(slots, raw, decoded, 1),
            public: borrowed_boolean(slots, 2),
        },
    )?)
}
fn borrowed_string(slots: &[SlotValue], raw: &[u8], decoded: &[u8], n: usize) -> String {
    let SlotValue::Str(s) = slots[n] else { panic!("wrong string slot") };
    let b = match s.source {
        jmax_abi::span::SpanSource::RawPayload => raw,
        jmax_abi::span::SpanSource::DecodedArena => decoded,
    };
    std::str::from_utf8(s.span.slice(b).unwrap()).unwrap().to_owned()
}
fn borrowed_integer(slots: &[SlotValue], n: usize) -> i64 {
    match slots[n] {
        SlotValue::I64(x) => x,
        SlotValue::U64(x) => i64::try_from(x).unwrap(),
        _ => panic!("wrong int slot"),
    }
}
fn borrowed_boolean(slots: &[SlotValue], n: usize) -> bool {
    match slots[n] { SlotValue::Bool(x) => x, _ => panic!("wrong bool slot") }
}
pub fn walk(b: &[u8]) -> R<usize> {
    jsonmax::parse::validate_syntax(b, options())?;
    Ok(b.len())
}
