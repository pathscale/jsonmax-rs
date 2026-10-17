//! Side-by-side decoding of Binance documents into typed values: the generated
//! native decoders against serde_json, sonic-rs and simd-json.
//!
//! Competitor structs are written the way a user of those crates would write
//! them: `String` decimals (what the wire carries, no parsing) and fixed-point
//! decimals (an i64 scaled by 1e8, parsed while deserializing). jmax-native
//! produces exact `Decimal128` values and rejects unknown keys, which the
//! serde-based rows do not (they ignore them), so the work is comparable but
//! not identical; the report says so.

#![allow(non_snake_case)]
extern crate alloc;
#[path = "../../metrics.rs"]
pub mod metrics;
#[allow(clippy::all, dead_code, unused)]
pub mod trades;

use jmax_native::{decode_into, NativeContext, NativeDecode, NumericPolicy, Reader, VmLimits};
use jmax_native_fixtures::binance::models as bm;
use jmax_native_fixtures::harness::{depth_document, snapshot_document, AGG_TRADE};
use serde::de::{DeserializeOwned, Deserializer, Visitor};
use serde_derive::Deserialize;
use std::fmt;

pub type R<T> = Result<T, Box<dyn std::error::Error>>;
pub fn err<E: fmt::Display>(e: E) -> Box<dyn std::error::Error> {
    e.to_string().into()
}

/// A decimal string parsed to an i64 scaled by 1e8 (the fixed-point a client
/// would pick for Binance prices). Truncates beyond 8 fractional digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Fx(pub i64);

pub fn parse_fx(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for byte in whole.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
    }
    let mut taken = 0;
    for byte in fraction.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        if taken < 8 {
            value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
            taken += 1;
        }
    }
    for _ in taken..8 {
        value = value.checked_mul(10)?;
    }
    Some(if negative { -value } else { value })
}

impl<'de> serde::Deserialize<'de> for Fx {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Fx;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a decimal string")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Fx, E> {
                parse_fx(v).map(Fx).ok_or_else(|| E::custom("bad decimal"))
            }
        }
        deserializer.deserialize_str(V)
    }
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct AggTradeS<D> {
    pub e: String,
    #[serde(rename = "E")]
    pub event_time: i64,
    pub s: String,
    pub a: i64,
    pub p: D,
    pub q: D,
    pub f: i64,
    pub l: i64,
    #[serde(rename = "T")]
    pub trade_time: i64,
    pub m: bool,
    #[serde(rename = "M")]
    pub best_match: Option<bool>,
}
#[derive(Deserialize, Debug, PartialEq)]
pub struct DepthUpdateS<D> {
    pub e: String,
    #[serde(rename = "E")]
    pub event_time: i64,
    pub s: String,
    #[serde(rename = "U")]
    pub first: i64,
    pub u: i64,
    pub b: Vec<(D, D)>,
    pub a: Vec<(D, D)>,
}
#[derive(Deserialize, Debug, PartialEq)]
pub struct SnapshotS<D> {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: i64,
    pub bids: Vec<(D, D)>,
    pub asks: Vec<(D, D)>,
}
#[derive(Deserialize, Debug, PartialEq)]
pub struct TradesS<D> {
    pub trades: Vec<AggTradeS<D>>,
}

/// One benchmark document: its bytes, the native type and the serde types.
pub trait Case {
    type Native: NativeDecode + Default;
    type Str: DeserializeOwned;
    type Fix: DeserializeOwned;
    const NAME: &'static str;
    fn bytes() -> Vec<u8>;
    /// A value every row must agree on: integers, counts and decimals scaled to 1e8.
    fn sum_native(value: &Self::Native) -> i128;
    fn sum_str(value: &Self::Str) -> i128;
    fn sum_fix(value: &Self::Fix) -> i128;
}

fn dec(d: jmax_native::Decimal128) -> i128 {
    assert_eq!(d.scale, 8, "fixtures use 8 fractional digits");
    d.coefficient
}
fn st(text: &str) -> i128 {
    i128::from(parse_fx(text).expect("decimal"))
}

pub struct AggTradeCase;
impl Case for AggTradeCase {
    type Native = bm::AggTrade;
    type Str = AggTradeS<String>;
    type Fix = AggTradeS<Fx>;
    const NAME: &'static str = "aggTrade";
    fn bytes() -> Vec<u8> {
        AGG_TRADE.as_bytes().to_vec()
    }
    fn sum_native(v: &bm::AggTrade) -> i128 {
        i128::from(v.event_time_ms + v.aggregate_trade_id + v.first_trade_id + v.last_trade_id + v.trade_time_ms)
            + dec(v.price)
            + dec(v.quantity)
            + v.symbol.len() as i128
            + i128::from(v.buyer_is_maker)
            + i128::from(v.best_match == Some(true))
    }
    fn sum_str(v: &AggTradeS<String>) -> i128 {
        i128::from(v.event_time + v.a + v.f + v.l + v.trade_time)
            + st(&v.p)
            + st(&v.q)
            + v.s.len() as i128
            + i128::from(v.m)
            + i128::from(v.best_match == Some(true))
    }
    fn sum_fix(v: &AggTradeS<Fx>) -> i128 {
        i128::from(v.event_time + v.a + v.f + v.l + v.trade_time)
            + i128::from(v.p.0)
            + i128::from(v.q.0)
            + v.s.len() as i128
            + i128::from(v.m)
            + i128::from(v.best_match == Some(true))
    }
}

fn levels_native(levels: &[alloc::boxed::Box<bm::PriceLevel>]) -> i128 {
    levels.iter().map(|l| dec(l.price) + dec(l.quantity)).sum::<i128>() + levels.len() as i128
}
fn levels_str(levels: &[(String, String)]) -> i128 {
    levels.iter().map(|(p, q)| st(p) + st(q)).sum::<i128>() + levels.len() as i128
}
fn levels_fix(levels: &[(Fx, Fx)]) -> i128 {
    levels.iter().map(|(p, q)| i128::from(p.0) + i128::from(q.0)).sum::<i128>() + levels.len() as i128
}

pub struct Depth20;
impl Case for Depth20 {
    type Native = bm::DepthUpdate;
    type Str = DepthUpdateS<String>;
    type Fix = DepthUpdateS<Fx>;
    const NAME: &'static str = "depth_20";
    fn bytes() -> Vec<u8> {
        depth_document(20)
    }
    fn sum_native(v: &bm::DepthUpdate) -> i128 {
        i128::from(v.event_time_ms + v.first_update_id + v.final_update_id) + levels_native(&v.bids) + levels_native(&v.asks)
    }
    fn sum_str(v: &DepthUpdateS<String>) -> i128 {
        i128::from(v.event_time + v.first + v.u) + levels_str(&v.b) + levels_str(&v.a)
    }
    fn sum_fix(v: &DepthUpdateS<Fx>) -> i128 {
        i128::from(v.event_time + v.first + v.u) + levels_fix(&v.b) + levels_fix(&v.a)
    }
}

pub struct Depth1000;
impl Case for Depth1000 {
    type Native = bm::DepthSnapshot;
    type Str = SnapshotS<String>;
    type Fix = SnapshotS<Fx>;
    const NAME: &'static str = "depth_1000";
    fn bytes() -> Vec<u8> {
        snapshot_document(1000)
    }
    fn sum_native(v: &bm::DepthSnapshot) -> i128 {
        i128::from(v.last_update_id) + levels_native(&v.bids) + levels_native(&v.asks)
    }
    fn sum_str(v: &SnapshotS<String>) -> i128 {
        i128::from(v.last_update_id) + levels_str(&v.bids) + levels_str(&v.asks)
    }
    fn sum_fix(v: &SnapshotS<Fx>) -> i128 {
        i128::from(v.last_update_id) + levels_fix(&v.bids) + levels_fix(&v.asks)
    }
}

pub struct Trades100;
impl Case for Trades100 {
    type Native = trades::models::TradeBatch;
    type Str = TradesS<String>;
    type Fix = TradesS<Fx>;
    const NAME: &'static str = "trades_100";
    fn bytes() -> Vec<u8> {
        let one = |i: usize| {
            format!(
                "{{\"e\":\"aggTrade\",\"E\":{},\"s\":\"BNBBTC\",\"a\":{},\"p\":\"{}.{:08}\",\"q\":\"{}.{:08}\",\"f\":{},\"l\":{},\"T\":{},\"m\":{},\"M\":true}}",
                1_700_000_000_000u64 + i as u64 * 13,
                12_345 + i,
                i % 900,
                i * 37 + 11,
                i % 70,
                i * 91 + 5,
                100 + i * 3,
                102 + i * 3,
                1_700_000_000_000u64 + i as u64 * 13 - 4,
                i % 2 == 0
            )
        };
        let body = (0..100).map(one).collect::<Vec<_>>().join(",");
        format!("{{\"trades\":[{body}]}}").into_bytes()
    }
    fn sum_native(v: &trades::models::TradeBatch) -> i128 {
        v.trades
            .iter()
            .map(|t| {
                i128::from(t.event_time_ms + t.aggregate_trade_id + t.first_trade_id + t.last_trade_id + t.trade_time_ms)
                    + dec(t.price)
                    + dec(t.quantity)
                    + t.symbol.len() as i128
                    + i128::from(t.buyer_is_maker)
                    + i128::from(t.best_match == Some(true))
            })
            .sum::<i128>()
            + v.trades.len() as i128
    }
    fn sum_str(v: &TradesS<String>) -> i128 {
        v.trades.iter().map(AggTradeCase::sum_str).sum::<i128>() + v.trades.len() as i128
    }
    fn sum_fix(v: &TradesS<Fx>) -> i128 {
        v.trades.iter().map(AggTradeCase::sum_fix).sum::<i128>() + v.trades.len() as i128
    }
}

pub const NUMERIC: fn() -> NumericPolicy = NumericPolicy::strict_default;

/// Limits that let `budget_safe` vouch for any of the documents here.
pub fn roomy_limits() -> VmLimits {
    VmLimits { max_steps: 1 << 28, ..VmLimits::default() }
}

/// The route taken for inputs `budget_safe` cannot vouch for, and by decoders
/// that accept unknown keys: the schema VM validates the whole input, then the
/// native decoder reads it again.
pub struct VmFirst<T: NativeDecode> {
    program: jmax_native::ValidatedProgram,
    ctx: NativeContext,
    limits: VmLimits,
    _marker: std::marker::PhantomData<T>,
}
impl<T: NativeDecode> VmFirst<T> {
    pub fn new() -> R<Self> {
        let program = T::build_program().map_err(err)?;
        let limits = VmLimits::default();
        let ctx = NativeContext::with_program(&program, limits, NUMERIC()).map_err(err)?;
        Ok(Self { program, ctx, limits, _marker: std::marker::PhantomData })
    }
    pub fn decode(&mut self, input: &[u8], out: &mut T) -> R<()> {
        jmax_schema_vm::direct::validate_borrowed_into(
            &self.program,
            input,
            self.limits,
            NUMERIC(),
            &mut jmax_schema_vm::ValidationScratch::default(),
        )
        .map_err(err)?;
        let mut reader = Reader::new(input);
        T::decode_core(&mut reader, &mut self.ctx, out).map_err(|_| err("core decode failed"))?;
        reader.end().map_err(|_| err("trailing input"))?;
        Ok(())
    }
}

pub fn native_context(limits: VmLimits) -> NativeContext {
    NativeContext::new(limits, NUMERIC())
}

/// One timed or counted operation: a closure over prepared state.
pub type Op<'a> = Box<dyn FnMut() -> R<()> + 'a>;

/// Every row for one case, as `(parser, op)`; each op decodes `bytes` once and
/// returns an error on failure. Values are checked against each other first.
pub fn rows<'a, C: Case + 'a>(bytes: &'a [u8]) -> R<Vec<(&'static str, Op<'a>)>> {
    use std::hint::black_box;
    // Agreement of every row on the same document.
    let reference: C::Str = serde_json::from_slice(bytes)?;
    let want = C::sum_str(&reference);
    let mut native = C::Native::default();
    decode_into(&mut native_context(VmLimits::default()), bytes, &mut native).map_err(err)?;
    assert_eq!(C::sum_native(&native), want, "{}: native", C::NAME);
    let fix: C::Fix = serde_json::from_slice(bytes)?;
    assert_eq!(C::sum_fix(&fix), want, "{}: serde_json fixed", C::NAME);
    let mut buf = bytes.to_vec();
    assert_eq!(C::sum_str(&simd_json::serde::from_slice::<C::Str>(&mut buf).map_err(err)?), want, "{}: simd-json", C::NAME);
    let mut buf = bytes.to_vec();
    assert_eq!(C::sum_fix(&simd_json::serde::from_slice::<C::Fix>(&mut buf).map_err(err)?), want, "{}: simd-json fixed", C::NAME);
    assert_eq!(C::sum_str(&sonic_rs::from_slice::<C::Str>(bytes).map_err(err)?), want, "{}: sonic-rs", C::NAME);
    assert_eq!(C::sum_fix(&sonic_rs::from_slice::<C::Fix>(bytes).map_err(err)?), want, "{}: sonic-rs fixed", C::NAME);
    let mut vm = VmFirst::<C::Native>::new()?;
    let mut again = C::Native::default();
    vm.decode(bytes, &mut again)?;
    assert_eq!(C::sum_native(&again), want, "{}: vm+reparse", C::NAME);

    let mut ops: Vec<(&'static str, Op<'a>)> = Vec::new();
    {
        let mut ctx = native_context(VmLimits::default());
        let mut out = C::Native::default();
        ops.push(("jmax-native", Box::new(move || Ok(black_box(decode_into(&mut ctx, black_box(bytes), &mut out).map_err(err)?)))));
    }
    {
        let mut ctx = native_context(roomy_limits());
        let mut out = C::Native::default();
        ops.push(("jmax-native budget", Box::new(move || Ok(black_box(decode_into(&mut ctx, black_box(bytes), &mut out).map_err(err)?)))));
    }
    {
        let mut out = C::Native::default();
        ops.push(("jmax vm+reparse", Box::new(move || { vm.decode(black_box(bytes), &mut out)?; black_box(&out); Ok(()) })));
    }
    ops.push(("serde_json String", Box::new(move || { black_box(serde_json::from_slice::<C::Str>(black_box(bytes))?); Ok(()) })));
    ops.push(("serde_json fixed", Box::new(move || { black_box(serde_json::from_slice::<C::Fix>(black_box(bytes))?); Ok(()) })));
    ops.push(("sonic-rs String", Box::new(move || { black_box(sonic_rs::from_slice::<C::Str>(black_box(bytes)).map_err(err)?); Ok(()) })));
    ops.push(("sonic-rs fixed", Box::new(move || { black_box(sonic_rs::from_slice::<C::Fix>(black_box(bytes)).map_err(err)?); Ok(()) })));
    {
        let mut buf = bytes.to_vec();
        ops.push(("simd-json String", Box::new(move || {
            buf.copy_from_slice(bytes);
            black_box(simd_json::serde::from_slice::<C::Str>(black_box(&mut buf)).map_err(err)?);
            Ok(())
        })));
    }
    {
        let mut buf = bytes.to_vec();
        ops.push(("simd-json fixed", Box::new(move || {
            buf.copy_from_slice(bytes);
            black_box(simd_json::serde::from_slice::<C::Fix>(black_box(&mut buf)).map_err(err)?);
            Ok(())
        })));
    }
    Ok(ops)
}

pub const CASES: &[&str] = &["aggTrade", "depth_20", "depth_1000", "trades_100"];

/// Prepare the rows of the named case and hand them to `f` with the document size.
pub fn with_case<T>(name: &str, f: impl FnOnce(usize, Vec<(&'static str, Op<'_>)>) -> R<T>) -> R<T> {
    fn go<C: Case, T>(f: impl FnOnce(usize, Vec<(&'static str, Op<'_>)>) -> R<T>) -> R<T> {
        let bytes = C::bytes();
        let ops = rows::<C>(&bytes)?;
        f(bytes.len(), ops)
    }
    match name {
        "aggTrade" => go::<AggTradeCase, T>(f),
        "depth_20" => go::<Depth20, T>(f),
        "depth_1000" => go::<Depth1000, T>(f),
        "trades_100" => go::<Trades100, T>(f),
        other => Err(err(format!("unknown case {other}"))),
    }
}
