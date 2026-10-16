//! Exact-decimal Spot market-data profiles and owner-bound decoded messages.
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::{ParsedMessage, Payload},
    error::{Error, ErrorCode, ErrorSource, Result},
    number::{Decimal128, DecimalPolicy},
    schema::{FieldId, SchemaFieldIr, SchemaId, SchemaIr, SlotValue, ValidatedSlots},
    span::{Span32, SpanSource, StrRef},
    syntax::{Limits, ScalarKind},
};

pub mod depth;
pub mod trade;
pub mod snapshot;
pub mod profiles;

pub const SPOT_PROFILE_VERSION: u16 = 1;
pub const DEPTH_SCHEMA_ID: SchemaId = SchemaId(3201);
pub const TRADE_SCHEMA_ID: SchemaId = SchemaId(3202);
pub const SNAPSHOT_SCHEMA_ID: SchemaId = SchemaId(3203);
pub const DEFAULT_MAX_LEVELS: u32 = 5000;
pub const MAX_SYMBOL_BYTES: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind { Depth, AggTrade, Snapshot }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapperPolicy { Bare, CombinedStream, Either }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExchangeProfile {
    pub version: u16,
    pub price: DecimalPolicy,
    pub quantity: DecimalPolicy,
    pub max_levels: u32,
    pub max_symbol_bytes: u32,
    pub wrapper: WrapperPolicy,
    pub limits: Limits,
}

impl Default for ExchangeProfile {
    fn default() -> Self {
        Self {
            version: SPOT_PROFILE_VERSION,
            price: DecimalPolicy::spot_8(),
            quantity: DecimalPolicy::spot_8(),
            max_levels: DEFAULT_MAX_LEVELS,
            max_symbol_bytes: MAX_SYMBOL_BYTES,
            wrapper: WrapperPolicy::Bare,
            limits: Limits::strict_default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookLevel { pub price: Decimal128, pub quantity: Decimal128 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepthValues {
    pub event_time_ms: u64,
    pub symbol: StrRef,
    pub first_update_id: u64,
    pub final_update_id: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeValues {
    pub event_time_ms: u64,
    pub symbol: StrRef,
    pub aggregate_trade_id: u64,
    pub price: Decimal128,
    pub quantity: Decimal128,
    pub first_trade_id: u64,
    pub last_trade_id: u64,
    pub trade_time_ms: u64,
    pub buyer_is_maker: bool,
    pub best_match: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotValues {
    pub last_update_id: u64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

pub type DepthMessage = ParsedMessage<DepthValues>;
pub type TradeMessage = ParsedMessage<TradeValues>;
pub type SnapshotMessage = ParsedMessage<SnapshotValues>;

#[derive(Clone, Debug)]
pub struct UnwrappedPayload {
    pub payload: Payload,
    pub stream: Option<String>,
}

/// Must apply the kind's frozen slot layout and validate the whole input.
/// Slots refer to the original complete payload, even for a combined wrapper.
pub trait ExchangeValidator {
    fn validate(&mut self, kind: MessageKind, payload: Payload, profile: &ExchangeProfile) -> Result<ValidatedSlots>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceDecision { Stale, Apply { next_update_id: u64 }, Gap { expected: u64, received: u64 } }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequenceState {
    pub(crate) last_update_id: Option<u64>,
    pub(crate) synchronized: bool,
}

impl SequenceState {
    pub fn new() -> Self { Self::default() }
    pub fn last_update_id(&self) -> Option<u64> { self.last_update_id }
    pub fn is_synchronized(&self) -> bool { self.synchronized }
    pub fn reset(&mut self) { *self = Self::default(); }
}

pub(crate) fn exchange_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}

/// This also catches adapters accidentally validating a copied inner document.
pub(crate) fn ensure_slot_owner(slots: &ValidatedSlots, payload: &[u8]) -> Result<()> {
    if slots.payload.as_slice() != payload
        || usize::try_from(slots.proof.payload_len()).ok() != Some(payload.len())
        || usize::try_from(slots.proof.decoded_len()).ok() != Some(slots.decoded.len())
    {
        return Err(exchange_error(ErrorCode::InternalError, "exchange validator owner mismatch"));
    }
    Ok(())
}

pub(crate) fn required_slot(slots: &ValidatedSlots, index: usize) -> Result<SlotValue> {
    match slots.slots.get(index).copied() {
        Some(SlotValue::Empty) | None => Err(exchange_error(ErrorCode::MissingRequiredField, "missing exchange slot")),
        Some(value) => Ok(value),
    }
}
