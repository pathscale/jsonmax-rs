#![allow(non_snake_case)]
use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};

struct CountingAllocator;
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static DEALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ZEROED_CALLS: AtomicU64 = AtomicU64::new(0);
static REALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(not(feature = "system-alloc"))]
use rusty_alloc::RustyAlloc as AllocBackend;
#[cfg(feature = "system-alloc")]
use std::alloc::System as AllocBackend;
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { AllocBackend.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { AllocBackend.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ZEROED_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { AllocBackend.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { AllocBackend.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

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
    io::{BufWriter, Write},
    time::{Duration, Instant},
};
type R<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct Twitter {
    search_metadata: SearchMetadata,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct SearchMetadata {
    query: String,
    count: i64,
    max_id: i64,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct Citm {
    #[serde(rename = "venueNames")]
    venueNames: Venues,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct Venues {
    PLEYEL_PLEYEL: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct Canada {
    #[serde(rename = "type")]
    r#type: String,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct Github {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    public: bool,
}
#[allow(non_snake_case)]
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
)]
struct Apache {
    mode: String,
    nodeName: String,
    numExecutors: i64,
    quietingDown: bool,
}
#[derive(
    Debug, PartialEq, Serialize, Deserialize, miniserde::Serialize, miniserde::Deserialize,
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
    o
}
fn query_path(name: &str) -> &'static str {
    match name {
        "twitter" => "search_metadata.query",
        "citm_catalog" => "venueNames.PLEYEL_PLEYEL",
        "canada" => "type",
        "github_events" => "[0].id",
        "apache_builds" => "mode",
        _ => "batch_id",
    }
}
fn selected_reference<'a>(
    name: &str,
    value: &'a serde_json::Value,
) -> Option<&'a serde_json::Value> {
    match name {
        "twitter" => value.get("search_metadata")?.get("query"),
        "citm_catalog" => value.get("venueNames")?.get("PLEYEL_PLEYEL"),
        "canada" => value.get("type"),
        "apache_builds" => value.get("mode"),
        "endpoint_batch" => value.get("batch_id"),
        _ => None,
    }
}
fn query_options() -> jsonmax::QueryOptions {
    jsonmax::QueryOptions {
        mode: jsonmax::QueryMode::Query,
        ..Default::default()
    }
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
fn convert_numbers(doc: &jsonmax::Document) -> R<usize> {
    let mut n = 0;
    for node in doc.nodes() {
        if node.kind() == jmax_dom::NodeKind::Number {
            let raw = node.raw()?;
            let text = std::str::from_utf8(raw)?;
            if text.contains(['.', 'e', 'E']) {
                black_box(text.parse::<f64>()?);
            } else if text.starts_with('-') {
                black_box(text.parse::<i64>()?);
            } else {
                black_box(text.parse::<u64>()?);
            }
            n += 1;
        }
    }
    Ok(n)
}
struct Job<'a> {
    category: &'static str,
    name: &'static str,
    bytes: usize,
    run: Box<dyn FnMut() -> R<()> + 'a>,
    samples: Vec<f64>,
    reps: usize,
    allocs_op: f64,
    bytes_op: f64,
}
fn add<'a, F, T>(
    jobs: &mut Vec<Job<'a>>,
    category: &'static str,
    name: &'static str,
    bytes: usize,
    mut f: F,
) where
    F: FnMut() -> R<T> + 'a,
    T: 'a,
{
    jobs.push(Job {
        category,
        name,
        bytes,
        run: Box::new(move || {
            black_box(f()?);
            Ok(())
        }),
        samples: vec![],
        reps: 1,
        allocs_op: 0.,
        bytes_op: 0.,
    });
}
macro_rules! typed_jobs {
    ($jobs:expr,$b:expr,$text:expr,$p:expr,$ty:ty,$jf:ident) => {{
        let expected: $ty = serde_json::from_slice($b)?;
        assert_eq!(expected, $jf($b, $p)?);
        let mut scratch = $b.to_vec();
        assert_eq!(expected, simd_json::serde::from_slice::<$ty>(&mut scratch)?);
        assert_eq!(expected, miniserde::json::from_str::<$ty>($text)?);
        assert_eq!(expected, sonic_rs::from_slice::<$ty>($b)?);
        add($jobs, "C-typed", "jmax-vm-owned-extract", $b.len(), || {
            $jf(black_box($b), $p)
        });
        add($jobs, "C-typed", "serde-json-typed", $b.len(), || {
            Ok(serde_json::from_slice::<$ty>(black_box($b))?)
        });
        add($jobs, "C-typed", "simd-json-typed+copy", $b.len(), || {
            let mut v = black_box($b).to_vec();
            Ok(simd_json::serde::from_slice::<$ty>(&mut v)?)
        });
        add($jobs, "C-typed", "miniserde-typed", $b.len(), || {
            Ok(miniserde::json::from_str::<$ty>(black_box($text))?)
        });
        add($jobs, "C-typed", "sonic-rs-typed", $b.len(), || {
            Ok(sonic_rs::from_slice::<$ty>(black_box($b))?)
        });
    }};
}
fn string_list(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => {
            for v in a {
                string_list(v, out)
            }
        }
        serde_json::Value::Object(o) => {
            for (k, v) in o {
                out.push(k.clone());
                string_list(v, out)
            }
        }
        _ => {}
    }
}
fn main() -> R<()> {
    conformance()?;
    let args: Vec<String> = std::env::args().collect();
    let dest = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("/tmp/jmax-vs-parsers/results.jsonl");
    let mut output = BufWriter::new(fs::File::create(dest)?);
    let check_only = std::env::var_os("JMAX_CHECK_ONLY").is_some();
    let samples: usize = std::env::var("JMAX_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(61);
    for name in [
        "twitter",
        "citm_catalog",
        "canada",
        "github_events",
        "apache_builds",
        "endpoint_batch",
    ] {
        eprintln!("preflight {name}");
        let b = fs::read(format!("/tmp/jmax-bench-data/{name}.json"))?;
        let text = std::str::from_utf8(&b)?;
        let b = b.as_slice();
        let reference: serde_json::Value = serde_json::from_slice(b)?;
        let doc = jsonmax::parse::parse_slice(b, options())?;
        let encoded = jsonmax::encode::document_to_vec(&doc, Default::default())?;
        assert_eq!(
            reference,
            serde_json::from_slice::<serde_json::Value>(&encoded)?,
            "jmax round trip {name}"
        );
        let mut scratch = b.to_vec();
        let borrowed = simd_json::to_borrowed_value(&mut scratch)?;
        let simd_encoded = simd_json::to_vec(&borrowed)?;
        assert!(serde_json::from_slice::<serde_json::Value>(&simd_encoded).is_ok());
        let sonic: sonic_rs::Value = sonic_rs::from_slice(b)?;
        let json_rust = json::parse(text)?;
        let mini: miniserde::json::Value = miniserde::json::from_str(text)?;
        let mini_encoded = miniserde::json::to_string(&mini);
        assert!(serde_json::from_str::<serde_json::Value>(&mini_encoded).is_ok());
        // All serializers are checked for semantic equality below, with a small
        // tolerance for differing decimal-to-binary64 algorithms in competing DOMs.
        semantic_equal(&reference, &serde_json::from_slice(&simd_encoded)?)?;
        semantic_equal(&reference, &serde_json::from_str(&mini_encoded)?)?;
        semantic_equal(&reference, &serde_json::from_str(&json_rust.dump())?)?;
        semantic_equal(
            &reference,
            &serde_json::from_slice(&sonic_rs::to_vec(&sonic)?)?,
        )?;
        let any = jsonmax::endpoint::compile_native_schema(
            &Schema {
                name: "any".into(),
                schema_id: jsonmax::SchemaId(2),
                root: TypeId(0),
                nodes: vec![TypeNode::Any],
                definitions: vec![],
            },
            CompileLimits::default_limits(),
        )?;
        let selection = Selection {
            field_ids: vec![],
            strict_validate_unselected: true,
        };
        let p = plan(name)?;
        let mut bad_tail = b.to_vec();
        bad_tail.extend_from_slice(b" false");
        assert!(walk(&bad_tail).is_err());
        assert!(vm(&bad_tail, &any, &selection).is_err());
        assert!(jsonmax::parse::parse_slice(&bad_tail, options()).is_err());
        assert!(serde_json::from_slice::<serde::de::IgnoredAny>(&bad_tail).is_err());
        assert!(sonic_rs::from_slice::<sonic_rs::LazyValue>(&bad_tail).is_err());
        let mut mutable = bad_tail.clone();
        assert!(simd_json::to_tape(&mut mutable).is_err());
        let mut strings = vec![];
        string_list(&reference, &mut strings);
        let string_bytes: usize = strings.iter().map(String::len).sum();
        let escaped = jmax_strings(&strings)?;
        assert_eq!(strings, serde_json::from_slice::<Vec<String>>(&escaped)?);
        let payload = Payload::from_slice(b);
        let pikkr_path = format!("$.{}", query_path(name));
        let mut jobs = vec![];
        add(
            &mut jobs,
            "A-scan",
            "jmax-syntax-strict-borrowed",
            b.len(),
            || walk(black_box(b)),
        );
        add(
            &mut jobs,
            "A-scan",
            "jmax-validate-borrowed",
            b.len(),
            || {
                Ok(jsonmax::parse::validate_borrowed(
                    black_box(b),
                    options(),
                    &mut jmax_abi::syntax::Scratch::default(),
                )?)
            },
        );
        add(&mut jobs, "A-scan", "jmax-vm-any+copy", b.len(), || {
            vm(black_box(b), &any, &selection)
        });
        {
            let mut scratch = jsonmax::endpoint::ValidationScratch::default();
            let any = &any;
            add(
                &mut jobs,
                "A-scan",
                "jmax-schema-validate-borrowed",
                b.len(),
                move || {
                    Ok(jsonmax::endpoint::validate_borrowed(
                        &any,
                        black_box(b),
                        options(),
                        &mut scratch,
                    )?)
                },
            );
        }
        {
            let mut scratch = jsonmax::endpoint::ValidationScratch::default();
            let any = &any;
            let payload = &payload;
            add(
                &mut jobs,
                "A-scan",
                "jmax-schema-validate-warm-payload",
                b.len(),
                move || {
                    Ok(jsonmax::endpoint::validate_payload_with_scratch(
                        black_box(payload.clone()),
                        &any,
                        options(),
                        &mut scratch,
                    )?)
                },
            );
        }
        add(&mut jobs, "A-scan", "serde-json-ignored", b.len(), || {
            Ok(serde_json::from_slice::<serde::de::IgnoredAny>(black_box(
                b,
            ))?)
        });
        add(&mut jobs, "A-scan", "simd-json-tape+copy", b.len(), || {
            let mut v = black_box(b).to_vec();
            let tape = simd_json::to_tape(&mut v)?;
            black_box(tape);
            Ok(())
        });
        add(
            &mut jobs,
            "A-scan",
            "sonic-rs-lazy-document",
            b.len(),
            || Ok(sonic_rs::from_slice::<sonic_rs::LazyValue>(black_box(b))?),
        );
        if name == "github_events" {
            add(
                &mut jobs,
                "A-query-partial",
                "sonic-rs-get-index0",
                b.len(),
                || Ok(sonic_rs::get_from_slice(black_box(b), [0usize])?),
            );
        } else {
            let key = match name {
                "twitter" => "search_metadata",
                "citm_catalog" => "venueNames",
                "canada" => "type",
                "apache_builds" => "mode",
                _ => "batch_id",
            };
            add(
                &mut jobs,
                "A-query-partial",
                "sonic-rs-get-field",
                b.len(),
                move || Ok(sonic_rs::get_from_slice(black_box(b), [key])?),
            );
        }
        let query = jsonmax::query::compile_paths(&[query_path(name)])?;
        add(
            &mut jobs,
            "A-query-partial",
            "jmax-query-only-extract-borrowed",
            b.len(),
            move || {
                Ok(jsonmax::query::execute_borrowed(
                    &query,
                    black_box(b),
                    query_options(),
                )?)
            },
        );
        // This plan describes one row, while the github corpus is a root
        // array. Keep whole-document schema jobs out of this row-only plan.
        if name != "github_events" {
            add(
                &mut jobs,
                "A-query-partial",
                "jmax-strict-extract-borrowed",
                b.len(),
                || {
                    Ok(jsonmax::endpoint::extract_borrowed(
                        &p.schema,
                        &p.selection,
                        black_box(b),
                        options(),
                    )?)
                },
            );
            add(
                &mut jobs,
                "A-query-partial",
                "jmax-schema-query-extract-borrowed",
                b.len(),
                || {
                    Ok(jsonmax::endpoint::extract_query_borrowed(
                        &p.schema,
                        &p.selection,
                        black_box(b),
                        options(),
                    )?)
                },
            );
            {
                let mut scratch = jsonmax::endpoint::ValidationScratch::default();
                let payload = &payload;
                let p = &p;
                add(
                    &mut jobs,
                    "A-query-partial",
                    "jmax-schema-extract-warm-payload",
                    b.len(),
                    move || {
                        let values = jsonmax::endpoint::extract_payload_with_scratch(
                            black_box(&payload),
                            &p.schema,
                            &p.selection,
                            options(),
                            &mut scratch,
                        )?;
                        black_box(values);
                        Ok(())
                    },
                );
            }
        } else {
            for operation in [
                "Strict schema Extract",
                "Query schema Extract",
                "Warm schema Extract",
            ] {
                writeln!(
                    output,
                    "{}",
                    serde_json::json!({"corpus":name,
                    "category":"harness-capability","parser":"jsonmax",
                    "operation":operation,"status":"N/A (harness schema describes one row; corpus root is an array)"})
                )?;
            }
        }
        if name != "github_events" {
            let paths = [pikkr_path.as_bytes()];
            let mut untrained = pikkr::Pikkr::new(&paths, usize::MAX)?;
            let untrained_values = untrained.parse(&b)?;
            if untrained_values.first().and_then(|v| *v).is_none() {
                return Err("ps-pikkr failed to capture requested field".into());
            }
            let expected =
                selected_reference(name, &reference).ok_or("missing ps-pikkr reference field")?;
            for captured in untrained_values.iter().flatten() {
                semantic_equal(expected, &serde_json::from_slice(captured)?)?;
            }
            let mut trained = pikkr::Pikkr::new(&paths, 1)?;
            trained.parse(&b)?; // Training is deliberately outside all timed measurements.
            let trained_values = trained.parse(&b)?;
            if trained_values.first().and_then(|v| *v).is_none() {
                return Err("trained ps-pikkr failed to capture requested field".into());
            }
            for captured in trained_values.iter().flatten() {
                semantic_equal(expected, &serde_json::from_slice(captured)?)?;
            }
            add(
                &mut jobs,
                "A-query-partial",
                "ps-pikkr-untrained-query",
                b.len(),
                move || Ok(untrained.parse(black_box(b))?),
            );
            add(
                &mut jobs,
                "A-query-partial",
                "ps-pikkr-trained-query",
                b.len(),
                move || Ok(trained.parse(black_box(b))?),
            );
        } else {
            writeln!(
                output,
                "{}",
                serde_json::json!({"corpus":name,"category":"ps-pikkr-capability","parser":"ps-pikkr","operation":"Array-index Query","status":"N/A (object-field selector only)"})
            )?;
        }
        add(&mut jobs, "B-dom", "jmax-dom+copy", b.len(), || {
            Ok(jsonmax::parse::parse_slice(black_box(b), options())?)
        });
        add(&mut jobs, "B-dom", "jmax-dom-shared-input", b.len(), || {
            Ok(jsonmax::parse::parse_payload(
                black_box(payload.clone()),
                options(),
            )?)
        });
        add(&mut jobs, "B-dom", "jmax-dom-borrowed", b.len(), || {
            Ok(jsonmax::parse::parse_borrowed(black_box(b), options())?)
        });
        {
            let mut scratch = jsonmax::DomScratch::new();
            let payload = &payload;
            add(
                &mut jobs,
                "B-dom",
                "jmax-dom-warm-owned-scratch",
                b.len(),
                move || {
                    let d = jsonmax::parse::parse_payload_with_scratch(
                        black_box(payload.clone()),
                        options(),
                        &mut scratch,
                    )?;
                    let n = d.node_count();
                    black_box(n);
                    d.recycle(&mut scratch);
                    Ok(n)
                },
            );
        }
        {
            let mut scratch = jsonmax::DomScratch::new();
            add(
                &mut jobs,
                "B-dom",
                "jmax-dom-warm-borrowed-scratch",
                b.len(),
                move || {
                    let d = jsonmax::parse::parse_borrowed_with_scratch(
                        black_box(b),
                        options(),
                        &mut scratch,
                    )?;
                    let n = d.node_count();
                    black_box(n);
                    d.recycle(&mut scratch);
                    Ok(n)
                },
            );
        }
        add(
            &mut jobs,
            "B-dom",
            "jmax-dom+number-conversion+copy",
            b.len(),
            || {
                let d = jsonmax::parse::parse_slice(black_box(b), options())?;
                black_box(convert_numbers(&d)?);
                Ok(d)
            },
        );
        add(&mut jobs, "B-dom", "serde-json-value", b.len(), || {
            Ok(serde_json::from_slice::<serde_json::Value>(black_box(b))?)
        });
        add(
            &mut jobs,
            "B-dom",
            "simd-json-borrowed+copy",
            b.len(),
            || {
                let mut v = black_box(b).to_vec();
                black_box(simd_json::to_borrowed_value(&mut v)?);
                Ok(())
            },
        );
        add(&mut jobs, "B-dom", "simd-json-owned+copy", b.len(), || {
            let mut v = black_box(b).to_vec();
            Ok(simd_json::to_owned_value(&mut v)?)
        });
        add(&mut jobs, "B-dom", "sonic-rs-value", b.len(), || {
            Ok(sonic_rs::from_slice::<sonic_rs::Value>(black_box(b))?)
        });
        add(&mut jobs, "B-dom", "json-rust-value", b.len(), || {
            Ok(json::parse(black_box(text))?)
        });
        add(&mut jobs, "B-dom", "miniserde-value", b.len(), || {
            Ok(miniserde::json::from_str::<miniserde::json::Value>(
                black_box(text),
            )?)
        });
        match name {
            "twitter" => typed_jobs!(&mut jobs, b, text, &p, Twitter, jmax_twitter),
            "citm_catalog" => typed_jobs!(&mut jobs, b, text, &p, Citm, jmax_citm),
            "canada" => typed_jobs!(&mut jobs, b, text, &p, Canada, jmax_canada),
            "github_events" => typed_jobs!(&mut jobs, b, text, &p, Vec<Github>, jmax_github),
            "apache_builds" => typed_jobs!(&mut jobs, b, text, &p, Apache, jmax_apache),
            _ => typed_jobs!(&mut jobs, b, text, &p, TypedPayload, jmax_endpoint),
        }
        add(
            &mut jobs,
            "D-serialize",
            "jmax-native-tape-encode-sink",
            b.len(),
            || {
                let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
                jsonmax::encode::encode_document_streaming(
                    black_box(&doc),
                    &mut sink,
                    Default::default(),
                )?;
                Ok(sink.into_vec())
            },
        );
        add(
            &mut jobs,
            "D-serialize",
            "serde-json-to-vec",
            b.len(),
            || Ok(serde_json::to_vec(black_box(&reference))?),
        );
        add(
            &mut jobs,
            "D-serialize",
            "simd-json-to-vec",
            b.len(),
            || Ok(simd_json::to_vec(black_box(&borrowed))?),
        );
        add(
            &mut jobs,
            "D-serialize",
            "miniserde-to-string",
            b.len(),
            || Ok(miniserde::json::to_string(black_box(&mini))),
        );
        add(&mut jobs, "D-serialize", "sonic-rs-to-vec", b.len(), || {
            Ok(sonic_rs::to_vec(black_box(&sonic))?)
        });
        add(
            &mut jobs,
            "D-escape-strings",
            "jmax-string-writer",
            string_bytes,
            || jmax_strings(black_box(&strings)),
        );
        add(
            &mut jobs,
            "D-escape-strings",
            "serde-json-strings",
            string_bytes,
            || Ok(serde_json::to_vec(black_box(&strings))?),
        );
        add(
            &mut jobs,
            "D-escape-strings",
            "simd-json-strings",
            string_bytes,
            || Ok(simd_json::to_vec(black_box(&strings))?),
        );
        add(
            &mut jobs,
            "D-escape-strings",
            "miniserde-strings",
            string_bytes,
            || Ok(miniserde::json::to_string(black_box(&strings))),
        );
        add(
            &mut jobs,
            "D-escape-strings",
            "sonic-rs-strings",
            string_bytes,
            || Ok(sonic_rs::to_vec(black_box(&strings))?),
        );
        for job in &mut jobs {
            (job.run)().map_err(|e| format!("{name}/{} preflight: {e}", job.name))?;
            if check_only {
                continue;
            }
            let start = Instant::now();
            let mut count = 0;
            while count < 5 || start.elapsed() < Duration::from_millis(50) {
                (job.run)()?;
                count += 1;
            }
            let ns = start.elapsed().as_nanos() as f64 / count as f64;
            job.reps = (2_000_000.0 / ns).ceil().clamp(1., 65536.) as usize;
            job.samples.reserve(samples);
            let before = allocation_snapshot();
            for _ in 0..5 {
                (job.run)()?;
            }
            let after = allocation_snapshot();
            job.allocs_op = (after.0 - before.0) as f64 / 5.;
            job.bytes_op = (after.1 - before.1) as f64 / 5.;
        }
        for operation in ["DOM Parse", "Full Validate", "Serialize"] {
            writeln!(
                output,
                "{}",
                serde_json::json!({"corpus":name,"category":"ps-pikkr-capability","parser":"ps-pikkr","operation":operation,"status":"N/A (Query-only selector)"})
            )?;
        }
        if check_only {
            eprintln!(
                "PASS {name}: DOM semantic checks, typed equality, {} operation preflights",
                jobs.len()
            );
            continue;
        }
        // Fixed-seed shuffled job order per round balances drift without concurrency.
        let mut state = 0x6a09e667f3bcc909u64;
        for _ in 0..samples {
            let mut order: Vec<_> = (0..jobs.len()).collect();
            for i in (1..order.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                order.swap(i, state as usize % (i + 1));
            }
            for i in order {
                let job = &mut jobs[i];
                let start = Instant::now();
                for _ in 0..job.reps {
                    (job.run)()?;
                }
                job.samples
                    .push(start.elapsed().as_nanos() as f64 / job.reps as f64);
            }
        }
        for mut job in jobs {
            let chronological = job.samples.clone();
            job.samples.sort_by(f64::total_cmp);
            let n = job.samples.len();
            let med = job.samples[(n - 1) / 2];
            let p95 = job.samples[(n * 95).div_ceil(100) - 1];
            let row = serde_json::json!({"corpus":name,"category":job.category,"parser":job.name,"bytes":job.bytes,"median_ns":med,"p95_ns":p95,"mb_s":job.bytes as f64*1000./med,"gb_s":job.bytes as f64/med,"reps":job.reps,"samples_ns":job.samples,"chronological_samples_ns":chronological,"allocs_op":job.allocs_op,"bytes_op":job.bytes_op});
            writeln!(output, "{row}")?;
            eprintln!(
                "{name:15} {:20} {:38} {:10.0} ns {:9.2} MB/s",
                job.category,
                job.name,
                med,
                job.bytes as f64 * 1000. / med
            );
        }
        prepared_simd(name, b, samples, &mut output)?;
        output.flush()?;
    }
    let allocator = if cfg!(feature = "system-alloc") {
        "System"
    } else {
        "RustyAlloc"
    };
    writeln!(
        output,
        "{}",
        serde_json::json!({"category":"harness","allocator":allocator,
  "alloc_calls":ALLOC_CALLS.load(Ordering::Relaxed),"dealloc_calls":DEALLOC_CALLS.load(Ordering::Relaxed),
  "alloc_zeroed_calls":ZEROED_CALLS.load(Ordering::Relaxed),"realloc_calls":REALLOC_CALLS.load(Ordering::Relaxed)})
    )?;
    Ok(())
}
fn jmax_strings(strings: &[String]) -> R<Vec<u8>> {
    let mut sink = jmax_encode::VecSink::new(32 * 1024 * 1024);
    {
        let mut w = jmax_encode::JsonWriter::new(&mut sink, Default::default());
        w.begin_array()?;
        for s in strings {
            w.string(s)?;
        }
        w.end_array()?;
        w.finish()?;
    }
    Ok(sink.into_vec())
}
fn semantic_equal(a: &serde_json::Value, b: &serde_json::Value) -> R<()> {
    use serde_json::Value::*;
    match (a, b) {
        (Number(x), Number(y)) => {
            if x == y {
                return Ok(());
            }
            let (x, y) = (x.as_f64().ok_or("number")?, y.as_f64().ok_or("number")?);
            if (x - y).abs() <= x.abs().max(y.abs()).max(1.) * 1e-14 {
                Ok(())
            } else {
                Err("numeric mismatch".into())
            }
        }
        (Array(x), Array(y)) if x.len() == y.len() => {
            for (a, b) in x.iter().zip(y) {
                semantic_equal(a, b)?;
            }
            Ok(())
        }
        (Object(x), Object(y)) if x.len() == y.len() => {
            for (k, a) in x {
                semantic_equal(a, y.get(k).ok_or("missing key")?)?;
            }
            Ok(())
        }
        _ if a == b => Ok(()),
        _ => Err("semantic mismatch".into()),
    }
}

fn conformance() -> R<()> {
    let cases: Vec<(&str, &[u8], bool)> = vec![
        (
            "unicode-and-escapes",
            br#"{"text":"\uD83D\uDE00\n\t\\\"","array":[true,false,null,0,1.5e2]}"#,
            true,
        ),
        (
            "literal-utf8",
            "{\"text\":\"東京 café 🛒\"}".as_bytes(),
            true,
        ),
        ("empty-object", b"{}", true),
        ("empty-array", b"[]", true),
        ("trailing-root", b"{} false", false),
        ("array-trailing-comma", b"[1,]", false),
        ("object-trailing-comma", br#"{"a":1,}"#, false),
        ("missing-colon", br#"{"a" 1}"#, false),
        ("invalid-escape", br#"{"s":"\x"}"#, false),
        ("lone-high-surrogate", br#"{"s":"\uD800"}"#, false),
        ("lone-low-surrogate", br#"{"s":"\uDC00"}"#, false),
        ("raw-control", b"[\"a\x01b\"]", false),
        ("invalid-utf8", b"[\"\xff\"]", false),
        ("invalid-number", b"[01]", false),
        ("incomplete-fraction", b"[1.]", false),
        ("incomplete-exponent", b"[1e+]", false),
        ("unclosed-nested", br#"{"a":[{"b":1}]"#, false),
        ("empty", b"", false),
    ];
    let mut rows = Vec::new();
    for (name, b, valid) in cases {
        let mut answers = std::collections::BTreeMap::new();
        let mut panicked = Vec::new();
        answers.insert("jmax-cursor", walk(b).is_ok());
        answers.insert(
            "jmax-dom",
            jsonmax::parse::parse_slice(b, options()).is_ok(),
        );
        answers.insert(
            "serde-ignored",
            serde_json::from_slice::<serde::de::IgnoredAny>(b).is_ok(),
        );
        answers.insert(
            "serde-value",
            serde_json::from_slice::<serde_json::Value>(b).is_ok(),
        );
        answers.insert(
            "sonic-lazy-document",
            match std::panic::catch_unwind(|| {
                sonic_rs::from_slice::<sonic_rs::LazyValue>(b).is_ok()
            }) {
                Ok(accepted) => accepted,
                Err(_) => {
                    panicked.push("sonic-lazy-document");
                    false
                }
            },
        );
        answers.insert(
            "sonic-value",
            match std::panic::catch_unwind(|| sonic_rs::from_slice::<sonic_rs::Value>(b).is_ok()) {
                Ok(accepted) => accepted,
                Err(_) => {
                    panicked.push("sonic-value");
                    false
                }
            },
        );
        let mut v = b.to_vec();
        answers.insert("simd-tape", simd_json::to_tape(&mut v).is_ok());
        let mut v = b.to_vec();
        answers.insert(
            "simd-borrowed",
            simd_json::to_borrowed_value(&mut v).is_ok(),
        );
        answers.insert(
            "json-rust",
            std::str::from_utf8(b).is_ok_and(|s| json::parse(s).is_ok()),
        );
        answers.insert(
            "miniserde-value",
            std::str::from_utf8(b)
                .is_ok_and(|s| miniserde::json::from_str::<miniserde::json::Value>(s).is_ok()),
        );
        assert_eq!(answers["jmax-cursor"], valid, "cursor conformance {name}");
        assert_eq!(answers["jmax-dom"], valid, "DOM conformance {name}");
        rows.push(serde_json::json!({"case":name,"valid_json":valid,"accepted":answers,"panicked":panicked}));
    }
    fs::write(
        "/tmp/jmax-vs-parsers/validation-matrix.json",
        serde_json::to_vec_pretty(&rows)?,
    )?;
    eprintln!("PASS conformance: {} syntax/UTF-8/escape cases", rows.len());
    Ok(())
}

fn allocation_snapshot() -> (u64, u64) {
    (
        ALLOC_CALLS.load(Ordering::Relaxed)
            + ZEROED_CALLS.load(Ordering::Relaxed)
            + REALLOC_CALLS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}
// Input refresh is outside each timer; allocation/destruction of the result is inside.
fn prepared_simd(name: &str, b: &[u8], samples: usize, output: &mut impl Write) -> R<()> {
    for tape in [false, true] {
        let mut pool: Vec<Vec<u8>> = (0..8).map(|_| b.to_vec()).collect();
        let mut times = Vec::with_capacity(samples);
        let mut measured_allocs = 0;
        let mut measured_bytes = 0;
        for sample in 0..samples + 3 {
            for buffer in &mut pool {
                buffer.copy_from_slice(b);
            }
            let before = allocation_snapshot();
            let start = Instant::now();
            for buffer in &mut pool {
                if tape {
                    black_box(simd_json::to_tape(black_box(buffer))?);
                } else {
                    black_box(simd_json::to_borrowed_value(black_box(buffer))?);
                }
            }
            let elapsed = start.elapsed().as_nanos() as f64 / 8.;
            let after = allocation_snapshot();
            if sample >= 3 {
                times.push(elapsed);
                measured_allocs += after.0 - before.0;
                measured_bytes += after.1 - before.1;
            }
        }
        let chronological = times.clone();
        times.sort_by(f64::total_cmp);
        let med = times[(samples - 1) / 2];
        let p95 = times[(samples * 95).div_ceil(100) - 1];
        let parser = if tape {
            "simd-json-tape-prepared"
        } else {
            "simd-json-borrowed-prepared"
        };
        writeln!(
            output,
            "{}",
            serde_json::json!({"corpus":name,"category":if tape {"A-scan"} else {"B-dom"},"parser":parser,"bytes":b.len(),"median_ns":med,"p95_ns":p95,"mb_s":b.len() as f64*1000./med,"reps":8,"samples_ns":times,"chronological_samples_ns":chronological,"allocs_op":measured_allocs as f64/(samples*8) as f64,"bytes_op":measured_bytes as f64/(samples*8) as f64,"copied_input_bytes_timed":0,"prepared_input_bytes":pool.len()*b.len()})
        )?;
    }
    Ok(())
}
