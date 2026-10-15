# jmax-exchange: frozen Wave 0 contract

Status: W0-ROOTS-7. All spans, slots, decimals, errors, limits, schema IR, proofs, and parsed-message owners use the frozen `jmax-abi` definitions. Each child imports `super::*`; no shared definition is owned by a child. This crate supports alloc without std. The root permits narrowly audited unsafe calls to `ParsedMessage::new_proven`; it does not permit unchecked span dereferences. Later waves own the four declared module files.

## Shared types

The complete root snapshot below specifies every struct, enum, trait, method, alias, constant, derive, and field visibility. An ExchangeProfile is an immutable configuration value. Each decode operation validates the profile before inspecting payload data. BookLevel stores exact Decimal128 values, never binary floating point. A zero quantity is meaningful for depth deletions and must be retained. All timestamps and sequence/trade IDs are u64; no signed or floating coercions are permitted.

ExchangeValidator must strictly validate the complete input, including unselected fields and any wrapper, and return exactly the frozen kind-specific slots described below. Its proof fingerprint must equal `profiles::schema_fingerprint(kind, profile)`. With a wrapper, slot spans remain relative to the complete original payload and its decoded arena. The decode entrypoints compare original bytes and proof lengths through `ensure_slot_owner`; they also check expected fingerprint, slot count/types, resolved spans, literal event tags, sequence relationships, and book/decimal constraints before returning an owner. A validator adapter cannot rely solely on the shallow ABI SchemaIr to validate book arrays.

Any invocation of `ParsedMessage::new_proven` must preserve the returned payload/decoded arena intact, validate every emitted StrRef using checked bounds and UTF-8, check scalar_len against decoded Unicode scalar count, and prove that the typed values contain no reference to scratch or an inner temporary payload. ValidationProof comes from the validator and is never issued by these modules. Any additional semantic failure returns an error before owner publication. Safe code must not assume an arbitrary caller-supplied ValidatedSlots is authentic merely because it has a proof field.

## Wave 1: depth.rs

```rust
pub const SLOT_EVENT: usize = 0;
pub const SLOT_EVENT_TIME: usize = 1;
pub const SLOT_SYMBOL: usize = 2;
pub const SLOT_FIRST_UPDATE_ID: usize = 3;
pub const SLOT_FINAL_UPDATE_ID: usize = 4;
pub const SLOT_BIDS: usize = 5;
pub const SLOT_ASKS: usize = 6;
pub const SLOT_COUNT: usize = 7;
pub fn schema_ir() -> SchemaIr;
pub fn decode_depth(payload: Payload, profile: &ExchangeProfile, validator: &mut dyn ExchangeValidator) -> Result<DepthMessage>;
```

The schema name is `spot_depth_v1`, schema_id DEPTH_SCHEMA_ID. Field IDs are slot index + 1. Required fields in slot order are `e`, `E`, `s`, `U`, `u`, `b`, `a`. Slots are respectively Str, U64, Str, U64, U64, RawSpan, RawSpan. `e` must resolve to `depthUpdate`. `s` is a nonempty UTF-8 string bounded by profile.max_symbol_bytes; retain its StrRef. `U <= u` is mandatory. No requirement that an event arrives contiguously is imposed by decoding.

`schema_ir` has required_count 7 and reject_unknown true. String fields use ScalarKind::String, timestamps/IDs use ScalarKind::Number, and book fields have scalar_kind None. All format and integer-bound options are None; string `s` has min_len Some(1) and max_len Some(MAX_SYMBOL_BYTES). The IR describes the default profile. A validator must apply the supplied profile's actual symbol limit instead of reusing this default bound blindly. Each `b` and `a` value must be an array of exactly two-string arrays `[price, quantity]`; validate every element, even if no book data is selected by an adapter. Each side independently respects max_levels and syntax max_array_items. Prices must be positive, quantities nonnegative. Preserve source level order and duplicates; a decoder is not an order-book merger. DepthValues contains the required metadata and fully owned decimal vectors.

## Wave 2: trade.rs

```rust
pub const SLOT_EVENT: usize = 0;
pub const SLOT_EVENT_TIME: usize = 1;
pub const SLOT_SYMBOL: usize = 2;
pub const SLOT_AGGREGATE_TRADE_ID: usize = 3;
pub const SLOT_PRICE: usize = 4;
pub const SLOT_QUANTITY: usize = 5;
pub const SLOT_FIRST_TRADE_ID: usize = 6;
pub const SLOT_LAST_TRADE_ID: usize = 7;
pub const SLOT_TRADE_TIME: usize = 8;
pub const SLOT_BUYER_IS_MAKER: usize = 9;
pub const SLOT_BEST_MATCH: usize = 10;
pub const SLOT_COUNT: usize = 11;
pub fn schema_ir() -> SchemaIr;
pub fn decode_trade(payload: Payload, profile: &ExchangeProfile, validator: &mut dyn ExchangeValidator) -> Result<TradeMessage>;
```

Schema name `spot_agg_trade_v1`, schema_id TRADE_SCHEMA_ID. Field IDs are slot index + 1. Keys are `e`, `E`, `s`, `a`, `p`, `q`, `f`, `l`, `T`, `m`, `M`. The first ten are required, `M` is optional. Slot kinds are Str, U64, Str, U64, Str, Str, U64, U64, U64, Bool, then Bool or Empty. Optional absence is Empty, never an omitted slot. Literal event must be `aggTrade`. `f <= l` is mandatory. Prices and trade quantities must both be strictly positive. `m` is not inverted: true means the buyer is the maker. `M` is retained as best_match without synthesizing a default.

SchemaIr required_count is 10, reject_unknown true, and scalar kinds follow the slot types, except boolean fields use None because the ABI distinguishes True and False literal kinds and has no union kind. The ExchangeValidator adapter must enforce that each such value is either True or False; it must not treat None as permission to accept another JSON type. All number bounds and format options are None. Symbol min/max follow depth. Decimal strings are parsed exactly under price/quantity policies after slot extraction; they are not f64 values and not raw JSON number tokens.

## Wave 3: snapshot.rs

```rust
pub const SLOT_LAST_UPDATE_ID: usize = 0;
pub const SLOT_BIDS: usize = 1;
pub const SLOT_ASKS: usize = 2;
pub const SLOT_COUNT: usize = 3;
pub fn schema_ir() -> SchemaIr;
pub fn decode_snapshot(payload: Payload, profile: &ExchangeProfile, validator: &mut dyn ExchangeValidator) -> Result<SnapshotMessage>;
pub fn unwrap_payload(payload: Payload, policy: WrapperPolicy, limits: Limits) -> Result<UnwrappedPayload>;
```

Schema name `spot_rest_depth_v1`, schema_id SNAPSHOT_SCHEMA_ID, required_count 3, reject_unknown true. Field IDs 1..=3 correspond to `lastUpdateId` U64, `bids` RawSpan, `asks` RawSpan. The first field uses ScalarKind::Number; the arrays use None. All optional SchemaFieldIr constraints are None. Level validation and decimal policies are identical to depth, including permitting zero quantities.

Bare means a direct kind-specific object. CombinedStream means exactly `{ "stream": <nonempty string>, "data": <kind-specific object> }` with no duplicate or additional wrapper keys. Either accepts those two alternatives without ambiguous fallback after a malformed wrapper is recognized. Wrapper depth, stream bytes, decoded bytes, unknown keys, and trailing bytes all respect Limits. A bare document has no synthesized stream name. `unwrap_payload` fully validates JSON structure, copies only the validated `data` object's lexical bytes into a new Payload for a wrapper, and returns a decoded owned stream String. A bare payload is returned without copying. This utility returns unvalidated exchange input, not a proof or typed message. Typed decoders must pass the complete original payload to ExchangeValidator and keep original span coordinates; they must not use the utility to rebase existing slots. Callers wanting a separate inner owner must call decode again with a Bare profile.

## Wave 4: profiles.rs

```rust
pub fn spot_profile() -> ExchangeProfile;
pub fn validate_profile(profile: &ExchangeProfile) -> Result<()>;
pub fn schema_fingerprint(kind: MessageKind, profile: &ExchangeProfile) -> Result<u64>;
pub fn validate_level(level: BookLevel, profile: &ExchangeProfile) -> Result<()>;
pub fn seed_snapshot(state: &mut SequenceState, last_update_id: u64) -> Result<()>;
pub fn classify_depth(state: &SequenceState, first_update_id: u64, final_update_id: u64) -> Result<SequenceDecision>;
pub fn apply_depth(state: &mut SequenceState, first_update_id: u64, final_update_id: u64) -> Result<SequenceDecision>;
```

spot_profile returns ExchangeProfile::default(). validate_profile accepts only version 1; unknown versions are UnsupportedSchemaFeature. max_levels, max_symbol_bytes, limits.max_payload_bytes, and limits.max_depth must be nonzero. Both decimal policies require max_precision in 1..=38, target_scale <= 38, and tick_step >= 0. Tick step zero disables the multiple check. Decimal exponent input is accepted only when allow_exponent is true. Exact fixed-point conversion must use checked i128 accumulation/rescaling, enforce precision, preserve target scale, and enforce tick_step. reject_inexact_rescale false permits truncation toward zero, never floating conversion or rounding to nearest. Negative zero normalizes to zero. Source numeric strings use strict decimal grammar with optional leading minus, at least one integer digit, optional nonempty fractional digits, and optional exponent only when enabled. Leading plus and leading integer zeros other than a single zero are invalid.

validate_level enforces the price and quantity policies on already converted values, requires exact target scales, price > 0, quantity >= 0, and tick multiples. Trade decoding additionally requires quantity > 0. Decimal failures use the canonical InvalidNumber, NumberOverflow, DecimalPrecisionOverflow, DecimalScaleMismatch, and TickMultipleViolation codes. Semantic positivity is ConstraintViolation.

schema_fingerprint is a stable FNV-1a-64 identity, not a security digest: offset 14695981039346656037, prime 1099511628211, wrapping arithmetic. Feed ASCII `jmax-exchange-v1` without NUL, kind byte Depth=0/AggTrade=1/Snapshot=2, profile.version as little-endian u16, then price and quantity policies in order (max_precision u8, target_scale u8, allow_exponent bool byte, reject_inexact_rescale bool byte, tick_step little-endian i128), max_levels u32, max_symbol_bytes u32, wrapper byte Bare=0/CombinedStream=1/Either=2. Finally feed every Limits field in ABI declaration order and little-endian width, bools as 0/1. Version 1 fixes the exact slot layout and literal/semantic constraints above; changing them requires a profile version change.

seed_snapshot rejects u64::MAX with NumberOverflow and otherwise sets last_update_id and synchronized true. Classification requires a seeded synchronized state (otherwise ConstraintViolation), validates first <= final, then returns Stale if final <= last; otherwise checked expected=last+1 and Apply when first <= expected <= final, or Gap when first > expected. Apply carries `next_update_id = final + 1`, computed with overflow checks. apply_depth commits last_update_id=final only on Apply; Stale leaves all state unchanged; Gap retains last_update_id but sets synchronized false, requiring a fresh snapshot. Errors never partially mutate state. Sequence tracking does not reorder, buffer, merge books, infer missing updates, or authenticate exchange data.

## Error and module invariants

All module-generated errors use ErrorSource::Decode, with checked u32 offsets where available and schema/field IDs attached when known. Validator, cursor, and decimal errors retain their canonical source and offset. Missing slots/keys use MissingRequiredField; wrong slot/token kinds TypeMismatch; extra keys UnknownObjectKey; duplicates DuplicateObjectKey; malformed wrappers InvalidSyntax or TypeMismatch; invalid ranges/versions use the codes specified above. Oversized arrays use ConstraintViolation, byte budgets PayloadTooLarge/DecodedArenaOverflow, and fallible allocations BudgetExhausted. No decoder returns a partially populated message. Strict JSON grammar and UTF-8 checks cover every byte even when only metadata is desired.

Depth and trade may call snapshot::unwrap_payload only as an explicit utility before a separate decoding operation; typed decode paths share profiles, but profiles imports no child parser. Snapshot may share book parsing through crate-private depth helpers; those helpers must not own shared public types. None of the modules can redefine the ABI or add dependencies. The root snapshot is the full shared definition set.

## Frozen root snapshot

```rust
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
```
