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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, vec};
    use jmax_abi::buffer::ValidationProof;

    const FIXTURE: &[u8] = br#"{"e":"aggTrade","E":123456789,"s":"BNBBTC","a":12345,"p":"0.00100000","q":"100.00000000","f":100,"l":105,"T":123456785,"m":true,"M":true}"#;

    struct Captured {
        payload: Payload,
        decoded: Box<[u8]>,
        slots: Vec<SlotValue>,
    }

    // Extraction makes no semantic claim. Even deliberately permissive or
    // malformed assertions go through inspect before any proof is issued.
    fn capture(payload: Payload, profile: &ExchangeProfile) -> Result<Captured> {
        let mut cursor = walk::from_payload(payload.clone(), profile.limits, SkipMode::Strict)?;
        let mut slots = vec![SlotValue::Empty; SLOT_COUNT];
        let mut pending = None;
        loop {
            match walk::next(&mut cursor)? {
                ScanStep::Finished => break,
                ScanStep::NeedMore => panic!("complete trade input requested more bytes"),
                ScanStep::Emit(Event::Key { str_ref }) => {
                    let name = cursor.resolve_str(str_ref).unwrap();
                    pending = FIELD_NAMES.iter().position(|candidate| *candidate == name);
                }
                ScanStep::Emit(Event::Scalar { kind, span, decoded }) => {
                    if let Some(index) = pending.take() {
                        slots[index] = match kind {
                            ScalarKind::String => SlotValue::Str(decoded.unwrap()),
                            ScalarKind::Number => SlotValue::U64(read_u64(
                                cursor.raw_bytes(), span, index as u32 + 1)?),
                            ScalarKind::True => SlotValue::Bool(true),
                            ScalarKind::False => SlotValue::Bool(false),
                            ScalarKind::Null => SlotValue::RawSpan(span),
                        };
                    }
                }
                ScanStep::Emit(Event::ObjectStart { offset } | Event::ArrayStart { offset }) => {
                    if let Some(index) = pending.take() {
                        slots[index] = SlotValue::RawSpan(Span32::new(offset, 1));
                    }
                }
                ScanStep::Emit(Event::ObjectEnd { .. } | Event::ArrayEnd { .. }) => {
                    pending = None;
                }
            }
        }
        Ok(Captured {
            payload, decoded: cursor.decoded_bytes().to_vec().into_boxed_slice(), slots,
        })
    }

    fn inspect(captured: &Captured, profile: &ExchangeProfile) -> Result<TradeValues> {
        let owner = SlotOwner {
            raw: captured.payload.as_slice(), decoded: &captured.decoded, slots: &captured.slots,
        };
        owner.validate(profile)?;
        read_document(captured.payload.clone(), &owner, profile)
    }

    struct TestValidator { calls: usize }

    impl ExchangeValidator for TestValidator {
        fn validate(&mut self, kind: MessageKind, payload: Payload, profile: &ExchangeProfile) -> Result<ValidatedSlots> {
            self.calls += 1;
            assert_eq!(kind, MessageKind::AggTrade);
            let captured = capture(payload, profile)?;
            inspect(&captured, profile)?;
            let fingerprint = profiles::schema_fingerprint(kind, profile)?;
            // SAFETY: Strict cursor traversal and inspect have validated the
            // complete JSON, exact slot values, string references and all trade
            // constraints. Length conversions are checked. These same owners
            // and assertions are returned without mutation by this validator.
            let proof = unsafe {
                ValidationProof::issue_unchecked(fingerprint,
                    u32::try_from(captured.payload.len()).unwrap(),
                    u32::try_from(captured.decoded.len()).unwrap())
            };
            Ok(ValidatedSlots {
                payload: captured.payload, decoded: captured.decoded, slots: captured.slots, proof,
            })
        }
    }

    struct ReturnedSlots(Option<ValidatedSlots>);

    impl ExchangeValidator for ReturnedSlots {
        fn validate(&mut self, _: MessageKind, _: Payload, _: &ExchangeProfile) -> Result<ValidatedSlots> {
            Ok(self.0.take().expect("one validator invocation"))
        }
    }

    fn decode(bytes: &[u8], profile: &ExchangeProfile) -> Result<TradeMessage> {
        decode_trade(Payload::from_slice(bytes), profile, &mut TestValidator { calls: 0 })
    }

    fn document(price: &str, quantity: &str, optional: &str) -> String {
        format!(r#"{{"e":"aggTrade","E":0,"s":"X","a":0,"p":"{price}","q":"{quantity}","f":0,"l":0,"T":0,"m":false{optional}}}"#)
    }

    fn fixture_slots(profile: &ExchangeProfile) -> ValidatedSlots {
        TestValidator { calls: 0 }.validate(MessageKind::AggTrade,
            Payload::from_slice(FIXTURE), profile).unwrap()
    }

    fn assert_bad_slots(mutator: impl FnOnce(&mut ValidatedSlots), expected: ErrorCode) {
        let profile = ExchangeProfile::default();
        let mut slots = fixture_slots(&profile);
        mutator(&mut slots);
        let error = decode_trade(Payload::from_slice(FIXTURE), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap_err();
        assert_eq!(error.code, expected, "{error}");
        assert_eq!(error.schema_id, TRADE_SCHEMA_ID.0);
    }

    #[test]
    fn frozen_schema_and_complete_slot_layout() {
        assert_eq!([SLOT_EVENT, SLOT_EVENT_TIME, SLOT_SYMBOL, SLOT_AGGREGATE_TRADE_ID,
            SLOT_PRICE, SLOT_QUANTITY, SLOT_FIRST_TRADE_ID, SLOT_LAST_TRADE_ID,
            SLOT_TRADE_TIME, SLOT_BUYER_IS_MAKER, SLOT_BEST_MATCH],
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(SLOT_COUNT, 11);
        let schema = schema_ir();
        assert_eq!(schema.name, "spot_agg_trade_v1");
        assert_eq!(schema.schema_id, TRADE_SCHEMA_ID);
        assert_eq!(schema.required_count, 10);
        assert!(schema.reject_unknown);
        assert_eq!(schema.fields.len(), SLOT_COUNT);
        for (index, field) in schema.fields.iter().enumerate() {
            assert_eq!(field.field_id, FieldId(index as u32 + 1));
            assert_eq!(field.name, FIELD_NAMES[index]);
            assert_eq!(field.required, index != SLOT_BEST_MATCH);
            assert_eq!(field.scalar_kind, match index {
                0 | 2 | 4 | 5 => Some(ScalarKind::String),
                1 | 3 | 6 | 7 | 8 => Some(ScalarKind::Number),
                _ => None,
            });
            assert!(field.format.is_none());
            assert!(field.min_i64.is_none() && field.max_i64.is_none());
            assert_eq!(field.min_len, if index == SLOT_SYMBOL { Some(1) } else { None });
            assert_eq!(field.max_len, if index == SLOT_SYMBOL { Some(MAX_SYMBOL_BYTES) } else { None });
        }
    }

    #[test]
    fn standard_fixture_preserves_owner_and_exact_values() {
        let profile = ExchangeProfile::default();
        let payload = Payload::from_slice(FIXTURE);
        let pointer = payload.as_slice().as_ptr();
        let message = decode_trade(payload, &profile, &mut TestValidator { calls: 0 }).unwrap();
        assert_eq!(message.raw_bytes().as_ptr(), pointer);
        assert_eq!(message.raw_bytes(), FIXTURE);
        let values = message.values();
        assert_eq!(values.event_time_ms, 123456789);
        assert_eq!(values.aggregate_trade_id, 12345);
        assert_eq!(values.price, Decimal128::new(100000, 8));
        assert_eq!(values.quantity, Decimal128::new(10000000000, 8));
        assert_eq!((values.first_trade_id, values.last_trade_id), (100, 105));
        assert_eq!(values.trade_time_ms, 123456785);
        assert!(values.buyer_is_maker);
        assert_eq!(values.best_match, Some(true));
        assert_eq!(message.resolve_str(values.symbol), Some("BNBBTC"));
        assert_eq!(message.proof().schema_fingerprint(),
            profiles::schema_fingerprint(MessageKind::AggTrade, &profile).unwrap());
    }

    #[test]
    fn optional_best_match_has_three_states_and_maker_is_not_inverted() {
        let profile = ExchangeProfile::default();
        for (suffix, expected) in [("", None), (",\"M\":false", Some(false)),
            (",\"M\":true", Some(true))] {
            for maker in [false, true] {
                let text = document("1", "2", suffix).replace("\"m\":false", &format!("\"m\":{maker}"));
                let slots = TestValidator { calls: 0 }.validate(MessageKind::AggTrade,
                    Payload::from_slice(text.as_bytes()), &profile).unwrap();
                assert_eq!(slots.slots.len(), SLOT_COUNT);
                assert_eq!(slots.slots[10], expected.map(SlotValue::Bool).unwrap_or(SlotValue::Empty));
                let values = decode(text.as_bytes(), &profile).unwrap().into_values();
                assert_eq!(values.buyer_is_maker, maker);
                assert_eq!(values.best_match, expected);
            }
        }
    }

    #[test]
    fn reordered_fields_u64_boundaries_and_equal_trade_range() {
        let profile = ExchangeProfile::default();
        let text = format!(r#"{{"M":false,"m":true,"T":{0},"l":{0},"f":{0},"q":"1","p":"1","a":{0},"s":"X","E":{0},"e":"aggTrade"}}"#, u64::MAX);
        let values = decode(text.as_bytes(), &profile).unwrap().into_values();
        assert_eq!(values.event_time_ms, u64::MAX);
        assert_eq!(values.aggregate_trade_id, u64::MAX);
        assert_eq!(values.first_trade_id, u64::MAX);
        assert_eq!(values.last_trade_id, u64::MAX);
        assert_eq!(values.trade_time_ms, u64::MAX);
        assert!(decode(document("1", "1", "").as_bytes(), &profile).is_ok());
        // Aggregate IDs, timestamps and constituent IDs have no extra ordering
        // relationship. Only first_trade_id <= last_trade_id is mandatory.
        let text = document("1", "1", "").replace("\"l\":0", &format!("\"l\":{}", u64::MAX));
        assert!(decode(text.as_bytes(), &profile).is_ok());
    }

    #[test]
    fn integer_grammar_overflow_and_span_boundaries() {
        assert_eq!(read_u64(b"0", Span32::new(0, 1), 2).unwrap(), 0);
        assert_eq!(read_u64(b"18446744073709551615", Span32::new(0, 20), 2).unwrap(), u64::MAX);
        for input in ["", "00", "01", "-0", "-1", "+1", "1.0", "1e0", " 1", "1 "] {
            let error = read_u64(input.as_bytes(), Span32::new(0, input.len() as u32), 2).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidNumber, "{input}");
            assert_eq!(error.source, ErrorSource::Decode);
            assert_eq!(error.field_id, 2);
        }
        let error = read_u64(b"__18446744073709551616", Span32::new(2, 20), 9).unwrap_err();
        assert_eq!(error.code, ErrorCode::NumberOverflow);
        assert_eq!(error.byte_offset, 21);
        assert_eq!(error.field_id, 9);
        for span in [Span32::new(u32::MAX, 1), Span32::new(0, 2), Span32::new(2, 0)] {
            assert_eq!(read_u64(b"1", span, 2).unwrap_err().code, ErrorCode::InternalError);
        }
        let profile = ExchangeProfile::default();
        for key in ["E", "a", "f", "l", "T"] {
            for (token, expected) in [("-1", ErrorCode::InvalidNumber), ("-0", ErrorCode::InvalidNumber),
                ("1.0", ErrorCode::InvalidNumber), ("1e0", ErrorCode::InvalidNumber),
                ("18446744073709551616", ErrorCode::NumberOverflow),
                ("\"1\"", ErrorCode::TypeMismatch)] {
                let text = document("1", "1", "").replace(&format!("\"{key}\":0"), &format!("\"{key}\":{token}"));
                assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, expected, "{key}: {token}");
            }
        }
    }

    #[test]
    fn unicode_symbols_escaped_keys_and_decimals_use_decoded_values() {
        let mut profile = ExchangeProfile::default();
        profile.max_symbol_bytes = 4;
        let text = br#"{"\u0065":"agg\u0054rade","E":0,"s":"\u00e9\u00e9","a":0,"p":"\u0031.25","q":"2\u002e5","f":0,"l":0,"T":0,"m":false}"#;
        let message = decode(text, &profile).unwrap();
        assert_eq!(message.resolve_str(message.values().symbol), Some("éé"));
        assert_eq!(message.values().symbol.scalar_len, 2);
        assert_eq!(message.values().symbol.source, SpanSource::DecodedArena);
        assert_eq!(message.values().price, Decimal128::new(125000000, 8));
        assert_eq!(message.values().quantity, Decimal128::new(250000000, 8));
        profile.max_symbol_bytes = 3;
        assert_eq!(decode(text, &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile.max_symbol_bytes = 40;
        let long = document("1", "1", "").replace("\"X\"", &format!("\"{}\"", "S".repeat(40)));
        assert!(decode(long.as_bytes(), &profile).is_ok());
        profile.max_symbol_bytes = 39;
        assert_eq!(decode(long.as_bytes(), &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        let supplementary = document("1", "1", "").replace("\"X\"", r#""\uD83D\uDE80""#);
        profile.max_symbol_bytes = 4;
        let message = decode(supplementary.as_bytes(), &profile).unwrap();
        assert_eq!(message.values().symbol.scalar_len, 1);
        assert_eq!(message.resolve_str(message.values().symbol), Some("🚀"));
    }

    #[test]
    fn decimal_grammar_positivity_and_error_context() {
        let profile = ExchangeProfile::default();
        for (value, expected) in [("0", ErrorCode::ConstraintViolation),
            ("-0.00000000", ErrorCode::ConstraintViolation), ("-1", ErrorCode::ConstraintViolation),
            ("01", ErrorCode::InvalidNumber), ("+1", ErrorCode::InvalidNumber),
            (".1", ErrorCode::InvalidNumber), ("1.", ErrorCode::InvalidNumber),
            ("1e0", ErrorCode::InvalidNumber), ("NaN", ErrorCode::InvalidNumber),
            ("Infinity", ErrorCode::InvalidNumber), (" 1", ErrorCode::InvalidNumber),
            ("1 ", ErrorCode::InvalidNumber), ("１", ErrorCode::InvalidNumber),
            ("0.000000001", ErrorCode::DecimalScaleMismatch)] {
            for (price, quantity, field) in [(value, "1", 5), ("1", value, 6)] {
                let text = document(price, quantity, "");
                let error = decode(text.as_bytes(), &profile).unwrap_err();
                assert_eq!(error.code, expected, "field {field}: {value}: {error}");
                assert_eq!(error.schema_id, TRADE_SCHEMA_ID.0);
                assert_eq!(error.field_id, field);
            }
        }
        let text = document("0.00000001", "0.00000001", "");
        let message = decode(text.as_bytes(), &profile).unwrap();
        assert_eq!(message.values().price, Decimal128::new(1, 8));
        assert_eq!(message.values().quantity, Decimal128::new(1, 8));
        for bad in [r#""\u002b1""#, r#""1\u0022""#, r#""1\n""#] {
            let text = document("1", "1", "").replace("\"p\":\"1\"", &format!("\"p\":{bad}"));
            assert!(decode(text.as_bytes(), &profile).is_err());
        }
    }

    #[test]
    fn decimal_policy_precision_ticks_exponents_and_truncation() {
        let mut profile = ExchangeProfile::default();
        profile.price.target_scale = 2;
        profile.price.max_precision = 3;
        profile.price.tick_step = 5;
        profile.quantity.target_scale = 0;
        profile.quantity.tick_step = 2;
        let text = document("1.25", "2", "");
        let values = decode(text.as_bytes(), &profile).unwrap().into_values();
        assert_eq!(values.price, Decimal128::new(125, 2));
        assert_eq!(values.quantity, Decimal128::new(2, 0));
        for (p, q, expected, field) in [("1.26", "2", ErrorCode::TickMultipleViolation, 5),
            ("10.00", "2", ErrorCode::DecimalPrecisionOverflow, 5),
            ("1.251", "2", ErrorCode::DecimalScaleMismatch, 5),
            ("1.25", "3", ErrorCode::TickMultipleViolation, 6),
            ("1.25", "2.1", ErrorCode::DecimalScaleMismatch, 6)] {
            let error = decode(document(p, q, "").as_bytes(), &profile).unwrap_err();
            assert_eq!(error.code, expected);
            assert_eq!(error.field_id, field);
        }
        profile.price.reject_inexact_rescale = false;
        assert_eq!(decode(document("1.259", "2", "").as_bytes(), &profile).unwrap()
            .values().price, Decimal128::new(125, 2));
        assert_eq!(decode(document("0.001", "2", "").as_bytes(), &profile)
            .unwrap_err().code, ErrorCode::ConstraintViolation);
        profile.price.allow_exponent = true;
        assert!(decode(document("125e-2", "2", "").as_bytes(), &profile).is_ok());
        assert_eq!(decode(document("1.25", "2e0", "").as_bytes(), &profile)
            .unwrap_err().code, ErrorCode::InvalidNumber);
        profile.price.max_precision = 38;
        profile.price.target_scale = 0;
        profile.price.tick_step = 0;
        let largest = "99999999999999999999999999999999999999";
        let values = decode(document(largest, "2", "").as_bytes(), &profile).unwrap().into_values();
        assert_eq!(values.price.coefficient, 99_999_999_999_999_999_999_999_999_999_999_999_999_i128);
        assert_eq!(decode(document("999999999999999999999999999999999999999", "2", "").as_bytes(), &profile)
            .unwrap_err().code, ErrorCode::NumberOverflow);
        assert_eq!(decode(document("1e38", "2", "").as_bytes(), &profile)
            .unwrap_err().code, ErrorCode::DecimalPrecisionOverflow);
    }

    #[test]
    fn wrapper_policies_preserve_full_payload_and_absolute_spans() {
        let inner = core::str::from_utf8(FIXTURE).unwrap();
        for text in [format!(r#"{{"stream":"bnb@aggTrade","data":{inner}}}"#),
            format!(r#"{{"data":{inner},"stream":"bnb\u0040aggTrade"}}"#)] {
            for wrapper in [WrapperPolicy::CombinedStream, WrapperPolicy::Either] {
                let profile = ExchangeProfile { wrapper, ..ExchangeProfile::default() };
                let message = decode(text.as_bytes(), &profile).unwrap();
                assert_eq!(message.raw_bytes(), text.as_bytes());
                assert_eq!(message.resolve_str(message.values().symbol), Some("BNBBTC"));
                assert_eq!(message.values().symbol.span.offset as usize,
                    text.find("BNBBTC").unwrap());
            }
            assert_eq!(decode(text.as_bytes(), &ExchangeProfile::default()).unwrap_err().code,
                ErrorCode::UnknownObjectKey);
        }
        let either = ExchangeProfile { wrapper: WrapperPolicy::Either, ..ExchangeProfile::default() };
        assert!(decode(FIXTURE, &either).is_ok());
        let combined = ExchangeProfile { wrapper: WrapperPolicy::CombinedStream, ..either };
        assert_eq!(decode(FIXTURE, &combined).unwrap_err().code, ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn malformed_wrappers_never_fall_back_to_bare_messages() {
        let inner = core::str::from_utf8(FIXTURE).unwrap();
        let profile = ExchangeProfile {
            wrapper: WrapperPolicy::Either,
            limits: Limits { reject_duplicate_keys: false, reject_unknown_keys: false,
                ..Limits::strict_default() },
            ..ExchangeProfile::default()
        };
        for (text, expected) in [
            (format!(r#"{{"data":{inner}}}"#), ErrorCode::MissingRequiredField),
            (format!(r#"{{"stream":"","data":{inner}}}"#), ErrorCode::ConstraintViolation),
            (format!(r#"{{"stream":7,"data":{inner}}}"#), ErrorCode::TypeMismatch),
            (format!(r#"{{"stream":"x","data":{inner},"extra":0}}"#), ErrorCode::UnknownObjectKey),
            (format!(r#"{{"stream":"x","data":{inner},"stream":"x"}}"#), ErrorCode::DuplicateObjectKey),
            (format!(r#"{{"stream":"x","data":{inner},"data":{inner}}}"#), ErrorCode::DuplicateObjectKey),
            (format!(r#"{{"stream":"x","data":{inner},"e":"aggTrade"}}"#), ErrorCode::UnknownObjectKey),
        ] {
            let captured = capture(Payload::from_slice(text.as_bytes()), &profile).unwrap();
            assert_eq!(inspect(&captured, &profile).unwrap_err().code, expected, "{text}");
        }
        // Use valid metadata assertions to reach malformed wrapper data even
        // when no inner trade is available for a permissive extractor.
        let captured = capture(Payload::from_slice(FIXTURE), &profile).unwrap();
        let owner = SlotOwner { raw: captured.payload.as_slice(), decoded: &captured.decoded,
            slots: &captured.slots };
        for bytes in [&br#"{"stream":"x"}"#[..], &br#"{"stream":"x","data":null}"#[..],
            &br#"{"stream":"x","data":[]}"#[..]] {
            assert!(read_document(Payload::from_slice(bytes), &owner, &profile).is_err());
        }
    }

    #[test]
    fn missing_members_wrong_types_and_invalid_metadata() {
        let profile = ExchangeProfile::default();
        for member in [r#""e":"aggTrade","#, r#""E":0,"#, r#""s":"X","#,
            r#""a":0,"#, r#""p":"1","#, r#""q":"1","#, r#""f":0,"#,
            r#""l":0,"#, r#""T":0,"#] {
            let text = document("1", "1", "").replace(member, "");
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code,
                ErrorCode::MissingRequiredField, "{member}");
        }
        let missing_m = document("1", "1", "").replace(",\"m\":false", "");
        assert_eq!(decode(missing_m.as_bytes(), &profile).unwrap_err().code, ErrorCode::MissingRequiredField);
        for (from, to, expected) in [("aggTrade", "depthUpdate", ErrorCode::ConstraintViolation),
            ("\"s\":\"X\"", "\"s\":\"\"", ErrorCode::ConstraintViolation),
            ("\"f\":0", "\"f\":1", ErrorCode::ConstraintViolation)] {
            let text = document("1", "1", "").replace(from, to);
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, expected);
        }
        for key in ["m", "M"] {
            for token in ["0", "1", "null", "\"true\"", "[]", "{}"] {
                let text = document("1", "1", ",\"M\":false")
                    .replace(&format!("\"{key}\":false"), &format!("\"{key}\":{token}"));
                assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code,
                    ErrorCode::TypeMismatch, "{key}: {token}");
            }
        }
        for key in ["p", "q"] {
            for token in ["1", "true", "null", "[]", "{}"] {
                let text = document("1", "1", "")
                    .replace(&format!("\"{key}\":\"1\""), &format!("\"{key}\":{token}"));
                assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code,
                    ErrorCode::TypeMismatch, "{key}: {token}");
            }
        }
    }

    #[test]
    fn strict_wire_checks_survive_permissive_slot_extraction() {
        let profile = ExchangeProfile {
            limits: Limits { reject_duplicate_keys: false, reject_unknown_keys: false,
                ..Limits::strict_default() },
            ..ExchangeProfile::default()
        };
        for (suffix, expected) in [(",\"m\":false", ErrorCode::DuplicateObjectKey),
            (",\"M\":false,\"M\":false", ErrorCode::DuplicateObjectKey),
            (",\"\\u006d\":false", ErrorCode::DuplicateObjectKey),
            (",\"extra\":0", ErrorCode::UnknownObjectKey)] {
            let text = document("1", "1", suffix);
            let captured = capture(Payload::from_slice(text.as_bytes()), &profile).unwrap();
            assert_eq!(inspect(&captured, &profile).unwrap_err().code, expected);
        }
        let original = capture(Payload::from_slice(FIXTURE), &profile).unwrap();
        let owner = SlotOwner { raw: original.payload.as_slice(), decoded: &original.decoded,
            slots: &original.slots };
        for suffix in ["x", "{}", ",", "\0", "\u{a0}"] {
            let mut bytes = FIXTURE.to_vec();
            bytes.extend_from_slice(suffix.as_bytes());
            assert!(read_document(Payload::from_slice(&bytes), &owner, &profile).is_err());
        }
        // A boolean-shaped slot cannot grant a nonboolean wire value access
        // through the schema IR's None scalar kind.
        for key in ["m", "M"] {
            let text = document("1", "1", ",\"M\":false")
                .replace(&format!("\"{key}\":false"), &format!("\"{key}\":null"));
            let mut captured = capture(Payload::from_slice(text.as_bytes()), &profile).unwrap();
            let index = if key == "m" { SLOT_BUYER_IS_MAKER } else { SLOT_BEST_MATCH };
            captured.slots[index] = SlotValue::Bool(false);
            assert_eq!(inspect(&captured, &profile).unwrap_err().code, ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn corrupted_public_slots_cannot_publish_an_owner() {
        assert_bad_slots(|s| { s.slots.pop(); }, ErrorCode::MissingRequiredField);
        assert_bad_slots(|s| s.slots.push(SlotValue::Empty), ErrorCode::TypeMismatch);
        for index in 0..SLOT_BEST_MATCH {
            assert_bad_slots(|s| s.slots[index] = SlotValue::Empty, ErrorCode::MissingRequiredField);
        }
        assert_bad_slots(|s| s.slots[1] = SlotValue::I64(1), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[1] = SlotValue::U64(1), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[3] = SlotValue::U64(1), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[6] = SlotValue::U64(106), ErrorCode::ConstraintViolation);
        assert_bad_slots(|s| s.slots[9] = SlotValue::Bool(false), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[10] = SlotValue::Bool(false), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[10] = SlotValue::Empty, ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[10] = SlotValue::U64(1), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[4] = SlotValue::Decimal(Decimal128::new(100000, 8)), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots.swap(4, 5), ErrorCode::InternalError);
        assert_bad_slots(|s| {
            let SlotValue::Str(mut reference) = s.slots[2] else { panic!("symbol slot") };
            reference.scalar_len += 1;
            s.slots[2] = SlotValue::Str(reference);
        }, ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[2] = SlotValue::Str(StrRef {
            source: SpanSource::RawPayload, span: Span32::new(u32::MAX, 1), scalar_len: 1,
        }), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[2] = SlotValue::Str(StrRef {
            source: SpanSource::DecodedArena, span: Span32::new(0, 1), scalar_len: 1,
        }), ErrorCode::InternalError);
        assert_bad_slots(|s| { s.decoded = vec![b'x'].into_boxed_slice(); }, ErrorCode::InternalError);
        assert_bad_slots(|s| { s.payload = Payload::from_slice(b"{}"); }, ErrorCode::InternalError);
        let profile = ExchangeProfile::default();
        let text = document("1", "1", "");
        let mut slots = TestValidator { calls: 0 }.validate(MessageKind::AggTrade,
            Payload::from_slice(text.as_bytes()), &profile).unwrap();
        slots.slots[10] = SlotValue::Bool(false);
        assert_eq!(decode_trade(Payload::from_slice(text.as_bytes()), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap_err().code, ErrorCode::InternalError);
    }

    #[test]
    fn decoded_owner_is_retained_and_mutated_strings_are_rejected() {
        let profile = ExchangeProfile::default();
        let text = document("1", "1", "").replace("\"X\"", r#""\u0058""#);
        let slots = TestValidator { calls: 0 }.validate(MessageKind::AggTrade,
            Payload::from_slice(text.as_bytes()), &profile).unwrap();
        let pointer = slots.decoded.as_ptr();
        let copy = slots.decoded.to_vec();
        let message = decode_trade(Payload::from_slice(text.as_bytes()), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap();
        assert_eq!(message.decoded_bytes().as_ptr(), pointer);
        assert_eq!(message.decoded_bytes(), copy.as_slice());
        assert_eq!(message.resolve_str(message.values().symbol), Some("X"));
        for (byte, expected) in [(0xff, ErrorCode::InvalidUtf8), (b'Y', ErrorCode::InternalError)] {
            let mut slots = TestValidator { calls: 0 }.validate(MessageKind::AggTrade,
                Payload::from_slice(text.as_bytes()), &profile).unwrap();
            let SlotValue::Str(reference) = slots.slots[2] else { panic!("symbol slot") };
            slots.decoded[reference.span.offset as usize] = byte;
            let error = decode_trade(Payload::from_slice(text.as_bytes()), &profile,
                &mut ReturnedSlots(Some(slots))).unwrap_err();
            assert_eq!(error.code, expected);
            assert_eq!(error.field_id, 3);
        }
    }

    #[test]
    fn payload_arena_depth_field_string_and_number_budgets() {
        let mut profile = ExchangeProfile::default();
        profile.limits.max_payload_bytes = FIXTURE.len() as u32;
        assert!(decode(FIXTURE, &profile).is_ok());
        profile.limits.max_payload_bytes -= 1;
        let mut validator = TestValidator { calls: 0 };
        assert_eq!(decode_trade(Payload::from_slice(FIXTURE), &profile, &mut validator)
            .unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert_eq!(validator.calls, 0);
        profile = ExchangeProfile::default();
        profile.limits.max_depth = 1;
        assert!(decode(FIXTURE, &profile).is_ok());
        profile.wrapper = WrapperPolicy::CombinedStream;
        let wrapper = format!(r#"{{"stream":"x","data":{}}}"#, core::str::from_utf8(FIXTURE).unwrap());
        assert_eq!(decode(wrapper.as_bytes(), &profile).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        profile = ExchangeProfile::default();
        profile.limits.max_fields_per_object = 11;
        assert!(decode(FIXTURE, &profile).is_ok());
        profile.limits.max_fields_per_object = 10;
        assert!(decode(FIXTURE, &profile).is_err());
        profile = ExchangeProfile::default();
        profile.limits.max_string_bytes = 5;
        assert!(decode(FIXTURE, &profile).is_err());
        profile = ExchangeProfile::default();
        profile.limits.max_number_digits = 8;
        assert!(decode(FIXTURE, &profile).is_err());
        profile = ExchangeProfile::default();
        let escaped = document("1", "1", "").replace("\"X\"", r#""\u0058""#);
        let captured = capture(Payload::from_slice(escaped.as_bytes()), &profile).unwrap();
        assert!(!captured.decoded.is_empty());
        profile.limits.max_decoded_bytes = captured.decoded.len() as u32;
        assert!(decode(escaped.as_bytes(), &profile).is_ok());
        profile.limits.max_decoded_bytes -= 1;
        assert_eq!(decode(escaped.as_bytes(), &profile).unwrap_err().code, ErrorCode::DecodedArenaOverflow);
    }

    #[test]
    fn validator_errors_fingerprints_and_invalid_profiles() {
        let profile = ExchangeProfile::default();
        let other = ExchangeProfile { max_levels: 1, ..profile };
        let slots = fixture_slots(&other);
        assert_eq!(decode_trade(Payload::from_slice(FIXTURE), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap_err().code, ErrorCode::SchemaImageInvalid);
        struct Failing(Error);
        impl ExchangeValidator for Failing {
            fn validate(&mut self, _: MessageKind, _: Payload, _: &ExchangeProfile) -> Result<ValidatedSlots> {
                Err(self.0.clone())
            }
        }
        let expected = Error::new(ErrorCode::Cancelled, ErrorSource::SchemaVm, 17,
            "test trade validator failure").with_schema(7, 8);
        assert_eq!(decode_trade(Payload::from_slice(FIXTURE), &profile,
            &mut Failing(expected.clone())).unwrap_err(), expected);
        for invalid in [ExchangeProfile { version: 2, ..profile },
            ExchangeProfile { max_symbol_bytes: 0, ..profile },
            ExchangeProfile { price: DecimalPolicy { max_precision: 39, ..profile.price }, ..profile },
            ExchangeProfile { quantity: DecimalPolicy { target_scale: 39, ..profile.quantity }, ..profile }] {
            let mut validator = TestValidator { calls: 0 };
            assert!(decode_trade(Payload::from_slice(b"invalid"), &invalid, &mut validator).is_err());
            assert_eq!(validator.calls, 0);
        }
    }

    #[test]
    fn malformed_documents_and_arbitrary_string_references_fail() {
        let profile = ExchangeProfile::default();
        for bytes in [&b""[..], &b"{"[..], &b"[]"[..], &b"null"[..],
            &b"{\"e\":\"aggTrade\",}"[..], &b"{\"e\":\"\\uD800\"}"[..],
            &b"{\"e\":\"\xff\"}"[..], &b"{\"m\":True}"[..]] {
            assert!(decode(bytes, &profile).is_err(), "{bytes:?}");
        }
        let slots = [SlotValue::Str(StrRef {
            span: Span32::new(0, 1), source: SpanSource::RawPayload, scalar_len: 1,
        })];
        let owner = SlotOwner { raw: b"\xff", decoded: &[], slots: &slots };
        assert_eq!(owner.string(0).unwrap_err().code, ErrorCode::InvalidUtf8);
        let slots = [SlotValue::Str(StrRef {
            span: Span32::new(u32::MAX, 1), source: SpanSource::RawPayload, scalar_len: 1,
        })];
        let owner = SlotOwner { raw: b"", decoded: &[], slots: &slots };
        assert_eq!(owner.string(0).unwrap_err().code, ErrorCode::InternalError);
        let text = format!(" \r\n{}\t ", core::str::from_utf8(FIXTURE).unwrap());
        assert!(decode(text.as_bytes(), &profile).is_ok());
    }
}
