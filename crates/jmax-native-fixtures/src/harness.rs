//! Differential harness: a generated native decoder against the schema VM and
//! an independent tree oracle built from `jmax-syntax` events.
//!
//! Every assertion names the input, so a failure reproduces from its message.

use crate::regen::{model, spec, Fixture};
use jmax_abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
use jmax_abi::syntax::{DuplicatePolicyOwner, Event, Limits, ScalarKind, ScanStep};
use jmax_endpoint_model::native::schema_graph_for;
use jmax_endpoint_model::registry::build_registry;
use jmax_endpoint_model::SchemaId;
use jmax_native::{Error, ErrorCode, NativeContext, NativeDecode, Reader, VmLimits, decode_into};
use jmax_schema_aot::{compile_graph, ValidatedProgram};
use jmax_schema_vm::ValidationScratch;

pub const AGG_TRADE: &str = r#"{"e":"aggTrade","E":123456789,"s":"BNBBTC","a":12345,"p":"0.00100000","q":"100.00000000","f":100,"l":105,"T":123456785,"m":true,"M":true}"#;

/// The reference program the generated decoder must agree with, lowered here
/// from the model and specification independently of the embedded bytes.
pub fn reference_program(fixture: &Fixture, root: &str, reject_unknown: bool) -> ValidatedProgram {
    let model = model(fixture);
    let spec = spec(fixture, reject_unknown);
    let registry = build_registry(&model).expect("registry");
    let index = fixture.roots.iter().position(|candidate| *candidate == root).expect("known root");
    let graph = schema_graph_for(&registry, &spec, root, SchemaId(5000 + index as u32)).expect("graph");
    compile_graph(&graph).expect("program")
}

pub fn depth_document(levels: usize) -> Vec<u8> {
    let level = |i: usize| format!("[\"{}.{:08}\",\"{}.{:08}\"]", i % 90000, i, i % 1000, i * 7);
    let book = |n: usize| (0..n).map(level).collect::<Vec<_>>().join(",");
    format!(
        "{{\"e\":\"depthUpdate\",\"E\":123456789,\"s\":\"BNBBTC\",\"U\":157,\"u\":160,\"b\":[{}],\"a\":[{}]}}",
        book(levels),
        book(levels)
    )
    .into_bytes()
}

pub fn snapshot_document(levels: usize) -> Vec<u8> {
    let level = |i: usize| format!("[\"{}.{:08}\",\"{}.{:08}\"]", i % 90000, i, i % 1000, i * 7);
    let book = |n: usize| (0..n).map(level).collect::<Vec<_>>().join(",");
    format!("{{\"lastUpdateId\":1027024,\"bids\":[{}],\"asks\":[{}]}}", book(levels), book(levels)).into_bytes()
}

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
/// escaped letters, leading zeros, exponents and swapped bytes.
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
                ScalarKind::String => Json::String(text(&decoded, reference?)),
                ScalarKind::Number => Json::Number(String::from_utf8_lossy(span.slice(input)?).into_owned()),
                ScalarKind::True => Json::Bool(true),
                ScalarKind::False => Json::Bool(false),
                ScalarKind::Null => Json::Null,
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
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn int(&self) -> Option<i64> {
        match self {
            Json::Number(n) => n.parse().ok(),
            _ => None,
        }
    }
    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn decimal(&self) -> Option<Decimal128> {
        let quoted = format!("\"{}\"", self.string()?);
        jmax_decimal::parse::parse_quoted(quoted.as_bytes(), 0, &DecimalPolicy::spot_8()).ok()
    }
}

/// Oracle decoders onto the types of one generated module.
macro_rules! oracles {
    ($module:ident, $name:ident) => {
        pub mod $name {
            use super::*;
            use crate::$module::models::{AggTrade, DepthSnapshot, DepthUpdate, PriceLevel};

            fn book(value: &Json) -> Option<Vec<Box<PriceLevel>>> {
                match value {
                    Json::Array(items) => items
                        .iter()
                        .map(|level| match level {
                            Json::Array(pair) if pair.len() == 2 => Some(Box::new(PriceLevel {
                                price: pair[0].decimal()?,
                                quantity: pair[1].decimal()?,
                            })),
                            _ => None,
                        })
                        .collect(),
                    _ => None,
                }
            }

            pub fn trade(input: &[u8]) -> Option<AggTrade> {
                let root = tree(input)?;
                Some(AggTrade {
                    event_type: root.get("e")?.string()?.to_owned(),
                    event_time_ms: root.get("E")?.int()?,
                    symbol: root.get("s")?.string()?.to_owned(),
                    aggregate_trade_id: root.get("a")?.int()?,
                    price: root.get("p")?.decimal()?,
                    quantity: root.get("q")?.decimal()?,
                    first_trade_id: root.get("f")?.int()?,
                    last_trade_id: root.get("l")?.int()?,
                    trade_time_ms: root.get("T")?.int()?,
                    buyer_is_maker: root.get("m")?.bool()?,
                    best_match: match root.get("M") {
                        None => None,
                        Some(value) => Some(value.bool()?),
                    },
                })
            }

            pub fn depth(input: &[u8]) -> Option<DepthUpdate> {
                let root = tree(input)?;
                Some(DepthUpdate {
                    event_type: root.get("e")?.string()?.to_owned(),
                    event_time_ms: root.get("E")?.int()?,
                    symbol: root.get("s")?.string()?.to_owned(),
                    first_update_id: root.get("U")?.int()?,
                    final_update_id: root.get("u")?.int()?,
                    bids: book(root.get("b")?)?,
                    asks: book(root.get("a")?)?,
                })
            }

            pub fn snapshot(input: &[u8]) -> Option<DepthSnapshot> {
                let root = tree(input)?;
                Some(DepthSnapshot {
                    last_update_id: root.get("lastUpdateId")?.int()?,
                    bids: book(root.get("bids")?)?,
                    asks: book(root.get("asks")?)?,
                })
            }
        }
    };
}

oracles!(binance, oracle_strict);
oracles!(binance_open, oracle_open);
oracles!(binance_plain, oracle_plain);

/// The limit sets every input is checked under: the defaults and one change at
/// a time that moves a boundary the decoders must honor.
pub fn limit_variants() -> Vec<VmLimits> {
    let base = VmLimits::default();
    let mut variants = vec![base];
    let mut change = |edit: &dyn Fn(&mut VmLimits)| {
        let mut limits = base;
        edit(&mut limits);
        variants.push(limits);
    };
    change(&|l| l.syntax.max_depth = 1);
    change(&|l| l.syntax.max_depth = 2);
    change(&|l| l.syntax.max_depth = 3);
    change(&|l| l.syntax.max_depth = 4);
    change(&|l| l.syntax.max_fields_per_object = 3);
    for fields in [6, 7, 10, 11, 16, 17] {
        change(&|l| l.syntax.max_fields_per_object = fields);
    }
    for items in [1, 2, 3, 4] {
        change(&|l| l.syntax.max_array_items = items);
    }
    change(&|l| l.syntax.max_string_bytes = 5);
    change(&|l| l.syntax.max_number_digits = 4);
    change(&|l| l.syntax.max_decoded_bytes = 40);
    change(&|l| l.syntax.reject_duplicate_keys = false);
    change(&|l| l.syntax.reject_unknown_keys = false);
    change(&|l| {
        l.syntax.reject_unknown_keys = false;
        l.syntax.reject_duplicate_keys = false;
    });
    change(&|l| {
        l.syntax.reject_unknown_keys = false;
        l.syntax.max_depth = 2;
    });
    change(&|l| l.max_steps = 600);
    change(&|l| l.max_scratch_bytes = 3_000);
    variants
}

fn run_vm(program: &ValidatedProgram, input: &[u8], limits: VmLimits) -> Result<(), Error> {
    jmax_schema_vm::direct::validate_borrowed_into(
        program,
        input,
        limits,
        NumericPolicy::strict_default(),
        &mut ValidationScratch::default(),
    )
    .map(|_| ())
}

fn resource(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::BudgetExhausted | ErrorCode::DecodedArenaOverflow | ErrorCode::PayloadTooLarge)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    pub inputs: u32,
    pub accepted: u32,
    pub rejected: u32,
    pub unsafe_route: u32,
}

/// A decoder under test: its reference program and its value oracle.
pub struct Subject<T> {
    pub root: &'static str,
    pub program: ValidatedProgram,
    pub oracle: fn(&[u8]) -> Option<T>,
}

impl<T> Subject<T> {
    pub fn new(fixture: &Fixture, root: &'static str, reject_unknown: bool, oracle: fn(&[u8]) -> Option<T>) -> Self {
        Self { root, program: reference_program(fixture, root, reject_unknown), oracle }
    }
}

/// Check one input under one limit set against every contract property:
/// the core's accept set equals the VM's, the public result is the VM's exact
/// error or the typed value, and the value equals the oracle's.
pub fn check_one<T>(
    subject: &Subject<T>,
    ctx: &mut NativeContext,
    reused: &mut T,
    limits: VmLimits,
    input: &[u8],
    tally: &mut Tally,
)
where
    T: NativeDecode + Default + PartialEq + core::fmt::Debug,
{
    let root = subject.root;
    let shown = || String::from_utf8_lossy(input).into_owned();
    tally.inputs += 1;
    let vm = run_vm(&subject.program, input, limits);
    let safe = jmax_schema_vm::budget_safe(&subject.program, input.len(), limits);

    let mut core_out = T::default();
    let mut reader = Reader::new(input);
    let core = T::decode_core(&mut reader, ctx, &mut core_out).and_then(|()| reader.end());
    match &vm {
        Ok(()) => assert!(core.is_ok(), "{root}: core rejected an input the VM accepts: {:?}", shown()),
        Err(error) if resource(error.code) => {}
        Err(error) => assert!(
            core.is_err(),
            "{root}: core accepted an input the VM rejects ({:?}): {:?}",
            error.code,
            shown()
        ),
    }
    if safe {
        assert_eq!(vm.is_ok(), core.is_ok(), "{root} safe input: {:?}", shown());
    } else {
        tally.unsafe_route += 1;
    }
    if vm.is_ok() && core.is_ok() {
        assert_eq!(Some(&core_out), (subject.oracle)(input).as_ref(), "{root}: core value differs from oracle: {:?}", shown());
    }

    let mut public_out = T::default();
    let public = decode_into(ctx, input, &mut public_out);
    match (&vm, &public) {
        (Err(expected), Err(got)) => {
            assert_eq!(
                (got.code, got.byte_offset),
                (expected.code, expected.byte_offset),
                "{root}: public error differs from VM: {:?}",
                shown()
            );
            tally.rejected += 1;
        }
        (Err(expected), Ok(())) => panic!("{root}: public Ok but VM {:?}: {:?}", expected.code, shown()),
        (Ok(()), Err(got)) => panic!("{root}: public {:?} but VM accepts: {:?}", got.code, shown()),
        (Ok(()), Ok(())) => {
            tally.accepted += 1;
            assert_eq!(Some(&public_out), (subject.oracle)(input).as_ref(), "{root}: public value differs from oracle: {:?}", shown());
        }
    }

    // A long-lived output, reused across accepted and rejected inputs alike,
    // ends up equal to a fresh one whenever the decode succeeds.
    let reuse = decode_into(ctx, input, reused);
    assert_eq!(reuse.is_ok(), vm.is_ok(), "{root}: reused decode differs from VM: {:?}", shown());
    if reuse.is_ok() {
        assert_eq!(Some(&*reused), (subject.oracle)(input).as_ref(), "{root}: reused output differs from oracle: {:?}", shown());
    }
}

/// Every seed plus `rounds` mutants of it, under every limit variant.
pub fn differential<T>(subject: &Subject<T>, seeds: &[Vec<u8>], rounds: usize, salt: u64) -> Tally
where
    T: NativeDecode + Default + PartialEq + core::fmt::Debug,
{
    let mut rng = Rng(0xfeed_beef_0000_0001 ^ salt);
    let mut inputs: Vec<Vec<u8>> = seeds.to_vec();
    for seed in seeds {
        let rounds = if seed.len() > 4_000 { 4 } else { rounds };
        for _ in 0..rounds {
            inputs.push(mutate(seed, &mut rng));
        }
    }
    let mut tally = Tally::default();
    for limits in limit_variants() {
        let mut ctx = NativeContext::new(limits, NumericPolicy::strict_default());
        let mut reused = T::default();
        // Install the program through a first decode of a valid seed.
        let _ = decode_into(&mut ctx, &seeds[0], &mut reused);
        for input in &inputs {
            check_one(subject, &mut ctx, &mut reused, limits, input, &mut tally);
        }
    }
    tally
}

pub const SHAPE: &str = r#"{"id":7,"delta":-3,"ratio":1.5,"ok":true,"type":"circle","origin":{"x":1,"y":-2},"tags":[{"name":"a","weight":0.5},{"name":"b"}],"names":["u","v\n"],"grid":[[1,2],[],[3]],"pairs":[[1,2],[3,4]],"price":"12.50000000","note":"hi","extra":[1,-2],"pin":{"name":"p"},"cost":"0.00000001","flag":false,"count":9}"#;

impl Json {
    /// An integer lexeme inside `min..=max`, with negative zero equal to zero.
    pub fn int_in(&self, min: i128, max: i128) -> Option<i128> {
        match self {
            Json::Number(n) => n.parse::<i128>().ok().filter(|v| (min..=max).contains(v)),
            _ => None,
        }
    }
    pub fn float(&self) -> Option<f64> {
        match self {
            Json::Number(n) => n.parse::<f64>().ok().filter(|v| v.is_finite()),
            _ => None,
        }
    }
    pub fn array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// Oracle decoder for the `Shape` fixture onto the types of one generated module.
macro_rules! shape_oracles {
    ($module:ident, $name:ident) => {
        pub mod $name {
            use super::*;
            use crate::$module::models::{Pair, Point, Shape, Tag};

            fn tag(value: &Json) -> Option<Tag> {
                Some(Tag {
                    name: value.get("name")?.string()?.to_owned(),
                    weight: match value.get("weight") {
                        None => None,
                        Some(weight) => Some(weight.float()?),
                    },
                })
            }

            fn optional<T>(value: Option<&Json>, read: impl Fn(&Json) -> Option<T>) -> Option<Option<T>> {
                match value {
                    None => Some(None),
                    Some(value) => Some(Some(read(value)?)),
                }
            }

            pub fn shape(input: &[u8]) -> Option<Shape> {
                let root = tree(input)?;
                let origin = root.get("origin")?;
                Some(Shape {
                    id: root.get("id")?.int_in(0, i128::from(u32::MAX))? as u32,
                    delta: root.get("delta")?.int_in(i128::from(i32::MIN), i128::from(i32::MAX))? as i32,
                    ratio: root.get("ratio")?.float()?,
                    ok: root.get("ok")?.bool()?,
                    r#type: root.get("type")?.string()?.to_owned(),
                    origin: Box::new(Point {
                        x: origin.get("x")?.int_in(i128::from(i64::MIN), i128::from(i64::MAX))? as i64,
                        y: origin.get("y")?.int_in(i128::from(i64::MIN), i128::from(i64::MAX))? as i64,
                    }),
                    tags: root.get("tags")?.array()?.iter().map(|v| Some(Box::new(tag(v)?))).collect::<Option<_>>()?,
                    names: root.get("names")?.array()?.iter().map(|v| Some(v.string()?.to_owned())).collect::<Option<_>>()?,
                    grid: root
                        .get("grid")?
                        .array()?
                        .iter()
                        .map(|row| row.array()?.iter().map(|v| v.int()).collect::<Option<Vec<i64>>>())
                        .collect::<Option<_>>()?,
                    pairs: root
                        .get("pairs")?
                        .array()?
                        .iter()
                        .map(|pair| match pair.array()? {
                            [low, high] => Some(Box::new(Pair { low: low.int()?, high: high.int()? })),
                            _ => None,
                        })
                        .collect::<Option<_>>()?,
                    price: root.get("price")?.decimal()?,
                    note: optional(root.get("note"), |v| Some(v.string()?.to_owned()))?,
                    extra: optional(root.get("extra"), |v| {
                        v.array()?
                            .iter()
                            .map(|n| Some(n.int_in(i128::from(i32::MIN), i128::from(i32::MAX))? as i32))
                            .collect::<Option<Vec<i32>>>()
                    })?,
                    pin: optional(root.get("pin"), |v| Some(Box::new(tag(v)?)))?,
                    cost: optional(root.get("cost"), |v| v.decimal())?,
                    flag: optional(root.get("flag"), |v| v.bool())?,
                    count: optional(root.get("count"), |v| Some(v.int_in(0, i128::from(u32::MAX))? as u32))?,
                })
            }
        }
    };
}

shape_oracles!(shapes, shape_strict);
shape_oracles!(shapes_open, shape_open);

/// A valid document of the `formats` fixture.
pub const FORMATS_DOC: &str = r#"{"id":"abc_DEF-12345678","tag":"V1StGXR8_Z5jdHi6B-myT","sess":"Ab12Cd34Ef56Gh78","uid":"123e4567-e89b-12d3-a456-426614174000","ip":"192.168.0.1","blob":"Zm9vYmE=","addr":"0x00112233445566778899aabbccddeeff00112233","tx":"0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","many":["aaaaaaaaaaaaaaaa","bbbb_bbb-bbbbbbb"],"sessions":["Zz09Aa12Bb34Cc56"],"names":["x"],"items":[{"ref":"abcd1234","uid":"00000000-0000-0000-0000-000000000000"}],"nick":"nick_ok1","extra":"AQID","peer":"::1"}"#;

/// Oracle decoder for the `formats` fixture. Base64 is decoded with its own
/// bit accumulator, independent of the runtime's decoder.
macro_rules! formats_oracles {
    ($module:ident, $name:ident) => {
        pub mod $name {
            use super::*;
            use crate::$module::models::{Ids, Item};

            fn base64(text: &str) -> Option<Vec<u8>> {
                let mut out = Vec::new();
                let (mut bits, mut count) = (0u32, 0u32);
                for byte in text.bytes().take_while(|byte| *byte != b'=') {
                    let value = match byte {
                        b'A'..=b'Z' => byte - b'A',
                        b'a'..=b'z' => byte - b'a' + 26,
                        b'0'..=b'9' => byte - b'0' + 52,
                        b'+' => 62,
                        b'/' => 63,
                        _ => return None,
                    };
                    bits = bits << 6 | u32::from(value);
                    count += 6;
                    if count >= 8 {
                        count -= 8;
                        out.push((bits >> count) as u8);
                        bits &= (1 << count) - 1;
                    }
                }
                Some(out)
            }

            fn strings(value: &Json) -> Option<Vec<String>> {
                value.array()?.iter().map(|v| Some(v.string()?.to_owned())).collect()
            }

            fn optional<T>(value: Option<&Json>, read: impl Fn(&Json) -> Option<T>) -> Option<Option<T>> {
                match value {
                    None => Some(None),
                    Some(value) => Some(Some(read(value)?)),
                }
            }

            pub fn ids(input: &[u8]) -> Option<Ids> {
                let root = tree(input)?;
                let text = |key: &str| Some(root.get(key)?.string()?.to_owned());
                Some(Ids {
                    id: text("id")?,
                    tag: text("tag")?,
                    sess: text("sess")?,
                    uid: text("uid")?,
                    ip: text("ip")?,
                    blob: base64(root.get("blob")?.string()?)?,
                    addr: text("addr")?,
                    tx: text("tx")?,
                    many: strings(root.get("many")?)?,
                    sessions: strings(root.get("sessions")?)?,
                    names: strings(root.get("names")?)?,
                    items: root
                        .get("items")?
                        .array()?
                        .iter()
                        .map(|item| {
                            Some(Box::new(Item {
                                r#ref: item.get("ref")?.string()?.to_owned(),
                                uid: item.get("uid")?.string()?.to_owned(),
                            }))
                        })
                        .collect::<Option<_>>()?,
                    nick: optional(root.get("nick"), |v| Some(v.string()?.to_owned()))?,
                    extra: optional(root.get("extra"), |v| base64(v.string()?))?,
                    peer: optional(root.get("peer"), |v| Some(v.string()?.to_owned()))?,
                })
            }
        }
    };
}

formats_oracles!(formats, formats_strict);
formats_oracles!(formats_open, formats_open_oracle);
