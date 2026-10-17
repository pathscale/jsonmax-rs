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

#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Twitter {
    search_metadata: SearchMetadata,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct SearchMetadata {
    query: String,
    count: i64,
    max_id: i64,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Citm {
    #[serde(rename = "venueNames")]
    venueNames: Venues,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Venues {
    PLEYEL_PLEYEL: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Canada {
    #[serde(rename = "type")]
    r#type: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Github {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    public: bool,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct Apache {
    mode: String,
    nodeName: String,
    numExecutors: i64,
    quietingDown: bool,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
pub struct TypedPayload {
    batch_id: String,
    tenant: String,
    schema_version: i64,
    request_count: i64,
    accepted: bool,
}

pub fn options() -> ParseOptions {
    let mut o = ParseOptions::default();
    o.limits.reject_unknown_keys = false;
    if std::env::var_os("NODUP").is_some() { o.limits.reject_duplicate_keys = false; }
    o
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
    let s = extract(b, p)?;
    Ok(Twitter {
        search_metadata: SearchMetadata {
            query: string(&s, 0),
            count: integer(&s, 1),
            max_id: integer(&s, 2),
        },
    })
}
pub fn jmax_citm(b: &[u8], p: &Plan) -> R<Citm> {
    let s = extract(b, p)?;
    Ok(Citm {
        venueNames: Venues {
            PLEYEL_PLEYEL: string(&s, 0),
        },
    })
}
pub fn jmax_canada(b: &[u8], p: &Plan) -> R<Canada> {
    let s = extract(b, p)?;
    Ok(Canada {
        r#type: string(&s, 0),
    })
}
pub fn jmax_apache(b: &[u8], p: &Plan) -> R<Apache> {
    let s = extract(b, p)?;
    Ok(Apache {
        mode: string(&s, 0),
        nodeName: string(&s, 1),
        numExecutors: integer(&s, 2),
        quietingDown: boolean(&s, 3),
    })
}
pub fn jmax_endpoint(b: &[u8], p: &Plan) -> R<TypedPayload> {
    let s = extract(b, p)?;
    Ok(TypedPayload {
        batch_id: string(&s, 0),
        tenant: string(&s, 1),
        schema_version: integer(&s, 2),
        request_count: integer(&s, 3),
        accepted: boolean(&s, 4),
    })
}
// The VM exposes fixed slots rather than a repeated-row collection. Split the
// root array using a complete strict cursor, then extract each object with the
// precompiled event plan. Both passes and all owned Strings are timed.
pub fn jmax_github(b: &[u8], p: &Plan) -> R<Vec<Github>> {
    let doc = jsonmax::parse::parse_borrowed(b, options())?;
    let mut rows = doc.root_node().array()?.iter();
    let mut out = Vec::new();
    while let Some(row) = rows.next() {
        let s = extract(row.raw()?, p)?;
        out.push(Github {
            id: string(&s, 0),
            r#type: string(&s, 1),
            public: boolean(&s, 2),
        });
    }
    Ok(out)
}
pub fn walk(b: &[u8]) -> R<usize> {
    jsonmax::parse::validate_syntax(b, options())?;
    Ok(b.len())
}
