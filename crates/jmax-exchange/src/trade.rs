//! Frozen Spot aggregate-trade schema and owner-bound, exact-decimal decoding.

use super::*;
use jmax_abi::syntax::{Event, ScanStep};
use jmax_cursor::{walk, Cursor, SkipMode};

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

const FIELD_NAMES: [&str; SLOT_COUNT] = ["e", "E", "s", "a", "p", "q", "f", "l", "T", "m", "M"];

/// Default-profile IR. Boolean unions, literal tags, unsigned integers and
/// decimal policies also require validation beyond the shallow ABI schema.
pub fn schema_ir() -> SchemaIr {
    let fields = FIELD_NAMES.iter().enumerate().map(|(index, name)| SchemaFieldIr {
        field_id: FieldId(index as u32 + 1),
        name: String::from(*name),
        required: index != SLOT_BEST_MATCH,
        scalar_kind: match index {
            SLOT_EVENT | SLOT_SYMBOL | SLOT_PRICE | SLOT_QUANTITY => Some(ScalarKind::String),
            SLOT_EVENT_TIME | SLOT_AGGREGATE_TRADE_ID | SLOT_FIRST_TRADE_ID
                | SLOT_LAST_TRADE_ID | SLOT_TRADE_TIME => Some(ScalarKind::Number),
            _ => None,
        },
        format: None,
        min_len: if index == SLOT_SYMBOL { Some(1) } else { None },
        max_len: if index == SLOT_SYMBOL { Some(MAX_SYMBOL_BYTES) } else { None },
        min_i64: None,
        max_i64: None,
    }).collect();
    SchemaIr {
        name: String::from("spot_agg_trade_v1"),
        schema_id: TRADE_SCHEMA_ID,
        required_count: 10,
        reject_unknown: true,
        fields,
    }
}

/// Decode against the supplied immutable profile, retaining the complete
/// original payload and validator arena, including combined-stream coordinates.
pub fn decode_trade(
    payload: Payload,
    profile: &ExchangeProfile,
    validator: &mut dyn ExchangeValidator,
) -> Result<TradeMessage> {
    profiles::validate_profile(profile)?;
    if payload.len() > profile.limits.max_payload_bytes as usize {
        return Err(trade_error(ErrorCode::PayloadTooLarge, 0, 0,
            "trade payload exceeds byte limit"));
    }
    let fingerprint = profiles::schema_fingerprint(MessageKind::AggTrade, profile)?;
    let slots = validator.validate(MessageKind::AggTrade, payload.clone(), profile)?;
    ensure_slot_owner(&slots, payload.as_slice())
        .map_err(|error| error.with_schema(TRADE_SCHEMA_ID.0, 0))?;
    if slots.proof.schema_fingerprint() != fingerprint {
        return Err(trade_error(ErrorCode::SchemaImageInvalid, 0, 0,
            "trade slot fingerprint differs from profile"));
    }
    if slots.decoded.len() > profile.limits.max_decoded_bytes as usize {
        return Err(trade_error(ErrorCode::DecodedArenaOverflow, 0, 0,
            "trade decoded arena exceeds byte limit"));
    }
    let owner = SlotOwner {
        raw: slots.payload.as_slice(), decoded: &slots.decoded, slots: &slots.slots,
    };
    owner.validate(profile)?;
    let values = read_document(slots.payload.clone(), &owner, profile)?;

    // SAFETY: The original validator payload, arena and proof move intact into
    // the owner. Proof lengths and fingerprint match. Every string slot has
    // checked bounds, UTF-8 and scalar count. Independent strict traversal has
    // validated the complete document and wrapper through EOF, bound each slot
    // to its wire value, and enforced required/optional membership and profile
    // semantics. Symbol retains its checked original StrRef. All other values
    // are owned scalars, with no cursor, scratch or temporary-owner references.
    Ok(unsafe {
        ParsedMessage::new_proven(slots.payload, slots.decoded, values, slots.proof)
    })
}

fn trade_error(code: ErrorCode, offset: u32, field: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, offset, detail).with_schema(TRADE_SCHEMA_ID.0, field)
}

/// A checked view of publicly mutable slot assertions, not a proof capability.
struct SlotOwner<'a> {
    raw: &'a [u8],
    decoded: &'a [u8],
    slots: &'a [SlotValue],
}

impl SlotOwner<'_> {
    fn required(&self, index: usize) -> Result<SlotValue> {
        match self.slots.get(index).copied() {
            None | Some(SlotValue::Empty) => Err(trade_error(
                ErrorCode::MissingRequiredField, 0, index as u32 + 1,
                "missing required trade slot")),
            Some(value) => Ok(value),
        }
    }

    fn string(&self, index: usize) -> Result<(StrRef, &str)> {
        let reference = match self.required(index)? {
            SlotValue::Str(reference) => reference,
            _ => return Err(trade_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "trade slot must be a string")),
        };
        let (storage, offset) = match reference.source {
            SpanSource::RawPayload => (self.raw, reference.span.offset),
            SpanSource::DecodedArena => (self.decoded, 0),
        };
        let bytes = reference.span.slice(storage).ok_or_else(|| trade_error(
            ErrorCode::InternalError, offset, index as u32 + 1,
            "trade string span is out of bounds"))?;
        let text = core::str::from_utf8(bytes).map_err(|_| trade_error(
            ErrorCode::InvalidUtf8, offset, index as u32 + 1,
            "trade string slot is not UTF-8"))?;
        if text.chars().count() != reference.scalar_len as usize {
            return Err(trade_error(ErrorCode::InternalError, offset, index as u32 + 1,
                "trade string scalar count is incorrect"));
        }
        Ok((reference, text))
    }

    fn integer(&self, index: usize) -> Result<u64> {
        match self.required(index)? {
            SlotValue::U64(value) => Ok(value),
            _ => Err(trade_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "trade slot must be an unsigned integer")),
        }
    }

    fn boolean(&self, index: usize) -> Result<bool> {
        match self.required(index)? {
            SlotValue::Bool(value) => Ok(value),
            _ => Err(trade_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "trade slot must be a boolean")),
        }
    }

    fn best_match(&self) -> Result<Option<bool>> {
        match self.slots.get(SLOT_BEST_MATCH) {
            Some(SlotValue::Empty) => Ok(None),
            Some(SlotValue::Bool(value)) => Ok(Some(*value)),
            None => Err(trade_error(ErrorCode::MissingRequiredField, 0, 11,
                "trade validator must retain the optional slot")),
            _ => Err(trade_error(ErrorCode::TypeMismatch, 0, 11,
                "trade best-match slot must be a boolean or empty")),
        }
    }

    fn validate(&self, profile: &ExchangeProfile) -> Result<()> {
        if self.slots.len() > SLOT_COUNT {
            return Err(trade_error(ErrorCode::TypeMismatch, 0, 0,
                "trade validator returned extra slots"));
        }
        if self.string(SLOT_EVENT)?.1 != "aggTrade" {
            return Err(trade_error(ErrorCode::ConstraintViolation, 0, 1,
                "trade event tag must be aggTrade"));
        }
        let (_, symbol) = self.string(SLOT_SYMBOL)?;
        if symbol.is_empty() || symbol.len() > profile.max_symbol_bytes as usize {
            return Err(trade_error(ErrorCode::ConstraintViolation, 0, 3,
                "trade symbol is empty or exceeds profile byte limit"));
        }
        self.string(SLOT_PRICE)?;
        self.string(SLOT_QUANTITY)?;
        self.integer(SLOT_EVENT_TIME)?;
        self.integer(SLOT_AGGREGATE_TRADE_ID)?;
        self.integer(SLOT_TRADE_TIME)?;
        if self.integer(SLOT_FIRST_TRADE_ID)? > self.integer(SLOT_LAST_TRADE_ID)? {
            return Err(trade_error(ErrorCode::ConstraintViolation, 0, 7,
                "trade first ID exceeds last ID"));
        }
        self.boolean(SLOT_BUYER_IS_MAKER)?;
        self.best_match()?;
        Ok(())
    }
}

fn next_event(cursor: &mut Cursor, field: u32) -> Result<Event> {
    match walk::next(cursor)? {
        ScanStep::Emit(event) => Ok(event),
        ScanStep::NeedMore | ScanStep::Finished => Err(trade_error(
            ErrorCode::UnexpectedEof, cursor.state().byte_offset, field,
            "expected another trade JSON event")),
    }
}

fn cursor_string(cursor: &Cursor, reference: StrRef, field: u32) -> Result<&str> {
    let text = cursor.resolve_str(reference).ok_or_else(|| trade_error(
        ErrorCode::InternalError, cursor.state().byte_offset, field,
        "cursor trade string does not resolve"))?;
    if text.chars().count() != reference.scalar_len as usize {
        return Err(trade_error(ErrorCode::InternalError, cursor.state().byte_offset,
            field, "cursor trade string scalar count is incorrect"));
    }
    Ok(text)
}

fn read_document(payload: Payload, owner: &SlotOwner<'_>, profile: &ExchangeProfile) -> Result<TradeValues> {
    let mut cursor = walk::from_payload(payload, profile.limits, SkipMode::Strict)?;
    if !matches!(next_event(&mut cursor, 0)?, Event::ObjectStart { .. }) {
        return Err(trade_error(ErrorCode::TypeMismatch, cursor.state().byte_offset, 0,
            "trade document must be an object"));
    }
    let first = next_event(&mut cursor, 0)?;
    let wrapped = match profile.wrapper {
        WrapperPolicy::Bare => false,
        WrapperPolicy::CombinedStream => true,
        WrapperPolicy::Either => match first {
            Event::Key { str_ref } => matches!(cursor_string(&cursor, str_ref, 0)?, "stream" | "data"),
            _ => false,
        },
    };
    let values = if wrapped {
        read_wrapper(&mut cursor, first, owner, profile)?
    } else {
        read_members(&mut cursor, first, owner, profile)?
    };
    match walk::next(&mut cursor)? {
        ScanStep::Finished => Ok(values),
        ScanStep::NeedMore => Err(trade_error(ErrorCode::UnexpectedEof,
            cursor.state().byte_offset, 0, "trade input did not finish")),
        ScanStep::Emit(_) => Err(trade_error(ErrorCode::TrailingContent,
            cursor.state().byte_offset, 0, "trailing trade input")),
    }
}

fn read_wrapper(
    cursor: &mut Cursor,
    mut event: Event,
    owner: &SlotOwner<'_>,
    profile: &ExchangeProfile,
) -> Result<TradeValues> {
    let mut seen = 0_u8;
    let mut values = None;
    loop {
        let key = match event {
            Event::ObjectEnd { .. } => break,
            Event::Key { str_ref } => str_ref,
            _ => return Err(trade_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                0, "combined trade wrapper requires object keys")),
        };
        let bit = match cursor_string(cursor, key, 0)? {
            "stream" => 1,
            "data" => 2,
            _ => return Err(trade_error(ErrorCode::UnknownObjectKey,
                cursor.state().byte_offset, 0, "unknown combined trade wrapper key")),
        };
        if seen & bit != 0 {
            return Err(trade_error(ErrorCode::DuplicateObjectKey, cursor.state().byte_offset,
                0, "duplicate combined trade wrapper key"));
        }
        seen |= bit;
        let value = next_event(cursor, 0)?;
        if bit == 1 {
            match value {
                Event::Scalar { kind: ScalarKind::String, decoded: Some(reference), .. } => {
                    if cursor_string(cursor, reference, 0)?.is_empty() {
                        return Err(trade_error(ErrorCode::ConstraintViolation,
                            cursor.state().byte_offset, 0, "combined trade stream name is empty"));
                    }
                }
                _ => return Err(trade_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                    0, "combined trade stream name must be a string")),
            }
        } else {
            if !matches!(value, Event::ObjectStart { .. }) {
                return Err(trade_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                    0, "combined trade data must be an object"));
            }
            let first = next_event(cursor, 0)?;
            values = Some(read_members(cursor, first, owner, profile)?);
        }
        event = next_event(cursor, 0)?;
    }
    if seen != 3 {
        return Err(trade_error(ErrorCode::MissingRequiredField, cursor.state().byte_offset,
            0, "combined trade wrapper requires stream and data"));
    }
    values.ok_or_else(|| trade_error(ErrorCode::MissingRequiredField,
        cursor.state().byte_offset, 0, "combined trade wrapper is missing data"))
}

fn read_members(
    cursor: &mut Cursor,
    mut event: Event,
    owner: &SlotOwner<'_>,
    profile: &ExchangeProfile,
) -> Result<TradeValues> {
    let mut seen = 0_u16;
    let mut price = None;
    let mut quantity = None;
    loop {
        let key = match event {
            Event::ObjectEnd { .. } => break,
            Event::Key { str_ref } => str_ref,
            _ => return Err(trade_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                0, "trade object requires field keys")),
        };
        let name = cursor_string(cursor, key, 0)?;
        let index = FIELD_NAMES.iter().position(|candidate| *candidate == name)
            .ok_or_else(|| trade_error(ErrorCode::UnknownObjectKey,
                cursor.state().byte_offset, 0, "unknown trade object key"))?;
        let field = index as u32 + 1;
        let bit = 1_u16 << index;
        if seen & bit != 0 {
            return Err(trade_error(ErrorCode::DuplicateObjectKey, cursor.state().byte_offset,
                field, "duplicate trade object key"));
        }
        seen |= bit;
        match (index, next_event(cursor, field)?) {
            (SLOT_EVENT | SLOT_SYMBOL | SLOT_PRICE | SLOT_QUANTITY,
                Event::Scalar { kind: ScalarKind::String, span, decoded: Some(reference) }) => {
                if cursor_string(cursor, reference, field)? != owner.string(index)?.1 {
                    return Err(trade_error(ErrorCode::InternalError, span.offset, field,
                        "trade string slot differs from wire value"));
                }
                if index == SLOT_PRICE || index == SLOT_QUANTITY {
                    let policy = if index == SLOT_PRICE { &profile.price } else { &profile.quantity };
                    let decimal = read_decimal(cursor, span, reference, policy, field)?;
                    if decimal.coefficient <= 0 {
                        return Err(trade_error(ErrorCode::ConstraintViolation, span.offset, field,
                            "trade price and quantity must be strictly positive"));
                    }
                    if index == SLOT_PRICE { price = Some(decimal); } else { quantity = Some(decimal); }
                }
            }
            (SLOT_EVENT_TIME | SLOT_AGGREGATE_TRADE_ID | SLOT_FIRST_TRADE_ID
                | SLOT_LAST_TRADE_ID | SLOT_TRADE_TIME,
                Event::Scalar { kind: ScalarKind::Number, span, .. }) => {
                if read_u64(cursor.raw_bytes(), span, field)? != owner.integer(index)? {
                    return Err(trade_error(ErrorCode::InternalError, span.offset, field,
                        "trade integer slot differs from wire value"));
                }
            }
            (SLOT_BUYER_IS_MAKER | SLOT_BEST_MATCH,
                Event::Scalar { kind: kind @ (ScalarKind::True | ScalarKind::False), span, .. }) => {
                let expected = if index == SLOT_BEST_MATCH {
                    owner.best_match()?
                } else {
                    Some(owner.boolean(index)?)
                };
                if expected != Some(kind == ScalarKind::True) {
                    return Err(trade_error(ErrorCode::InternalError, span.offset, field,
                        "trade boolean slot differs from wire value"));
                }
            }
            _ => return Err(trade_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                field, "trade field has the wrong JSON type")),
        }
        event = next_event(cursor, 0)?;
    }
    for index in 0..SLOT_BEST_MATCH {
        if seen & (1_u16 << index) == 0 {
            return Err(trade_error(ErrorCode::MissingRequiredField, cursor.state().byte_offset,
                index as u32 + 1, "missing required trade object key"));
        }
    }
    if seen & (1_u16 << SLOT_BEST_MATCH) == 0 && owner.best_match()?.is_some() {
        return Err(trade_error(ErrorCode::InternalError, cursor.state().byte_offset, 11,
            "trade best-match slot is present for an absent field"));
    }
    Ok(TradeValues {
        event_time_ms: owner.integer(SLOT_EVENT_TIME)?,
        symbol: owner.string(SLOT_SYMBOL)?.0,
        aggregate_trade_id: owner.integer(SLOT_AGGREGATE_TRADE_ID)?,
        price: price.ok_or_else(|| trade_error(ErrorCode::MissingRequiredField,
            cursor.state().byte_offset, 5, "missing trade price"))?,
        quantity: quantity.ok_or_else(|| trade_error(ErrorCode::MissingRequiredField,
            cursor.state().byte_offset, 6, "missing trade quantity"))?,
        first_trade_id: owner.integer(SLOT_FIRST_TRADE_ID)?,
        last_trade_id: owner.integer(SLOT_LAST_TRADE_ID)?,
        trade_time_ms: owner.integer(SLOT_TRADE_TIME)?,
        buyer_is_maker: owner.boolean(SLOT_BUYER_IS_MAKER)?,
        best_match: owner.best_match()?,
    })
}

fn read_u64(raw: &[u8], span: Span32, field: u32) -> Result<u64> {
    let bytes = span.slice(raw).ok_or_else(|| trade_error(ErrorCode::InternalError,
        span.offset, field, "trade integer span is out of bounds"))?;
    if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
        return Err(trade_error(ErrorCode::InvalidNumber, span.offset, field,
            "trade integer requires unsigned decimal digits"));
    }
    let mut value = 0_u64;
    for (index, &byte) in bytes.iter().enumerate() {
        // Checked Span32 bounds prove every visited absolute offset fits u32.
        let offset = span.offset + index as u32;
        if !byte.is_ascii_digit() {
            return Err(trade_error(ErrorCode::InvalidNumber, offset, field,
                "trade integer requires unsigned decimal digits"));
        }
        value = value.checked_mul(10).and_then(|n| n.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| trade_error(ErrorCode::NumberOverflow, offset, field,
                "trade integer exceeds u64"))?;
    }
    Ok(value)
}

fn read_decimal(
    cursor: &Cursor,
    span: Span32,
    reference: StrRef,
    policy: &DecimalPolicy,
    field: u32,
) -> Result<Decimal128> {
    let raw = span.slice(cursor.raw_bytes()).ok_or_else(|| trade_error(
        ErrorCode::InternalError, span.offset, field, "trade decimal token span is invalid"))?;
    // Parse ordinary tokens directly. Escapes belong to JSON, so apply decimal
    // grammar to the cursor's cooked value after adding the required quotes.
    let result = if !raw.contains(&b'\\') {
        jmax_decimal::parse::parse_quoted(raw, span.offset, policy)
    } else {
        let text = cursor_string(cursor, reference, field)?;
        let size = text.len().checked_add(2).ok_or_else(|| trade_error(
            ErrorCode::BudgetExhausted, span.offset, field,
            "decoded trade decimal token length overflows"))?;
        let mut quoted = Vec::new();
        quoted.try_reserve_exact(size).map_err(|_| trade_error(ErrorCode::BudgetExhausted,
            span.offset, field, "decoded trade decimal allocation failed"))?;
        quoted.push(b'"');
        quoted.extend_from_slice(text.as_bytes());
        quoted.push(b'"');
        jmax_decimal::parse::parse_quoted(&quoted, span.offset, policy)
    };
    result.map_err(|error| error.with_schema(TRADE_SCHEMA_ID.0, field))
}
