#![allow(non_snake_case)]
#[global_allocator] static GLOBAL: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;

use jmax_schema_ir::{
    ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind,
};
use jsonmax::{
    CompileLimits, FieldId, ParseOptions, Payload, Selection, SlotValue, ValidatedSlots,
};
use serde_derive::{Deserialize, Serialize};
use std::{
    fs,
    hint::black_box,
    io::Write,
    time::Instant,
};
type R<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct Twitter {
    search_metadata: SearchMetadata,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct SearchMetadata {
    query: String,
    count: i64,
    max_id: i64,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct Citm {
    #[serde(rename = "venueNames")]
    venueNames: Venues,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct Venues {
    PLEYEL_PLEYEL: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct Canada {
    #[serde(rename = "type")]
    r#type: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct Github {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    public: bool,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct Apache {
    mode: String,
    nodeName: String,
    numExecutors: i64,
    quietingDown: bool,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize,
)]
struct TypedPayload {
    batch_id: String,
    tenant: String,
    schema_version: i64,
    request_count: i64,
    accepted: bool,
}

fn options() -> ParseOptions {
    let mut o = ParseOptions::default();
    o.limits.reject_unknown_keys = false;
    if std::env::var_os("NODUP").is_some() { o.limits.reject_duplicate_keys = false; }
    o
}
fn string(slots: &ValidatedSlots, n: usize) -> String {
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
fn integer(s: &ValidatedSlots, n: usize) -> i64 {
    match s.slots[n] {
        SlotValue::I64(x) => x,
        SlotValue::U64(x) => i64::try_from(x).unwrap(),
        _ => panic!("wrong int slot"),
    }
}
fn boolean(s: &ValidatedSlots, n: usize) -> bool {
    match s.slots[n] {
        SlotValue::Bool(x) => x,
        _ => panic!("wrong bool slot"),
    }
}
struct Plan {
    schema: jsonmax::CompiledSchema,
    selection: Selection,
}
fn make_plan(name: &str, fields: &[(&[&str], ValueKind)]) -> R<Plan> {
    let mut nodes = vec![TypeNode::Object(ObjectShape {
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
                TypeNode::Object(ObjectShape {
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
fn plan(name: &str) -> R<Plan> {
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
fn vm(
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
fn extract(b: &[u8], p: &Plan) -> R<ValidatedSlots> {
    Ok(jsonmax::endpoint::extract_payload(
        Payload::from_slice(b),
        &p.schema,
        &p.selection,
        options(),
    )?)
}
fn jmax_twitter(b: &[u8], p: &Plan) -> R<Twitter> {
    let s = extract(b, p)?;
    Ok(Twitter {
        search_metadata: SearchMetadata {
            query: string(&s, 0),
            count: integer(&s, 1),
            max_id: integer(&s, 2),
        },
    })
}
fn jmax_citm(b: &[u8], p: &Plan) -> R<Citm> {
    let s = extract(b, p)?;
    Ok(Citm {
        venueNames: Venues {
            PLEYEL_PLEYEL: string(&s, 0),
        },
    })
}
fn jmax_canada(b: &[u8], p: &Plan) -> R<Canada> {
    let s = extract(b, p)?;
    Ok(Canada {
        r#type: string(&s, 0),
    })
}
fn jmax_apache(b: &[u8], p: &Plan) -> R<Apache> {
    let s = extract(b, p)?;
    Ok(Apache {
        mode: string(&s, 0),
        nodeName: string(&s, 1),
        numExecutors: integer(&s, 2),
        quietingDown: boolean(&s, 3),
    })
}
fn jmax_endpoint(b: &[u8], p: &Plan) -> R<TypedPayload> {
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
fn jmax_github(b: &[u8], p: &Plan) -> R<Vec<Github>> {
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
fn walk(b: &[u8]) -> R<usize> {
    jsonmax::parse::validate_syntax(b, options())?;
    Ok(b.len())
}

struct Job<'a> {
    name: String,
    run: Box<dyn FnMut() -> R<()> + 'a>,
    reps: usize,
    samples: Vec<f64>,
}
fn add<'a, T: 'a>(jobs: &mut Vec<Job<'a>>, name: String, mut f: impl FnMut() -> R<T> + 'a) {
    jobs.push(Job { name, run: Box::new(move || { black_box(f()?); Ok(()) }), reps: 1, samples: vec![] });
}
fn typed<'a, T: PartialEq + std::fmt::Debug + serde::de::DeserializeOwned + 'a>(
    jobs: &mut Vec<Job<'a>>, name: &str, b: &'a [u8], p: &'a Plan, f: fn(&[u8], &Plan) -> R<T>, serde: bool,
) -> R<()> {
    let expected: T = serde_json::from_slice(b)?;
    assert_eq!(expected, f(b, p)?);
    add(jobs, format!("{name}\ttyped"), move || f(black_box(b), p));
    if serde {
        add(jobs, format!("{name}\tserde-typed"), move || Ok(serde_json::from_slice::<T>(black_box(b))?));
    }
    Ok(())
}

fn main() -> R<()> {
    if std::env::var_os("DIFF").is_some() { return diff::run(); }
    let dir = std::env::var("JMAX_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    let samples: usize = std::env::var("SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    let filter = std::env::var("FILTER").ok();
    let serde = std::env::var_os("SERDE").is_some();
    let target_ns: f64 = std::env::var("TARGET_NS").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000_000.0);
    let all = ["twitter", "citm_catalog", "canada", "github_events", "apache_builds", "endpoint_batch"];
    let corpora = std::env::var("CORPORA").ok();
    let names: Vec<&str> = all.iter().copied().filter(|n| corpora.as_ref().map_or(true, |c| c.split(',').any(|x| x == *n))).collect();
    let mut files = vec![];
    for name in &names { files.push(fs::read(format!("{dir}/{name}.json"))?); }
    let mut docs = vec![];
    let mut values = vec![];
    let mut plans = vec![];
    for (i, name) in names.iter().enumerate() {
        let b = files[i].as_slice();
        let doc = jsonmax::parse::parse_slice(b, options())?;
        let reference: serde_json::Value = serde_json::from_slice(b)?;
        let encoded = jsonmax::encode::document_to_vec(&doc, Default::default())?;
        assert_eq!(reference, serde_json::from_slice::<serde_json::Value>(&encoded)?, "round trip {name}");
        let mut bad = b.to_vec();
        bad.extend_from_slice(b" false");
        assert!(walk(&bad).is_err());
        assert!(jsonmax::parse::parse_borrowed(&bad, options()).is_err());
        docs.push(doc);
        values.push(reference);
        plans.push(plan(name)?);
    }
    let small = small_store(&dir)?;
    let mut anys = vec![];
    let mut queries = vec![];
    for name in &names {
        anys.push(jsonmax::endpoint::compile_native_schema(&Schema { name: "any".into(), schema_id: jsonmax::SchemaId(2), root: TypeId(0), nodes: vec![TypeNode::Any], definitions: vec![] }, CompileLimits::default_limits())?);
        let path = match *name { "twitter" => "search_metadata.query", "citm_catalog" => "venueNames.PLEYEL_PLEYEL", "canada" => "type", "github_events" => "[0].id", "apache_builds" => "mode", _ => "batch_id" };
        queries.push(jsonmax::query::compile_paths(&[path])?);
    }
    let mut jobs: Vec<Job> = vec![];
    for (i, name) in names.iter().enumerate() {
        let b = files[i].as_slice();
        let doc = &docs[i];
        let val = &values[i];
        let p = &plans[i];
        add(&mut jobs, format!("{name}\tscan"), move || walk(black_box(b)));
        let any = &anys[i];
        add(&mut jobs, format!("{name}\tvm-any"), move || Ok(jsonmax::endpoint::validate_payload(Payload::from_slice(black_box(b)), any, options())?));
        add(&mut jobs, format!("{name}\tquery"), {
            let q = &queries[i];
            move || Ok(jsonmax::query::execute_borrowed(q, black_box(b), jsonmax::QueryOptions { mode: jsonmax::QueryMode::Query, ..Default::default() })?)
        });
        add(&mut jobs, format!("{name}\tdom-borrowed"), move || Ok(jsonmax::parse::parse_borrowed(black_box(b), options())?));
        add(&mut jobs, format!("{name}\tdom-owned"), move || Ok(jsonmax::parse::parse_slice(black_box(b), options())?));
        match *name {
            "twitter" => typed(&mut jobs, name, b, p, jmax_twitter, serde)?,
            "citm_catalog" => typed(&mut jobs, name, b, p, jmax_citm, serde)?,
            "canada" => typed(&mut jobs, name, b, p, jmax_canada, serde)?,
            "github_events" => typed(&mut jobs, name, b, p, jmax_github, serde)?,
            "apache_builds" => typed(&mut jobs, name, b, p, jmax_apache, serde)?,
            _ => typed(&mut jobs, name, b, p, jmax_endpoint, serde)?,
        }
        add(&mut jobs, format!("{name}\tserialize"), move || {
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_document_streaming(black_box(doc), &mut sink, Default::default())?;
            Ok(sink.into_vec())
        });
        if serde {
            add(&mut jobs, format!("{name}\tserde-scan"), move || Ok(serde_json::from_slice::<serde::de::IgnoredAny>(black_box(b))?));
            add(&mut jobs, format!("{name}\tserde-dom"), move || Ok(serde_json::from_slice::<serde_json::Value>(black_box(b))?));
            add(&mut jobs, format!("{name}\tserde-serialize"), move || Ok(serde_json::to_vec(black_box(val))?));
        }
    }
    if corpora.as_ref().map_or(true, |c| c.contains("small")) { small_jobs(&mut jobs, &dir, &small)?; }
    if let Some(f) = &filter {
        let pats: Vec<&str> = f.split(',').collect();
        jobs.retain(|j| pats.iter().any(|p| j.name.contains(p)));
    }
    for job in &mut jobs {
        (job.run)()?;
        let start = Instant::now();
        let mut count = 0;
        let fixed = std::env::var_os("FIXED").is_some();
        while count < 3 || (!fixed && start.elapsed().as_millis() < 20) { (job.run)()?; count += 1; }
        let ns = start.elapsed().as_nanos() as f64 / count as f64;
        job.reps = (target_ns / ns).ceil().clamp(1., 65536.) as usize;
    }
    let mut state = 0x6a09e667f3bcc909u64;
    for _ in 0..samples {
        let mut order: Vec<_> = (0..jobs.len()).collect();
        for i in (1..order.len()).rev() {
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            order.swap(i, state as usize % (i + 1));
        }
        for i in order {
            let job = &mut jobs[i];
            let start = Instant::now();
            for _ in 0..job.reps { (job.run)()?; }
            job.samples.push(start.elapsed().as_nanos() as f64 / job.reps as f64);
        }
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    for job in &mut jobs {
        job.samples.sort_by(f64::total_cmp);
        let med = job.samples[(job.samples.len() - 1) / 2];
        writeln!(out, "{}\t{:.1}", job.name, med)?;
    }
    Ok(())
}
mod diff;

fn small_jobs<'a>(jobs: &mut Vec<Job<'a>>, dir: &str, store: &'a [(String, Vec<u8>, Plan)]) -> R<()> {
    let _ = dir;
    for (name, b, p) in store {
        let b = b.as_slice();
        add(jobs, format!("{name}\tscan"), move || walk(black_box(b)));
        add(jobs, format!("{name}\tdom-borrowed"), move || Ok(jsonmax::parse::parse_borrowed(black_box(b), options())?.node_count()));
        add(jobs, format!("{name}\tserialize"), move || {
            let doc = jsonmax::parse::parse_borrowed(black_box(b), options())?;
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_borrowed_document_streaming(&doc, &mut sink, Default::default())?;
            Ok(sink.into_vec())
        });
        add(jobs, format!("{name}\ttyped"), move || {
            let s = jsonmax::endpoint::extract_borrowed(&p.schema, &p.selection, black_box(b), options())?;
            Ok(s.slots.len())
        });
    }
    Ok(())
}
fn small_store(dir: &str) -> R<Vec<(String, Vec<u8>, Plan)>> {
    use ValueKind::*;
    Ok(vec![
        ("rpc_small".into(), fs::read(format!("{dir}/rpc_small.json"))?, make_plan("rpc", &[(&["method"], String), (&["seq"], Integer), (&["params", "account_id"], String)])?),
        ("binance_small".into(), fs::read(format!("{dir}/binance_small.json"))?, make_plan("bn", &[(&["e"], String), (&["E"], Integer), (&["p"], String), (&["m"], Boolean)])?),
    ])
}
