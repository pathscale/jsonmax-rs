//! Side-by-side decoding of Binance documents into typed values: the generated
//! native decoders against serde_json, sonic-rs and simd-json.
//!
//! Competitor structs are written the way a user of those crates would write
//! them: `String` decimals (what the wire carries, no parsing) and fixed-point
//! decimals (an i64 scaled by 1e8, parsed while deserializing). jmax native
//! produces exact `Decimal128` values and rejects unknown keys, which the
//! serde-based rows do not (they ignore them), so the work is comparable but
//! not identical; the report says so.

#![allow(non_snake_case)]
extern crate alloc;
#[path = "../../metrics.rs"]
pub mod metrics;
#[path = "../../alloc_metrics.rs"] pub mod alloc_metrics;
pub static VM_RETAINED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub mod general;
#[path = "../../harness/src/common.rs"]
pub mod common;
pub static QUERY_RETAINED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
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
    /// The pure schema-VM decoder for this document, when one can exist.
    fn vm() -> R<Option<Box<dyn FnMut(&[u8]) -> R<Self::Native>>>> {
        Ok(None)
    }
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
    fn vm() -> R<Option<Box<dyn FnMut(&[u8]) -> R<bm::AggTrade>>>> {
        let mut vm = AggTradeVm::new()?;
        Ok(Some(Box::new(move |bytes| vm.decode(bytes))))
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
    fn vm() -> R<Option<Box<dyn FnMut(&[u8]) -> R<bm::DepthUpdate>>>> {
        let mut vm = DepthUpdateVm::new()?;
        Ok(Some(Box::new(move |bytes| vm.decode(bytes))))
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
    fn vm() -> R<Option<Box<dyn FnMut(&[u8]) -> R<bm::DepthSnapshot>>>> {
        let mut vm = DepthSnapshotVm::new()?;
        Ok(Some(Box::new(move |bytes| vm.decode(bytes))))
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
    fn vm() -> R<Option<Box<dyn FnMut(&[u8]) -> R<trades::models::TradeBatch>>>> {
        let mut vm = TradesVm::new()?;
        Ok(Some(Box::new(move |bytes| vm.decode(bytes))))
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

/// Row labels. The two typed jmax engines are `jmax native` and `jmax vm`; the
/// diagnostic rows are only produced with `DIAG=1`.
pub const NATIVE_ROW: &str = "jmax native";
pub const VM_ROW: &str = "jmax vm";
pub const VM_NATIVE_DIAG_ROW: &str = "jmax vm+native (diagnostic)";
pub const BUDGET_DIAG_ROW: &str = "jmax native budget (diagnostic)";
pub fn diag() -> bool {
    std::env::var_os("DIAG").is_some()
}

/// The pure schema-VM route for the flat Binance aggTrade object: one VM pass
/// validates the document and fills one slot per field, then the struct is
/// built from the slots (the same shape as the general harness's typed row).
/// Array-bearing documents use the row-staging VM routes below.
pub struct AggTradeVm {
    schema: jsonmax::CompiledSchema,
    selection: jsonmax::Selection,
    scratch: jsonmax::endpoint::ValidationScratch,
}
impl AggTradeVm {
    pub fn new() -> R<Self> {
        use jmax_schema_ir::{ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind};
        let decimal = || {
            let mut c = ScalarConstraints::new(ValueKind::Decimal);
            c.decimal = Some(jmax_schema_ir::DecimalConstraints {
                policy: jmax_abi::DecimalPolicy::spot_8(),
                minimum: None,
                maximum: None,
            });
            c
        };
        let fields: [(&str, ScalarConstraints, bool); 11] = [
            ("e", ScalarConstraints::new(ValueKind::String), true),
            ("E", ScalarConstraints::new(ValueKind::Integer), true),
            ("s", ScalarConstraints::new(ValueKind::String), true),
            ("a", ScalarConstraints::new(ValueKind::Integer), true),
            ("p", decimal(), true),
            ("q", decimal(), true),
            ("f", ScalarConstraints::new(ValueKind::Integer), true),
            ("l", ScalarConstraints::new(ValueKind::Integer), true),
            ("T", ScalarConstraints::new(ValueKind::Integer), true),
            ("m", ScalarConstraints::new(ValueKind::Boolean), true),
            ("M", ScalarConstraints::new(ValueKind::Boolean), false),
        ];
        let mut properties = Vec::new();
        let mut nodes = Vec::new();
        for (slot, (name, constraints, required)) in fields.into_iter().enumerate() {
            properties.push(Property {
                field_id: jsonmax::FieldId(slot as u32 + 1),
                name: name.into(),
                ty: TypeId(slot as u32 + 1),
                required,
                slot: Some(slot as u32),
            });
            nodes.push(TypeNode::Scalar(constraints));
        }
        nodes.insert(
            0,
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties,
                reject_unknown: true,
                min_properties: None,
                max_properties: None,
            }),
        );
        let schema = jsonmax::endpoint::compile_native_schema(
            &Schema { name: "aggTrade".into(), schema_id: jsonmax::SchemaId(1), root: TypeId(0), nodes, definitions: vec![] },
            jsonmax::CompileLimits::default_limits(),
        )
        .map_err(err)?;
        let selection = jsonmax::Selection {
            field_ids: (1..=11).map(jsonmax::FieldId).collect(),
            strict_validate_unselected: true,
        };
        Ok(Self { schema, selection, scratch: Default::default() })
    }
    pub fn decode(&mut self, bytes: &[u8]) -> R<bm::AggTrade> {
        use jsonmax::SlotValue;
        let s = jsonmax::endpoint::extract_borrowed_with_scratch(
            &self.schema,
            &self.selection,
            bytes,
            common::options(),
            &mut self.scratch,
        )
        .map_err(err)?;
        let text = |n: usize| -> R<String> {
            let SlotValue::Str(r) = s.slots[n] else { return Err(err("string slot")) };
            let base = match r.source {
                jmax_abi::span::SpanSource::RawPayload => s.raw,
                jmax_abi::span::SpanSource::DecodedArena => s.decoded,
            };
            Ok(std::str::from_utf8(r.span.slice(base).ok_or_else(|| err("span"))?).map_err(err)?.to_owned())
        };
        let int = |n: usize| -> R<i64> {
            match s.slots[n] {
                SlotValue::I64(x) => Ok(x),
                SlotValue::U64(x) => i64::try_from(x).map_err(err),
                _ => Err(err("int slot")),
            }
        };
        let dec = |n: usize| -> R<jmax_native::Decimal128> {
            match s.slots[n] {
                SlotValue::Decimal(d) => Ok(d),
                _ => Err(err("decimal slot")),
            }
        };
        let flag = |n: usize| -> R<Option<bool>> {
            match s.slots[n] {
                SlotValue::Bool(x) => Ok(Some(x)),
                SlotValue::Empty => Ok(None),
                _ => Err(err("bool slot")),
            }
        };
        Ok(bm::AggTrade {
            event_type: text(0)?,
            event_time_ms: int(1)?,
            symbol: text(2)?,
            aggregate_trade_id: int(3)?,
            price: dec(4)?,
            quantity: dec(5)?,
            first_trade_id: int(6)?,
            last_trade_id: int(7)?,
            trade_time_ms: int(8)?,
            buyer_is_maker: flag(9)?.ok_or_else(|| err("m missing"))?,
            best_match: flag(10)?,
        })
    }
}

/// Slot readers shared by the typed VM routes below.
mod slotvals {
    use super::{err, R};
    use jsonmax::SlotValue;
    pub fn text(raw: &[u8], decoded: &[u8], v: SlotValue) -> R<String> {
        let SlotValue::Str(r) = v else { return Err(err("string slot")) };
        let base = match r.source {
            jmax_abi::span::SpanSource::RawPayload => raw,
            jmax_abi::span::SpanSource::DecodedArena => decoded,
        };
        Ok(std::str::from_utf8(r.span.slice(base).ok_or_else(|| err("span"))?).map_err(err)?.to_owned())
    }
    pub fn int(v: SlotValue) -> R<i64> {
        match v {
            SlotValue::I64(x) => Ok(x),
            SlotValue::U64(x) => i64::try_from(x).map_err(err),
            _ => Err(err("int slot")),
        }
    }
    pub fn dec(v: SlotValue) -> R<jmax_native::Decimal128> {
        match v {
            SlotValue::Decimal(d) => Ok(d),
            _ => Err(err("decimal slot")),
        }
    }
    pub fn flag(v: SlotValue) -> R<Option<bool>> {
        match v {
            SlotValue::Bool(x) => Ok(Some(x)),
            SlotValue::Empty => Ok(None),
            _ => Err(err("bool slot")),
        }
    }
}

fn spot_decimal() -> jmax_schema_ir::ScalarConstraints {
    let mut c = jmax_schema_ir::ScalarConstraints::new(jmax_schema_ir::ValueKind::Decimal);
    c.decimal = Some(jmax_schema_ir::DecimalConstraints {
        policy: jmax_abi::DecimalPolicy::spot_8(),
        minimum: None,
        maximum: None,
    });
    c
}

/// Append a scalar node and return a property that refers to it.
fn scalar_property(
    nodes: &mut Vec<jmax_schema_ir::TypeNode>,
    field: u32,
    name: &str,
    constraints: jmax_schema_ir::ScalarConstraints,
    required: bool,
    slot: u32,
) -> jmax_schema_ir::Property {
    nodes.push(jmax_schema_ir::TypeNode::Scalar(constraints));
    jmax_schema_ir::Property {
        field_id: jsonmax::FieldId(field),
        name: name.into(),
        ty: jmax_schema_ir::TypeId(nodes.len() as u32 - 1),
        required,
        slot: Some(slot),
    }
}

/// A fixed `[price, quantity]` level array inside an open-ended list: appends
/// the list and level nodes and returns the list node's id.
fn levels_node(nodes: &mut Vec<jmax_schema_ir::TypeNode>) -> jmax_schema_ir::TypeId {
    use jmax_schema_ir::{ArrayShape, TypeId, TypeNode};
    let list = nodes.len() as u32;
    nodes.push(TypeNode::Array(ArrayShape { nullable: false, items: TypeId(list + 1), min_items: None, max_items: None }));
    nodes.push(TypeNode::Array(ArrayShape { nullable: false, items: TypeId(list + 2), min_items: Some(2), max_items: Some(2) }));
    nodes.push(TypeNode::Scalar(spot_decimal()));
    TypeId(list)
}

fn root_object(properties: Vec<jmax_schema_ir::Property>) -> jmax_schema_ir::TypeNode {
    jmax_schema_ir::TypeNode::Object(jmax_schema_ir::ObjectShape {
        nullable: false,
        properties,
        reject_unknown: true,
        min_properties: None,
        max_properties: None,
    })
}

fn compile_rows(name: &str, id: u32, nodes: Vec<jmax_schema_ir::TypeNode>) -> R<jsonmax::CompiledSchema> {
    jsonmax::endpoint::compile_native_schema(
        &jmax_schema_ir::Schema { name: name.into(), schema_id: jsonmax::SchemaId(id), root: jmax_schema_ir::TypeId(0), nodes, definitions: vec![] },
        jsonmax::CompileLimits::default_limits(),
    )
    .map_err(err)
}

fn level_boxes(block: &jmax_schema_vm::RowBlock) -> R<Vec<Box<bm::PriceLevel>>> {
    let mut out = Vec::with_capacity(block.len());
    for row in block.rows() {
        out.push(Box::new(bm::PriceLevel { price: slotvals::dec(row[0])?, quantity: slotvals::dec(row[1])? }));
    }
    Ok(out)
}

fn block_for(out: &jmax_schema_vm::BorrowedRowsRef<'_, '_>, slot: u32) -> R<usize> {
    out.groups.iter().position(|g| g.array_slot == slot).ok_or_else(|| err("missing row group"))
}

/// The pure schema-VM route for `{"e".."b":[[p,q]..],"a":[[p,q]..]}`: one VM
/// pass validates the document, fills the scalar slots and stages every
/// price level of both books as a row; the struct is built from those.
pub struct DepthUpdateVm {
    schema: jsonmax::CompiledSchema,
    selection: jsonmax::Selection,
    scratch: jsonmax::endpoint::ValidationScratch,
}
impl DepthUpdateVm {
    pub fn new() -> R<Self> {
        use jmax_schema_ir::{ScalarConstraints as C, ValueKind as K};
        let mut nodes = vec![];
        nodes.push(root_object(vec![]));
        let mut props = vec![
            scalar_property(&mut nodes, 1, "e", C::new(K::String), true, 0),
            scalar_property(&mut nodes, 2, "E", C::new(K::Integer), true, 1),
            scalar_property(&mut nodes, 3, "s", C::new(K::String), true, 2),
            scalar_property(&mut nodes, 4, "U", C::new(K::Integer), true, 3),
            scalar_property(&mut nodes, 5, "u", C::new(K::Integer), true, 4),
        ];
        let book = levels_node(&mut nodes);
        for (field, name, slot) in [(6, "b", 5), (7, "a", 6)] {
            props.push(jmax_schema_ir::Property { field_id: jsonmax::FieldId(field), name: name.into(), ty: book, required: true, slot: Some(slot) });
        }
        nodes[0] = root_object(props);
        let schema = compile_rows("depthUpdate", 2, nodes)?;
        let selection = jsonmax::Selection { field_ids: vec![], strict_validate_unselected: true };
        Ok(Self { schema, selection, scratch: Default::default() })
    }
    pub fn decode(&mut self, bytes: &[u8]) -> R<bm::DepthUpdate> {
        let out = jsonmax::endpoint::extract_rows_borrowed_with_scratch(&self.schema, &self.selection, bytes, common::options(), &mut self.scratch).map_err(err)?;
        let (b, a) = (block_for(&out, 5)?, block_for(&out, 6)?);
        Ok(bm::DepthUpdate {
            event_type: slotvals::text(out.raw, out.decoded, out.slots[0])?,
            event_time_ms: slotvals::int(out.slots[1])?,
            symbol: slotvals::text(out.raw, out.decoded, out.slots[2])?,
            first_update_id: slotvals::int(out.slots[3])?,
            final_update_id: slotvals::int(out.slots[4])?,
            bids: level_boxes(&out.groups[b])?,
            asks: level_boxes(&out.groups[a])?,
        })
    }
}

/// The pure schema-VM route for the order-book snapshot.
pub struct DepthSnapshotVm {
    schema: jsonmax::CompiledSchema,
    selection: jsonmax::Selection,
    scratch: jsonmax::endpoint::ValidationScratch,
}
impl DepthSnapshotVm {
    pub fn new() -> R<Self> {
        use jmax_schema_ir::{ScalarConstraints as C, ValueKind as K};
        let mut nodes = vec![root_object(vec![])];
        let mut props = vec![scalar_property(&mut nodes, 1, "lastUpdateId", C::new(K::Integer), true, 0)];
        let book = levels_node(&mut nodes);
        for (field, name, slot) in [(2, "bids", 1), (3, "asks", 2)] {
            props.push(jmax_schema_ir::Property { field_id: jsonmax::FieldId(field), name: name.into(), ty: book, required: true, slot: Some(slot) });
        }
        nodes[0] = root_object(props);
        let schema = compile_rows("depthSnapshot", 3, nodes)?;
        let selection = jsonmax::Selection { field_ids: vec![], strict_validate_unselected: true };
        Ok(Self { schema, selection, scratch: Default::default() })
    }
    pub fn decode(&mut self, bytes: &[u8]) -> R<bm::DepthSnapshot> {
        let out = jsonmax::endpoint::extract_rows_borrowed_with_scratch(&self.schema, &self.selection, bytes, common::options(), &mut self.scratch).map_err(err)?;
        let (b, a) = (block_for(&out, 1)?, block_for(&out, 2)?);
        Ok(bm::DepthSnapshot {
            last_update_id: slotvals::int(out.slots[0])?,
            bids: level_boxes(&out.groups[b])?,
            asks: level_boxes(&out.groups[a])?,
        })
    }
}

/// The pure schema-VM route for `{"trades":[aggTrade, ...]}`: the VM stages
/// one eleven-column row per trade.
pub struct TradesVm {
    schema: jsonmax::CompiledSchema,
    selection: jsonmax::Selection,
    scratch: jsonmax::endpoint::ValidationScratch,
}
impl TradesVm {
    pub fn new() -> R<Self> {
        use jmax_schema_ir::{ArrayShape, Property, TypeId, TypeNode, ScalarConstraints as C, ValueKind as K};
        let mut nodes = vec![root_object(vec![])];
        // Node 1 is the trades list, node 2 its element object.
        nodes.push(TypeNode::Array(ArrayShape { nullable: false, items: TypeId(2), min_items: None, max_items: None }));
        nodes.push(root_object(vec![]));
        let fields: [(&str, C, bool); 11] = [
            ("e", C::new(K::String), true),
            ("E", C::new(K::Integer), true),
            ("s", C::new(K::String), true),
            ("a", C::new(K::Integer), true),
            ("p", spot_decimal(), true),
            ("q", spot_decimal(), true),
            ("f", C::new(K::Integer), true),
            ("l", C::new(K::Integer), true),
            ("T", C::new(K::Integer), true),
            ("m", C::new(K::Boolean), true),
            ("M", C::new(K::Boolean), false),
        ];
        let mut item = Vec::new();
        for (i, (name, constraints, required)) in fields.into_iter().enumerate() {
            item.push(scalar_property(&mut nodes, 2 + i as u32, name, constraints, required, 1 + i as u32));
        }
        nodes[2] = root_object(item);
        nodes[0] = root_object(vec![Property { field_id: jsonmax::FieldId(1), name: "trades".into(), ty: TypeId(1), required: true, slot: Some(0) }]);
        let schema = compile_rows("trades", 4, nodes)?;
        let selection = jsonmax::Selection { field_ids: vec![], strict_validate_unselected: true };
        Ok(Self { schema, selection, scratch: Default::default() })
    }
    pub fn decode(&mut self, bytes: &[u8]) -> R<trades::models::TradeBatch> {
        let out = jsonmax::endpoint::extract_rows_borrowed_with_scratch(&self.schema, &self.selection, bytes, common::options(), &mut self.scratch).map_err(err)?;
        let block = &out.groups[block_for(&out, 0)?];
        let mut trades = Vec::with_capacity(block.len());
        for r in block.rows() {
            trades.push(Box::new(trades::models::AggTrade {
                event_type: slotvals::text(out.raw, out.decoded, r[0])?,
                event_time_ms: slotvals::int(r[1])?,
                symbol: slotvals::text(out.raw, out.decoded, r[2])?,
                aggregate_trade_id: slotvals::int(r[3])?,
                price: slotvals::dec(r[4])?,
                quantity: slotvals::dec(r[5])?,
                first_trade_id: slotvals::int(r[6])?,
                last_trade_id: slotvals::int(r[7])?,
                trade_time_ms: slotvals::int(r[8])?,
                buyer_is_maker: slotvals::flag(r[9])?.ok_or_else(|| err("m missing"))?,
                best_match: slotvals::flag(r[10])?,
            }));
        }
        Ok(trades::models::TradeBatch { trades })
    }
}

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
    scratch: jmax_schema_vm::ValidationScratch,
    _marker: std::marker::PhantomData<T>,
}
impl<T: NativeDecode> VmFirst<T> {
    pub fn new() -> R<Self> {
        let program = T::build_program().map_err(err)?;
        let limits = VmLimits::default();
        let ctx = NativeContext::with_program(&program, limits, NUMERIC()).map_err(err)?;
        Ok(Self { program, ctx, limits, scratch: jmax_schema_vm::ValidationScratch::default(), _marker: std::marker::PhantomData })
    }
    pub fn decode(&mut self, input: &[u8], out: &mut T) -> R<()> {
        jmax_schema_vm::direct::validate_borrowed_into(
            &self.program,
            input,
            self.limits,
            NUMERIC(),
            &mut self.scratch,
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
    let mut vm_native = None;
    if diag() {
        let vm_before = alloc_metrics::live();
        let mut vm = VmFirst::<C::Native>::new()?;
        let mut again = C::Native::default();
        vm.decode(bytes, &mut again)?;
        assert_eq!(C::sum_native(&again), want, "{}: vm+native", C::NAME);
        drop(again);
        VM_RETAINED.store(alloc_metrics::live().saturating_sub(vm_before), std::sync::atomic::Ordering::Relaxed);
        vm_native = Some(vm);
    }
    let vm_row = C::vm()?;
    let mut vm_row = match vm_row {
        Some(mut f) => {
            assert_eq!(C::sum_native(&f(bytes)?), want, "{}: jmax vm", C::NAME);
            Some(f)
        }
        None => None,
    };


    let mut ops: Vec<(&'static str, Op<'a>)> = Vec::new();
    {
        let mut ctx = native_context(VmLimits::default());
        let mut out = C::Native::default();
        ops.push((NATIVE_ROW, Box::new(move || Ok(black_box(decode_into(&mut ctx, black_box(bytes), &mut out).map_err(err)?)))));
    }
    if let Some(mut f) = vm_row.take() {
        ops.push((VM_ROW, Box::new(move || { black_box(f(black_box(bytes))?); Ok(()) })));
    }
    if diag() {
        let mut ctx = native_context(roomy_limits());
        let mut out = C::Native::default();
        ops.push((BUDGET_DIAG_ROW, Box::new(move || Ok(black_box(decode_into(&mut ctx, black_box(bytes), &mut out).map_err(err)?)))));
        let mut vm = vm_native.take().expect("diag vm");
        let mut out = C::Native::default();
        ops.push((VM_NATIVE_DIAG_ROW, Box::new(move || { vm.decode(black_box(bytes), &mut out)?; black_box(&out); Ok(()) })));
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
    // Query selection is a separate workload from typed native decoding.
    let path = if C::NAME == "depth_1000" { "$.lastUpdateId" } else if C::NAME == "trades_100" { "$.trades[0].s" } else { "$.s" };
    let pointer = if C::NAME == "depth_1000" { "/lastUpdateId" } else if C::NAME == "trades_100" { "/trades/0/s" } else { "/s" };
    ops.extend(query_rows(bytes, path, pointer)?);
    ops.push(("jmax validate", Box::new(move || { black_box(common::walk(black_box(bytes))?); Ok(()) })));
    ops.push(("jmax dom-owned", Box::new(move || { black_box(jsonmax::parse::parse_slice(black_box(bytes), common::options())?); Ok(()) })));
    ops.push(("jmax dom-borrowed", Box::new(move || { black_box(jsonmax::parse::parse_borrowed(black_box(bytes), common::options())?); Ok(()) })));
    let doc = jsonmax::parse::parse_slice(bytes, common::options())?;
    ops.push(("jmax serialize", Box::new(move || { black_box(jsonmax::encode::document_to_vec(&doc, Default::default())?); Ok(()) })));
    ops.push(("jiter validate", Box::new(move || { let mut it = jiter::Jiter::new(black_box(bytes)); it.next_skip().map_err(err)?; it.finish().map_err(err)?; Ok(()) })));
    ops.push(("jiter dom-owned", Box::new(move || { black_box(jiter::JsonValue::parse(black_box(bytes), false).map_err(err)?.into_static()); Ok(()) })));
    ops.push(("jiter dom-borrowed", Box::new(move || { black_box(jiter::JsonValue::parse(black_box(bytes), false).map_err(err)?); Ok(()) })));
    ops.push(("serde_json validate", Box::new(move || { black_box(serde_json::from_slice::<serde::de::IgnoredAny>(black_box(bytes))?); Ok(()) })));
    ops.push(("sonic-rs validate", Box::new(move || { black_box(sonic_rs::from_slice::<serde::de::IgnoredAny>(black_box(bytes)).map_err(err)?); Ok(()) })));
    ops.push(("serde_json dom-owned", Box::new(move || { black_box(serde_json::from_slice::<serde_json::Value>(black_box(bytes))?); Ok(()) })));
    ops.push(("sonic-rs dom-owned", Box::new(move || { black_box(sonic_rs::from_slice::<sonic_rs::Value>(black_box(bytes)).map_err(err)?); Ok(()) })));
    ops.push(("simd-json dom-owned", Box::new({ let mut buf = bytes.to_vec(); move || { buf.copy_from_slice(bytes); black_box(simd_json::to_owned_value(black_box(&mut buf)).map_err(err)?); Ok(()) } })));
    ops.push(("simd-json dom-borrowed", Box::new({ let mut buf = bytes.to_vec(); move || { buf.copy_from_slice(bytes); black_box(simd_json::to_borrowed_value(black_box(&mut buf)).map_err(err)?); Ok(()) } })));
    ops.push(("simd-json tape", Box::new({ let mut buf = bytes.to_vec(); move || { buf.copy_from_slice(bytes); black_box(simd_json::to_tape(black_box(&mut buf)).map_err(err)?); Ok(()) } })));
    let reference: serde_json::Value = serde_json::from_slice(bytes)?;
    ops.push(("serde_json serialize", Box::new(move || { black_box(serde_json::to_vec(black_box(&reference))?); Ok(()) })));
    let sonic_value: sonic_rs::Value = sonic_rs::from_slice(bytes).map_err(err)?;
    ops.push(("sonic-rs serialize", Box::new(move || { black_box(sonic_rs::to_vec(black_box(&sonic_value)).map_err(err)?); Ok(()) })));
    let mut simd_buf = bytes.to_vec();
    let simd_value = simd_json::to_owned_value(&mut simd_buf).map_err(err)?;
    ops.push(("simd-json serialize", Box::new(move || { black_box(simd_json::to_vec(black_box(&simd_value)).map_err(err)?); Ok(()) })));
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

/// Query-only/strict-extract work, checked against a shared serde pointer.
pub fn query_rows<'a>(bytes: &'a [u8], path: &'static str, pointer: &str) -> R<Vec<(&'static str, Op<'a>)>> {
    use std::hint::black_box;
    let mut ops: Vec<(&'static str, Op<'a>)> = Vec::new();
    let reference: serde_json::Value = serde_json::from_slice(bytes)?;
    let expected = reference.pointer(pointer).expect("native query field");
    for (name, training) in [("ps-pikkr untrained query-only", usize::MAX), ("ps-pikkr trained query-only", 1)] {
        let mut check = pikkr::Pikkr::new(&[path], training).map_err(err)?;
        for _ in 0..3 {
            let result = check.parse(bytes).map_err(err)?;
            assert_eq!(&serde_json::from_slice::<serde_json::Value>(result[0].expect("native pikkr result"))?, expected);
        }
        let mut parser = None;
        let mut measured_retained = 0usize;
        ops.push((name, Box::new(move || {
            let before = alloc_metrics::live();
            if parser.is_none() { parser = Some(pikkr::Pikkr::new(&[path], training).map_err(err)?); }
            let parser = parser.as_mut().unwrap();
            black_box(parser.parse(black_box(bytes)).map_err(err)?);
            let after = alloc_metrics::live();
            if after >= before { measured_retained += after - before; } else { measured_retained -= before - after; }
            QUERY_RETAINED.store(parser.retained_heap_bytes().max(measured_retained), std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })));
    }
    let check_query = jsonmax::query::compile_paths(&[path.trim_start_matches("$.").trim_start_matches('$')])?;
    let check_result = jsonmax::query::execute_borrowed(&check_query, bytes, jsonmax::QueryOptions::default())?;
    assert_eq!(&serde_json::from_slice::<serde_json::Value>(check_result.raw_match(0, 0).expect("native jmax query result"))?, expected);
    for (name, mode) in [("jmax query-only", jsonmax::QueryMode::Query), ("jmax strict-extract", jsonmax::QueryMode::Strict)] {
        // Construct the query on the first warmup so its retained index is counted.
        let mut query = None;
        ops.push((name, Box::new(move || {
            if query.is_none() { query = Some(jsonmax::query::compile_paths(&[path.trim_start_matches("$.").trim_start_matches('$')])?); }
            black_box(jsonmax::query::execute_borrowed(query.as_ref().unwrap(), black_box(bytes), jsonmax::QueryOptions { mode, ..Default::default() })?); Ok(())
        })));
    }
    Ok(ops)
}
