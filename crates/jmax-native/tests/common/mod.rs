#![allow(dead_code)]
//! Fixture programs, seeds, mutation and an independent tree oracle built from
//! jmax-syntax events.

use jmax_abi::syntax::{DuplicatePolicyOwner, Event, Limits, ScanStep};
use jmax_endpoint_model::native::{schema_graph_for, validate_native_spec};
use jmax_endpoint_model::registry::build_registry;
use jmax_endpoint_model::SchemaId;
use jmax_endpoint_ron::{ConfigLimits, LoadOptions, load_model, load_native_spec};
use jmax_schema_aot::{ValidatedProgram, compile_graph};

const BINANCE: &str = include_str!("../../../../fixtures/native/binance.ron");
const BINANCE_NATIVE: &str = include_str!("../../../../fixtures/native/binance.native.ron");

pub fn program(root: &str) -> jmax_native::Result<ValidatedProgram> {
    let options = LoadOptions { require_version: false, ..LoadOptions::default() };
    let model = load_model(BINANCE, None, &options)?;
    let spec = load_native_spec(BINANCE_NATIVE, &ConfigLimits::default())?;
    let registry = build_registry(&model)?;
    validate_native_spec(&registry, &spec)?;
    let id = 5000 + spec.roots.iter().position(|r| r == root).unwrap() as u32;
    compile_graph(&schema_graph_for(&registry, &spec, root, SchemaId(id))?)
}

pub fn depth_document(levels: usize) -> Vec<u8> {
    let level = |i: usize| format!("[\"{}.{:08}\",\"{}.{:08}\"]", i % 90000, i, i % 1000, i * 7);
    let book = |n: usize| (0..n).map(level).collect::<Vec<_>>().join(",");
    format!(
        "{{\"e\":\"depthUpdate\",\"E\":123456789,\"s\":\"BNBBTC\",\"U\":157,\"u\":160,\"b\":[{}],\"a\":[{}]}}",
        book(levels), book(levels)
    )
    .into_bytes()
}

pub const AGG_TRADE: &str = r#"{"e":"aggTrade","E":123456789,"s":"BNBBTC","a":12345,"p":"0.00100000","q":"100.00000000","f":100,"l":105,"T":123456785,"m":true,"M":true}"#;

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const BYTES: &[u8] = b"{}[]:,\"\\ \t\n0123456789-+.eEtrufalsn\x00\x1f\x80\xc3\xa9\xed\xf0\xffuA";

/// Flips, insertions, deletions, truncation, whitespace runs, duplicated runs,
/// escaped and swapped keys, leading zeros and exponents.
pub fn mutate(input: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut input = input.to_vec();
    for _ in 0..1 + rng.below(3) {
        if input.is_empty() {
            break;
        }
        let at = rng.below(input.len());
        match rng.below(10) {
            0 => input[at] = BYTES[rng.below(BYTES.len())],
            1 => input.insert(at, BYTES[rng.below(BYTES.len())]),
            2 => {
                input.remove(at);
            }
            3 => input.truncate(at),
            4 => {
                for _ in 0..rng.below(40) {
                    input.insert(at, b' ');
                }
            }
            5 => {
                let end = (at + rng.below(30)).min(input.len());
                let copy = input[at..end].to_vec();
                input.splice(at..at, copy);
            }
            6 => {
                // Escape one letter: an escaped key or value spelling.
                if input[at].is_ascii_alphabetic() {
                    let escaped = format!("\\u{:04x}", input[at]);
                    input.splice(at..=at, escaped.bytes());
                }
            }
            7 => {
                input.splice(at..at, *b"0");
            }
            8 => {
                input.splice(at..at, *b"e2");
            }
            _ => {
                // Swap two nearby bytes: reorders keys and values.
                let other = (at + 1 + rng.below(20)).min(input.len() - 1);
                input.swap(at, other);
            }
        }
    }
    input
}

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    String(String),
    Number(String),
    Bool(bool),
    Null,
}

/// Build a tree from jmax-syntax events; `None` for anything it rejects.
pub fn tree(input: &[u8]) -> Option<Json> {
    let mut limits = Limits::strict_default();
    limits.reject_duplicate_keys = false;
    let mut cursor = jmax_syntax::BlockCursor::new(input, limits, None, DuplicatePolicyOwner::Disabled).ok()?;
    let mut decoded = Vec::new();
    let mut stack: Vec<(Json, Option<String>)> = Vec::new();
    let mut root = None;
    let text = |cursor_decoded: &[u8], reference: jmax_abi::span::StrRef| -> String {
        let bytes = match reference.source {
            jmax_abi::span::SpanSource::RawPayload => reference.span.slice(input),
            jmax_abi::span::SpanSource::DecodedArena => reference.span.slice(cursor_decoded),
        };
        String::from_utf8_lossy(bytes.unwrap()).into_owned()
    };
    loop {
        let event = match cursor.next_event(&mut decoded).ok()? {
            ScanStep::Emit(event) => event,
            ScanStep::Finished => break,
            ScanStep::NeedMore => return None,
        };
        let value = match event {
            Event::ObjectStart { .. } => {
                stack.push((Json::Object(vec![]), None));
                continue;
            }
            Event::ArrayStart { .. } => {
                stack.push((Json::Array(vec![]), None));
                continue;
            }
            Event::Key { str_ref } => {
                stack.last_mut()?.1 = Some(text(&decoded, str_ref));
                continue;
            }
            Event::ObjectEnd { .. } | Event::ArrayEnd { .. } => stack.pop()?.0,
            Event::Scalar { kind, span, decoded: reference } => match kind {
                jmax_abi::syntax::ScalarKind::String => Json::String(text(&decoded, reference?)),
                jmax_abi::syntax::ScalarKind::Number => {
                    Json::Number(String::from_utf8_lossy(span.slice(input)?).into_owned())
                }
                jmax_abi::syntax::ScalarKind::True => Json::Bool(true),
                jmax_abi::syntax::ScalarKind::False => Json::Bool(false),
                jmax_abi::syntax::ScalarKind::Null => Json::Null,
            },
        };
        match stack.last_mut() {
            None => root = Some(value),
            Some((Json::Object(members), key)) => members.push((key.take()?, value)),
            Some((Json::Array(items), _)) => items.push(value),
            _ => return None,
        }
    }
    cursor.finish_eof().ok()?;
    root
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn string(&self) -> Option<&str> {
        match self { Json::String(s) => Some(s), _ => None }
    }
    pub fn int(&self) -> Option<i64> {
        match self { Json::Number(n) => n.parse().ok(), _ => None }
    }
    pub fn bool(&self) -> Option<bool> {
        match self { Json::Bool(b) => Some(*b), _ => None }
    }
    pub fn decimal(&self) -> Option<jmax_abi::number::Decimal128> {
        let quoted = format!("\"{}\"", self.string()?);
        jmax_decimal::parse::parse_quoted(quoted.as_bytes(), 0, &jmax_abi::number::DecimalPolicy::spot_8()).ok()
    }
}
