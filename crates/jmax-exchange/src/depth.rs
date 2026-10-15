//! Frozen Spot depth slots and checked, exact-decimal message decoding.

use super::*;
use jmax_abi::syntax::{Event, ScanStep};
use jmax_cursor::{walk, Cursor, SkipMode};

pub const SLOT_EVENT: usize = 0;
pub const SLOT_EVENT_TIME: usize = 1;
pub const SLOT_SYMBOL: usize = 2;
pub const SLOT_FIRST_UPDATE_ID: usize = 3;
pub const SLOT_FINAL_UPDATE_ID: usize = 4;
pub const SLOT_BIDS: usize = 5;
pub const SLOT_ASKS: usize = 6;
pub const SLOT_COUNT: usize = 7;

const FIELD_NAMES: [&str; SLOT_COUNT] = ["e", "E", "s", "U", "u", "b", "a"];

/// The default-profile schema. Book shape and profile-specific constraints are
/// deliberately enforced by decoding as well as by the validator adapter.
pub fn schema_ir() -> SchemaIr {
    let fields = FIELD_NAMES.iter().enumerate().map(|(index, name)| {
        SchemaFieldIr {
            field_id: FieldId(index as u32 + 1),
            name: String::from(*name),
            required: true,
            scalar_kind: match index {
                SLOT_EVENT | SLOT_SYMBOL => Some(ScalarKind::String),
                SLOT_EVENT_TIME | SLOT_FIRST_UPDATE_ID | SLOT_FINAL_UPDATE_ID => {
                    Some(ScalarKind::Number)
                }
                _ => None,
            },
            format: None,
            min_len: if index == SLOT_SYMBOL { Some(1) } else { None },
            max_len: if index == SLOT_SYMBOL { Some(MAX_SYMBOL_BYTES) } else { None },
            min_i64: None,
            max_i64: None,
        }
    }).collect();
    SchemaIr {
        name: String::from("spot_depth_v1"),
        schema_id: DEPTH_SCHEMA_ID,
        required_count: SLOT_COUNT as u16,
        reject_unknown: true,
        fields,
    }
}

/// Validate the complete original owner, check the adapter's slots against the
/// document, and publish only fully owned books and an owner-bound symbol.
pub fn decode_depth(
    payload: Payload,
    profile: &ExchangeProfile,
    validator: &mut dyn ExchangeValidator,
) -> Result<DepthMessage> {
    profiles::validate_profile(profile)?;
    if payload.len() > profile.limits.max_payload_bytes as usize {
        return Err(depth_error(ErrorCode::PayloadTooLarge, 0, 0,
            "depth payload exceeds byte limit"));
    }
    let fingerprint = profiles::schema_fingerprint(MessageKind::Depth, profile)?;
    let slots = validator.validate(MessageKind::Depth, payload.clone(), profile)?;
    ensure_slot_owner(&slots, payload.as_slice()).map_err(|error| {
        error.with_schema(DEPTH_SCHEMA_ID.0, 0)
    })?;
    if slots.proof.schema_fingerprint() != fingerprint {
        return Err(depth_error(ErrorCode::SchemaImageInvalid, 0, 0,
            "depth slot fingerprint differs from profile"));
    }
    if slots.decoded.len() > profile.limits.max_decoded_bytes as usize {
        return Err(depth_error(ErrorCode::DecodedArenaOverflow, 0, 0,
            "depth decoded arena exceeds byte limit"));
    }
    let owner = SlotOwner {
        raw: slots.payload.as_slice(),
        decoded: &slots.decoded,
        slots: &slots.slots,
    };
    let values = owner.depth_values(profile)?;
    let values = read_document(slots.payload.clone(), &owner, profile, values)?;

    // SAFETY: The validator's original payload, arena and proof are moved intact.
    // Lengths, fingerprint, exact slot layout, every emitted string's bounds,
    // UTF-8 and scalar count have been checked. A strict traversal independently
    // checks the complete wrapper/document, binds every slot to its wire value,
    // and validates every level and decimal. Symbol is the checked original
    // StrRef; books own Decimal128 values and retain no cursor/scratch reference.
    Ok(unsafe {
        ParsedMessage::new_proven(slots.payload, slots.decoded, values, slots.proof)
    })
}

fn depth_error(code: ErrorCode, offset: u32, field: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, offset, detail)
        .with_schema(DEPTH_SCHEMA_ID.0, field)
}

fn book_error(
    code: ErrorCode,
    offset: u32,
    schema: SchemaId,
    field: FieldId,
    detail: &'static str,
) -> Error {
    Error::new(code, ErrorSource::Decode, offset, detail).with_schema(schema.0, field.0)
}

/// A checked view rather than a capability: publicly mutable ValidatedSlots
/// cannot establish the unsafe publication invariants by themselves.
struct SlotOwner<'a> {
    raw: &'a [u8],
    decoded: &'a [u8],
    slots: &'a [SlotValue],
}

impl SlotOwner<'_> {
    fn slot(&self, index: usize) -> Result<SlotValue> {
        match self.slots.get(index).copied() {
            None | Some(SlotValue::Empty) => Err(depth_error(
                ErrorCode::MissingRequiredField, 0, index as u32 + 1,
                "missing required depth slot")),
            Some(value) => Ok(value),
        }
    }

    fn string(&self, index: usize) -> Result<(StrRef, &str)> {
        let reference = match self.slot(index)? {
            SlotValue::Str(reference) => reference,
            _ => return Err(depth_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "depth slot must be a string")),
        };
        let storage = match reference.source {
            SpanSource::RawPayload => self.raw,
            SpanSource::DecodedArena => self.decoded,
        };
        let offset = match reference.source {
            SpanSource::RawPayload => reference.span.offset,
            SpanSource::DecodedArena => 0,
        };
        let bytes = reference.span.slice(storage).ok_or_else(|| depth_error(
            ErrorCode::InternalError, offset, index as u32 + 1,
            "depth string span is out of bounds"))?;
        let text = core::str::from_utf8(bytes).map_err(|_| depth_error(
            ErrorCode::InvalidUtf8, offset, index as u32 + 1,
            "depth string slot is not UTF-8"))?;
        if text.chars().count() != reference.scalar_len as usize {
            return Err(depth_error(ErrorCode::InternalError, offset, index as u32 + 1,
                "depth string scalar count is incorrect"));
        }
        Ok((reference, text))
    }

    fn integer(&self, index: usize) -> Result<u64> {
        match self.slot(index)? {
            SlotValue::U64(value) => Ok(value),
            _ => Err(depth_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "depth slot must be an unsigned integer")),
        }
    }

    fn span(&self, index: usize) -> Result<Span32> {
        match self.slot(index)? {
            SlotValue::RawSpan(span) if span.slice(self.raw).is_some() => Ok(span),
            SlotValue::RawSpan(span) => Err(depth_error(ErrorCode::InternalError,
                span.offset, index as u32 + 1, "depth book span is out of bounds")),
            _ => Err(depth_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "depth book slot must be a raw span")),
        }
    }

    fn depth_values(&self, profile: &ExchangeProfile) -> Result<DepthValues> {
        if self.slots.len() > SLOT_COUNT {
            return Err(depth_error(ErrorCode::TypeMismatch, 0, 0,
                "depth validator returned extra slots"));
        }
        let (_, event) = self.string(SLOT_EVENT)?;
        if event != "depthUpdate" {
            return Err(depth_error(ErrorCode::ConstraintViolation, 0, 1,
                "depth event tag must be depthUpdate"));
        }
        let (symbol, text) = self.string(SLOT_SYMBOL)?;
        if text.is_empty() || text.len() > profile.max_symbol_bytes as usize {
            return Err(depth_error(ErrorCode::ConstraintViolation, 0, 3,
                "depth symbol is empty or exceeds profile byte limit"));
        }
        let event_time_ms = self.integer(SLOT_EVENT_TIME)?;
        let first_update_id = self.integer(SLOT_FIRST_UPDATE_ID)?;
        let final_update_id = self.integer(SLOT_FINAL_UPDATE_ID)?;
        if first_update_id > final_update_id {
            return Err(depth_error(ErrorCode::ConstraintViolation, 0, 4,
                "depth first update ID exceeds final update ID"));
        }
        self.span(SLOT_BIDS)?;
        self.span(SLOT_ASKS)?;
        Ok(DepthValues {
            event_time_ms, symbol, first_update_id, final_update_id,
            bids: Vec::new(), asks: Vec::new(),
        })
    }
}

fn next_event(cursor: &mut Cursor, schema: SchemaId, field: FieldId) -> Result<Event> {
    match walk::next(cursor)? {
        ScanStep::Emit(event) => Ok(event),
        ScanStep::NeedMore | ScanStep::Finished => Err(book_error(
            ErrorCode::UnexpectedEof, cursor.state().byte_offset, schema, field,
            "expected another exchange JSON event")),
    }
}

fn cursor_string(cursor: &Cursor, reference: StrRef, schema: SchemaId, field: FieldId) -> Result<&str> {
    let text = cursor.resolve_str(reference).ok_or_else(|| book_error(
        ErrorCode::InternalError, cursor.state().byte_offset, schema, field,
        "cursor string does not resolve"))?;
    if text.chars().count() != reference.scalar_len as usize {
        return Err(book_error(ErrorCode::InternalError, cursor.state().byte_offset,
            schema, field, "cursor string scalar count is incorrect"));
    }
    Ok(text)
}

fn read_document(
    payload: Payload,
    owner: &SlotOwner<'_>,
    profile: &ExchangeProfile,
    mut values: DepthValues,
) -> Result<DepthValues> {
    let mut cursor = walk::from_payload(payload, profile.limits, SkipMode::Strict)?;
    if !matches!(next_event(&mut cursor, DEPTH_SCHEMA_ID, FieldId(0))?, Event::ObjectStart { .. }) {
        return Err(depth_error(ErrorCode::TypeMismatch, cursor.state().byte_offset, 0,
            "depth document must be an object"));
    }
    let first = next_event(&mut cursor, DEPTH_SCHEMA_ID, FieldId(0))?;
    let wrapped = match profile.wrapper {
        WrapperPolicy::Bare => false,
        WrapperPolicy::CombinedStream => true,
        WrapperPolicy::Either => match first {
            Event::Key { str_ref } => matches!(
                cursor_string(&cursor, str_ref, DEPTH_SCHEMA_ID, FieldId(0))?,
                "stream" | "data"
            ),
            _ => false,
        },
    };
    if wrapped {
        read_wrapper(&mut cursor, first, owner, profile, &mut values)?;
    } else {
        read_members(&mut cursor, first, owner, profile, &mut values)?;
    }
    match walk::next(&mut cursor)? {
        ScanStep::Finished => Ok(values),
        ScanStep::NeedMore => Err(depth_error(ErrorCode::UnexpectedEof,
            cursor.state().byte_offset, 0, "depth input did not finish")),
        ScanStep::Emit(_) => Err(depth_error(ErrorCode::TrailingContent,
            cursor.state().byte_offset, 0, "trailing depth input")),
    }
}

fn read_wrapper(
    cursor: &mut Cursor,
    mut event: Event,
    owner: &SlotOwner<'_>,
    profile: &ExchangeProfile,
    values: &mut DepthValues,
) -> Result<()> {
    let mut seen = 0_u8;
    loop {
        let key = match event {
            Event::ObjectEnd { .. } => break,
            Event::Key { str_ref } => str_ref,
            _ => return Err(depth_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                0, "combined stream wrapper requires object keys")),
        };
        let bit = match cursor_string(cursor, key, DEPTH_SCHEMA_ID, FieldId(0))? {
            "stream" => 1,
            "data" => 2,
            _ => return Err(depth_error(ErrorCode::UnknownObjectKey,
                cursor.state().byte_offset, 0, "unknown combined stream wrapper key")),
        };
        if seen & bit != 0 {
            return Err(depth_error(ErrorCode::DuplicateObjectKey, cursor.state().byte_offset,
                0, "duplicate combined stream wrapper key"));
        }
        seen |= bit;
        let value = next_event(cursor, DEPTH_SCHEMA_ID, FieldId(0))?;
        if bit == 1 {
            match value {
                Event::Scalar { kind: ScalarKind::String, decoded: Some(reference), .. } => {
                    if cursor_string(cursor, reference, DEPTH_SCHEMA_ID, FieldId(0))?.is_empty() {
                        return Err(depth_error(ErrorCode::ConstraintViolation,
                            cursor.state().byte_offset, 0, "combined stream name is empty"));
                    }
                }
                _ => return Err(depth_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                    0, "combined stream name must be a string")),
            }
        } else {
            if !matches!(value, Event::ObjectStart { .. }) {
                return Err(depth_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                    0, "combined stream data must be an object"));
            }
            let first = next_event(cursor, DEPTH_SCHEMA_ID, FieldId(0))?;
            read_members(cursor, first, owner, profile, values)?;
        }
        event = next_event(cursor, DEPTH_SCHEMA_ID, FieldId(0))?;
    }
    if seen != 3 {
        return Err(depth_error(ErrorCode::MissingRequiredField, cursor.state().byte_offset,
            0, "combined stream wrapper requires stream and data"));
    }
    Ok(())
}

fn read_members(
    cursor: &mut Cursor,
    mut event: Event,
    owner: &SlotOwner<'_>,
    profile: &ExchangeProfile,
    values: &mut DepthValues,
) -> Result<()> {
    let mut seen = 0_u8;
    loop {
        let key = match event {
            Event::ObjectEnd { .. } => break,
            Event::Key { str_ref } => str_ref,
            _ => return Err(depth_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                0, "depth object requires field keys")),
        };
        let name = cursor_string(cursor, key, DEPTH_SCHEMA_ID, FieldId(0))?;
        let index = FIELD_NAMES.iter().position(|candidate| *candidate == name)
            .ok_or_else(|| depth_error(ErrorCode::UnknownObjectKey,
                cursor.state().byte_offset, 0, "unknown depth object key"))?;
        let field = FieldId(index as u32 + 1);
        let bit = 1_u8 << index;
        if seen & bit != 0 {
            return Err(depth_error(ErrorCode::DuplicateObjectKey, cursor.state().byte_offset,
                field.0, "duplicate depth object key"));
        }
        seen |= bit;
        let value = next_event(cursor, DEPTH_SCHEMA_ID, field)?;
        match index {
            SLOT_EVENT | SLOT_SYMBOL => {
                let reference = match value {
                    Event::Scalar { kind: ScalarKind::String, decoded: Some(reference), .. } => reference,
                    _ => return Err(depth_error(ErrorCode::TypeMismatch,
                        cursor.state().byte_offset, field.0, "depth field must be a string")),
                };
                if cursor_string(cursor, reference, DEPTH_SCHEMA_ID, field)? != owner.string(index)?.1 {
                    return Err(depth_error(ErrorCode::InternalError, cursor.state().byte_offset,
                        field.0, "depth string slot differs from wire value"));
                }
            }
            SLOT_EVENT_TIME | SLOT_FIRST_UPDATE_ID | SLOT_FINAL_UPDATE_ID => {
                let span = match value {
                    Event::Scalar { kind: ScalarKind::Number, span, .. } => span,
                    _ => return Err(depth_error(ErrorCode::TypeMismatch,
                        cursor.state().byte_offset, field.0, "depth field must be a number")),
                };
                let integer = read_u64(cursor.raw_bytes(), span, field)?;
                if integer != owner.integer(index)? {
                    return Err(depth_error(ErrorCode::InternalError, span.offset, field.0,
                        "depth integer slot differs from wire value"));
                }
            }
            SLOT_BIDS | SLOT_ASKS => {
                let (span, levels) = read_book_levels(cursor, profile, DEPTH_SCHEMA_ID, field)?;
                if span != owner.span(index)? {
                    return Err(depth_error(ErrorCode::InternalError, span.offset, field.0,
                        "depth book slot differs from wire span"));
                }
                if index == SLOT_BIDS { values.bids = levels; } else { values.asks = levels; }
            }
            _ => return Err(depth_error(ErrorCode::InternalError, 0, field.0,
                "invalid frozen depth field index")),
        }
        event = next_event(cursor, DEPTH_SCHEMA_ID, FieldId(0))?;
    }
    for index in 0..SLOT_COUNT {
        if seen & (1_u8 << index) == 0 {
            return Err(depth_error(ErrorCode::MissingRequiredField, cursor.state().byte_offset,
                index as u32 + 1, "missing required depth object key"));
        }
    }
    Ok(())
}

fn read_u64(raw: &[u8], span: Span32, field: FieldId) -> Result<u64> {
    let bytes = span.slice(raw).ok_or_else(|| depth_error(ErrorCode::InternalError,
        span.offset, field.0, "depth integer span is out of bounds"))?;
    if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
        return Err(depth_error(ErrorCode::InvalidNumber, span.offset, field.0,
            "depth integer requires unsigned decimal digits"));
    }
    let mut value = 0_u64;
    for (index, &byte) in bytes.iter().enumerate() {
        // The checked span proves every index and its absolute offset fit u32.
        let offset = span.offset + index as u32;
        if !byte.is_ascii_digit() {
            return Err(depth_error(ErrorCode::InvalidNumber, offset, field.0,
                "depth integer requires unsigned decimal digits"));
        }
        value = value.checked_mul(10).and_then(|n| n.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| depth_error(ErrorCode::NumberOverflow, offset, field.0,
                "depth integer exceeds u64"))?;
    }
    Ok(value)
}

/// Shared with snapshot decoders. `cursor.current()` must be the side's array
/// start. Consumes through its matching end and retains absolute input spans.
/// No proof is issued and no reference into this cursor escapes the function.
pub(crate) fn read_book_levels(
    cursor: &mut Cursor,
    profile: &ExchangeProfile,
    schema: SchemaId,
    field: FieldId,
) -> Result<(Span32, Vec<BookLevel>)> {
    let start = match cursor.current() {
        Some(Event::ArrayStart { offset }) => offset,
        _ => return Err(book_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
            schema, field, "book side must be an array")),
    };
    let mut levels = Vec::new();
    loop {
        match next_event(cursor, schema, field)? {
            Event::ArrayEnd { offset } => {
                let len = offset.checked_sub(start).and_then(|len| len.checked_add(1))
                    .ok_or_else(|| book_error(ErrorCode::InternalError, start, schema,
                        field, "book array span overflows"))?;
                let span = Span32::new(start, len);
                if span.slice(cursor.raw_bytes()).is_none() {
                    return Err(book_error(ErrorCode::InternalError, start, schema, field,
                        "book array span is out of bounds"));
                }
                return Ok((span, levels));
            }
            Event::ArrayStart { offset } => {
                if levels.len() >= profile.max_levels as usize
                    || levels.len() >= profile.limits.max_array_items as usize
                {
                    return Err(book_error(ErrorCode::ConstraintViolation, offset, schema,
                        field, "book side exceeds level limit"));
                }
                let price = read_decimal(cursor, &profile.price, schema, field)?;
                let quantity = read_decimal(cursor, &profile.quantity, schema, field)?;
                if !matches!(next_event(cursor, schema, field)?, Event::ArrayEnd { .. }) {
                    return Err(book_error(ErrorCode::ConstraintViolation,
                        cursor.state().byte_offset, schema, field,
                        "book level must contain exactly two strings"));
                }
                let level = BookLevel { price, quantity };
                profiles::validate_level(level, profile)
                    .map_err(|error| error.with_schema(schema.0, field.0))?;
                levels.try_reserve(1).map_err(|_| book_error(ErrorCode::BudgetExhausted,
                    offset, schema, field, "book level allocation failed"))?;
                levels.push(level);
            }
            _ => return Err(book_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                schema, field, "book level must be a two-string array")),
        }
    }
}

fn read_decimal(
    cursor: &mut Cursor,
    policy: &DecimalPolicy,
    schema: SchemaId,
    field: FieldId,
) -> Result<Decimal128> {
    let (span, reference) = match next_event(cursor, schema, field)? {
        Event::Scalar { kind: ScalarKind::String, span, decoded: Some(reference) } => (span, reference),
        _ => return Err(book_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
            schema, field, "book price and quantity must be strings")),
    };
    let raw = span.slice(cursor.raw_bytes()).ok_or_else(|| book_error(
        ErrorCode::InternalError, span.offset, schema, field, "decimal token span is invalid"))?;
    // Unescaped strings can be parsed directly from their original quoted token.
    // Escaped strings are already fully validated by the cursor; quote their
    // cooked UTF-8 so numeric grammar is applied to the actual string value.
    let value = if !raw.contains(&b'\\') {
        jmax_decimal::parse::parse_quoted(raw, span.offset, policy)
    } else {
        let text = cursor_string(cursor, reference, schema, field)?;
        let size = text.len().checked_add(2).ok_or_else(|| book_error(
            ErrorCode::BudgetExhausted, span.offset, schema, field,
            "decoded decimal token length overflows"))?;
        let mut quoted = Vec::new();
        quoted.try_reserve_exact(size).map_err(|_| book_error(ErrorCode::BudgetExhausted,
            span.offset, schema, field, "decoded decimal allocation failed"))?;
        quoted.push(b'"');
        quoted.extend_from_slice(text.as_bytes());
        quoted.push(b'"');
        jmax_decimal::parse::parse_quoted(&quoted, span.offset, policy)
    };
    value.map_err(|error| error.with_schema(schema.0, field.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, vec};
    use jmax_abi::buffer::ValidationProof;

    const FIXTURE: &[u8] = br#"{"e":"depthUpdate","E":123456789,"s":"BNBBTC","U":157,"u":160,"b":[["0.00240000","10.00000000"]],"a":[["0.00260000","0.00000000"]]}"#;

    struct Captured {
        payload: Payload,
        decoded: Box<[u8]>,
        slots: Vec<SlotValue>,
    }

    // Test extraction intentionally does not claim semantic validation. The
    // production traversal must validate these values before proof issuance.
    fn capture(payload: Payload, profile: &ExchangeProfile) -> Result<Captured> {
        let mut cursor = walk::from_payload(payload.clone(), profile.limits, SkipMode::Strict)?;
        let mut slots = vec![SlotValue::Empty; SLOT_COUNT];
        let mut pending = None;
        let mut book = None;
        let mut depth = 0_u32;
        loop {
            match walk::next(&mut cursor)? {
                ScanStep::Finished => break,
                ScanStep::NeedMore => panic!("complete test payload requested more bytes"),
                ScanStep::Emit(event) => match event {
                    Event::Key { str_ref } => {
                        let name = cursor.resolve_str(str_ref).unwrap();
                        pending = FIELD_NAMES.iter().position(|candidate| *candidate == name);
                    }
                    Event::ObjectStart { .. } => {
                        if let Some(index) = pending.take() {
                            // An object in a scalar/book slot is present, but
                            // has the wrong kind rather than being absent.
                            slots[index] = SlotValue::Bool(false);
                        }
                    }
                    Event::ArrayStart { offset } => {
                        if let Some(index) = pending.take() {
                            if index == SLOT_BIDS || index == SLOT_ASKS {
                                book = Some((index, offset, depth));
                            }
                        }
                        depth += 1;
                    }
                    Event::ArrayEnd { offset } => {
                        depth -= 1;
                        if let Some((index, start, outer_depth)) = book {
                            if depth == outer_depth {
                                slots[index] = SlotValue::RawSpan(Span32::new(start, offset - start + 1));
                                book = None;
                            }
                        }
                    }
                    Event::Scalar { kind, span, decoded } => {
                        if let Some(index) = pending.take() {
                            slots[index] = match kind {
                                ScalarKind::String => SlotValue::Str(decoded.unwrap()),
                                ScalarKind::Number => SlotValue::U64(read_u64(
                                    cursor.raw_bytes(), span, FieldId(index as u32 + 1))?),
                                _ => SlotValue::Bool(kind == ScalarKind::True),
                            };
                        }
                    }
                    Event::ObjectEnd { .. } => { pending = None; }
                },
            }
        }
        Ok(Captured {
            payload,
            decoded: cursor.decoded_bytes().to_vec().into_boxed_slice(),
            slots,
        })
    }

    fn inspect(captured: &Captured, profile: &ExchangeProfile) -> Result<DepthValues> {
        let owner = SlotOwner {
            raw: captured.payload.as_slice(), decoded: &captured.decoded, slots: &captured.slots,
        };
        read_document(captured.payload.clone(), &owner, profile, owner.depth_values(profile)?)
    }

    struct TestValidator { calls: usize }

    impl ExchangeValidator for TestValidator {
        fn validate(&mut self, kind: MessageKind, payload: Payload, profile: &ExchangeProfile) -> Result<ValidatedSlots> {
            self.calls += 1;
            assert_eq!(kind, MessageKind::Depth);
            let captured = capture(payload, profile)?;
            inspect(&captured, profile)?;
            let fingerprint = profiles::schema_fingerprint(kind, profile)?;
            // SAFETY: This test validator has strictly parsed the entire payload
            // and checked all slots, references and profile semantics above.
            // Length conversions are checked and the same owners are returned.
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
            Ok(self.0.take().expect("single validator call"))
        }
    }

    fn decode(bytes: &[u8], profile: &ExchangeProfile) -> Result<DepthMessage> {
        decode_depth(Payload::from_slice(bytes), profile, &mut TestValidator { calls: 0 })
    }

    fn fixture_slots(profile: &ExchangeProfile) -> ValidatedSlots {
        TestValidator { calls: 0 }.validate(MessageKind::Depth,
            Payload::from_slice(FIXTURE), profile).unwrap()
    }

    fn assert_bad_slots(mutator: impl FnOnce(&mut ValidatedSlots), expected: ErrorCode) {
        let profile = ExchangeProfile::default();
        let mut slots = fixture_slots(&profile);
        mutator(&mut slots);
        let result = decode_depth(Payload::from_slice(FIXTURE), &profile,
            &mut ReturnedSlots(Some(slots)));
        let error = result.unwrap_err();
        assert_eq!(error.code, expected, "{error}");
        assert_eq!(error.schema_id, DEPTH_SCHEMA_ID.0);
    }

    fn document(bids: &str, asks: &str) -> String {
        format!(r#"{{"e":"depthUpdate","E":0,"s":"X","U":0,"u":0,"b":{bids},"a":{asks}}}"#)
    }

    #[test]
    fn frozen_schema_and_slot_layout() {
        assert_eq!([SLOT_EVENT, SLOT_EVENT_TIME, SLOT_SYMBOL, SLOT_FIRST_UPDATE_ID,
            SLOT_FINAL_UPDATE_ID, SLOT_BIDS, SLOT_ASKS], [0, 1, 2, 3, 4, 5, 6]);
        let schema = schema_ir();
        assert_eq!(schema.name, "spot_depth_v1");
        assert_eq!(schema.schema_id, DEPTH_SCHEMA_ID);
        assert_eq!(schema.required_count, 7);
        assert!(schema.reject_unknown);
        assert_eq!(schema.fields.len(), SLOT_COUNT);
        for (index, field) in schema.fields.iter().enumerate() {
            assert_eq!(field.field_id, FieldId(index as u32 + 1));
            assert_eq!(field.name, FIELD_NAMES[index]);
            assert!(field.required);
            assert!(field.format.is_none());
            assert!(field.min_i64.is_none() && field.max_i64.is_none());
            assert_eq!(field.min_len, if index == SLOT_SYMBOL { Some(1) } else { None });
            assert_eq!(field.max_len, if index == SLOT_SYMBOL { Some(32) } else { None });
            assert_eq!(field.scalar_kind, match index {
                0 | 2 => Some(ScalarKind::String),
                1 | 3 | 4 => Some(ScalarKind::Number),
                _ => None,
            });
        }
    }

    #[test]
    fn exact_fixture_preserves_deletions_and_owner() {
        let profile = ExchangeProfile::default();
        let payload = Payload::from_slice(FIXTURE);
        let pointer = payload.as_slice().as_ptr();
        let message = decode_depth(payload, &profile, &mut TestValidator { calls: 0 }).unwrap();
        assert_eq!(message.raw_bytes().as_ptr(), pointer);
        assert_eq!(message.raw_bytes(), FIXTURE);
        let values = message.values();
        assert_eq!(values.event_time_ms, 123456789);
        assert_eq!((values.first_update_id, values.final_update_id), (157, 160));
        assert_eq!(message.resolve_str(values.symbol), Some("BNBBTC"));
        assert_eq!(values.bids, [BookLevel {
            price: Decimal128::new(240000, 8), quantity: Decimal128::new(1000000000, 8),
        }]);
        assert_eq!(values.asks[0].price, Decimal128::new(260000, 8));
        assert_eq!(values.asks[0].quantity, Decimal128::new(0, 8));
        assert_eq!(message.proof().schema_fingerprint(),
            profiles::schema_fingerprint(MessageKind::Depth, &profile).unwrap());
    }

    #[test]
    fn empty_books_duplicate_levels_order_and_negative_zero() {
        let profile = ExchangeProfile::default();
        let empty = document("[]", "[]");
        let values = decode(empty.as_bytes(), &profile).unwrap().into_values();
        assert!(values.bids.is_empty() && values.asks.is_empty());
        let text = document(r#"[["2","0"],["1","-0.00000000"],["2","0"]]"#, "[]");
        let values = decode(text.as_bytes(), &profile).unwrap().into_values();
        assert_eq!(values.bids.len(), 3);
        assert_eq!(values.bids[0], values.bids[2]);
        assert_eq!(values.bids[1].price, Decimal128::new(100000000, 8));
        assert_eq!(values.bids[1].quantity, Decimal128::new(0, 8));
    }

    #[test]
    fn u64_boundaries_and_noncontiguous_updates_are_valid() {
        let profile = ExchangeProfile::default();
        let text = format!(r#"{{"u":{},"U":1,"s":"X","e":"depthUpdate","a":[],"b":[],"E":{}}}"#,
            u64::MAX, u64::MAX);
        let message = decode(text.as_bytes(), &profile).unwrap();
        assert_eq!(message.values().event_time_ms, u64::MAX);
        assert_eq!(message.values().final_update_id, u64::MAX);
        for input in ["", "00", "01", "-0", "-1", "+1", "1.0", "1e0", "18446744073709551616"] {
            assert!(read_u64(input.as_bytes(), Span32::new(0, input.len() as u32), FieldId(2)).is_err(), "{input}");
        }
        let error = read_u64(b"18446744073709551616", Span32::new(0, 20), FieldId(2)).unwrap_err();
        assert_eq!(error.code, ErrorCode::NumberOverflow);
        assert_eq!(error.byte_offset, 19);
        assert_eq!(read_u64(b"00", Span32::new(1, 1), FieldId(2)).unwrap(), 0);
        assert_eq!(read_u64(b"1", Span32::new(u32::MAX, 2), FieldId(2)).unwrap_err().code, ErrorCode::InternalError);
    }

    #[test]
    fn escaped_keys_strings_and_unicode_byte_limits() {
        let mut profile = ExchangeProfile::default();
        profile.max_symbol_bytes = 4;
        let text = br#"{"\u0065":"depth\u0055pdate","E":0,"s":"\u00e9\u00e9","U":0,"u":0,"b":[["\u0031","\u0030"]],"a":[]}"#;
        let message = decode(text, &profile).unwrap();
        assert_eq!(message.resolve_str(message.values().symbol), Some("éé"));
        assert_eq!(message.values().symbol.scalar_len, 2);
        assert_eq!(message.values().bids[0].price, Decimal128::new(100000000, 8));
        profile.max_symbol_bytes = 3;
        assert_eq!(decode(text, &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile.max_symbol_bytes = 40;
        let long = document("[]", "[]").replace("\"X\"", &format!("\"{}\"", "S".repeat(40)));
        assert!(decode(long.as_bytes(), &profile).is_ok());
    }

    #[test]
    fn wrapper_coordinates_and_either_policy() {
        let inner = core::str::from_utf8(FIXTURE).unwrap();
        for text in [format!(r#"{{"stream":"bnb@depth","data":{inner}}}"#),
            format!(r#"{{"data":{inner},"stream":"bnb\u0040depth"}}"#)] {
            for wrapper in [WrapperPolicy::CombinedStream, WrapperPolicy::Either] {
                let profile = ExchangeProfile { wrapper, ..ExchangeProfile::default() };
                let message = decode(text.as_bytes(), &profile).unwrap();
                assert_eq!(message.raw_bytes(), text.as_bytes());
                assert_eq!(message.resolve_str(message.values().symbol), Some("BNBBTC"));
                assert!(message.values().symbol.span.offset > 30);
            }
        }
        let either = ExchangeProfile { wrapper: WrapperPolicy::Either, ..ExchangeProfile::default() };
        assert!(decode(FIXTURE, &either).is_ok());
        let combined = ExchangeProfile { wrapper: WrapperPolicy::CombinedStream, ..either };
        assert_eq!(decode(FIXTURE, &combined).unwrap_err().code, ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn level_shape_and_decimal_error_vectors() {
        let profile = ExchangeProfile::default();
        let vectors = [
            (r#"{}"#, ErrorCode::TypeMismatch),
            (r#"[1]"#, ErrorCode::TypeMismatch),
            (r#"[[]]"#, ErrorCode::TypeMismatch),
            (r#"[["1"]]"#, ErrorCode::TypeMismatch),
            (r#"[["1","2","3"]]"#, ErrorCode::ConstraintViolation),
            (r#"[[1,"2"]]"#, ErrorCode::TypeMismatch),
            (r#"[["1",null]]"#, ErrorCode::TypeMismatch),
            (r#"[["0","1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["-1","1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["1","-1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["01","1"]]"#, ErrorCode::InvalidNumber),
            (r#"[["+1","1"]]"#, ErrorCode::InvalidNumber),
            (r#"[[".1","1"]]"#, ErrorCode::InvalidNumber),
            (r#"[["1e0","1"]]"#, ErrorCode::InvalidNumber),
            (r#"[["0.000000001","1"]]"#, ErrorCode::DecimalScaleMismatch),
        ];
        for (book, expected) in vectors {
            let text = document(book, "[]");
            let error = decode(text.as_bytes(), &profile).unwrap_err();
            assert_eq!(error.code, expected, "{book}: {error}");
        }
        let invalid_ask = document("[]", r#"[["1","-1"]]"#);
        let error = decode(invalid_ask.as_bytes(), &profile).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.field_id, 7);
    }

    #[test]
    fn decimal_policy_boundaries_precision_tick_and_truncation() {
        let mut profile = ExchangeProfile::default();
        profile.price.target_scale = 2;
        profile.price.max_precision = 3;
        profile.price.tick_step = 5;
        profile.quantity.target_scale = 0;
        let valid = document(r#"[["1.25","0"]]"#, "[]");
        assert_eq!(decode(valid.as_bytes(), &profile).unwrap().values().bids[0].price,
            Decimal128::new(125, 2));
        for (price, expected) in [("1.26", ErrorCode::TickMultipleViolation),
            ("10.00", ErrorCode::DecimalPrecisionOverflow),
            ("1.251", ErrorCode::DecimalScaleMismatch)] {
            let text = document(&format!(r#"[["{price}","0"]]"#), "[]");
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, expected);
        }
        profile.price.reject_inexact_rescale = false;
        let text = document(r#"[["1.259","0"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap().values().bids[0].price,
            Decimal128::new(125, 2));
        profile.price.allow_exponent = true;
        let text = document(r#"[["125e-2","0"]]"#, "[]");
        assert!(decode(text.as_bytes(), &profile).is_ok());
        profile.price.max_precision = 38;
        profile.price.target_scale = 0;
        profile.price.tick_step = 0;
        let text = document(r#"[["99999999999999999999999999999999999999","0"]]"#, "[]");
        assert!(decode(text.as_bytes(), &profile).is_ok());
        let text = document(r#"[["999999999999999999999999999999999999999","0"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, ErrorCode::NumberOverflow);
    }

    #[test]
    fn independent_side_limits_and_syntax_budgets() {
        let mut profile = ExchangeProfile::default();
        profile.max_levels = 1;
        assert!(decode(FIXTURE, &profile).is_ok());
        let text = document(r#"[["1","0"],["2","0"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile.max_levels = 2;
        profile.limits.max_array_items = 2;
        assert!(decode(text.as_bytes(), &profile).is_ok());
        profile.limits.max_array_items = 1;
        assert_eq!(decode(FIXTURE, &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile = ExchangeProfile::default();
        profile.limits.max_payload_bytes = FIXTURE.len() as u32;
        assert!(decode(FIXTURE, &profile).is_ok());
        profile.limits.max_payload_bytes -= 1;
        assert_eq!(decode(FIXTURE, &profile).unwrap_err().code, ErrorCode::PayloadTooLarge);
        profile = ExchangeProfile::default();
        profile.limits.max_depth = 2;
        assert_eq!(decode(FIXTURE, &profile).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        profile = ExchangeProfile::default();
        profile.limits.max_string_bytes = 5;
        assert!(decode(FIXTURE, &profile).is_err());
        profile = ExchangeProfile::default();
        profile.limits.max_number_digits = 2;
        assert!(decode(FIXTURE, &profile).is_err());
    }

    #[test]
    fn corrupt_slots_cannot_publish_an_owner() {
        assert_bad_slots(|s| { s.slots.pop(); }, ErrorCode::MissingRequiredField);
        assert_bad_slots(|s| s.slots.push(SlotValue::Empty), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[1] = SlotValue::Empty, ErrorCode::MissingRequiredField);
        assert_bad_slots(|s| s.slots[1] = SlotValue::I64(123456789), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[1] = SlotValue::U64(1), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[3] = SlotValue::U64(161), ErrorCode::ConstraintViolation);
        assert_bad_slots(|s| s.slots[5] = SlotValue::Bool(false), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[5] = SlotValue::RawSpan(Span32::new(u32::MAX, 1)), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots.swap(5, 6), ErrorCode::InternalError);
        assert_bad_slots(|s| {
            if let SlotValue::Str(mut value) = s.slots[2] {
                value.scalar_len += 1;
                s.slots[2] = SlotValue::Str(value);
            }
        }, ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[2] = SlotValue::Str(StrRef {
            source: SpanSource::DecodedArena, span: Span32::new(0, 1), scalar_len: 1,
        }), ErrorCode::InternalError);
        assert_bad_slots(|s| { s.decoded = vec![b'x'].into_boxed_slice(); }, ErrorCode::InternalError);
        assert_bad_slots(|s| { s.payload = Payload::from_slice(b"{}"); }, ErrorCode::InternalError);
        assert_bad_slots(|s| {
            let SlotValue::Str(mut event) = s.slots[0] else { panic!("event slot") };
            event.span.len = 1;
            event.scalar_len = 1;
            s.slots[0] = SlotValue::Str(event);
        }, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn fingerprint_and_validator_errors_are_preserved() {
        let profile = ExchangeProfile::default();
        let other = ExchangeProfile { max_levels: 1, ..profile };
        let slots = fixture_slots(&other);
        let error = decode_depth(Payload::from_slice(FIXTURE), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap_err();
        assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
        struct Failing(Error);
        impl ExchangeValidator for Failing {
            fn validate(&mut self, _: MessageKind, _: Payload, _: &ExchangeProfile) -> Result<ValidatedSlots> {
                Err(self.0.clone())
            }
        }
        let expected = Error::new(ErrorCode::Cancelled, ErrorSource::SchemaVm, 17,
            "test validator failure").with_schema(7, 8);
        let error = decode_depth(Payload::from_slice(FIXTURE), &profile,
            &mut Failing(expected.clone())).unwrap_err();
        assert_eq!(error, expected);
        let mut validator = TestValidator { calls: 0 };
        let invalid = ExchangeProfile { version: 2, ..profile };
        assert_eq!(decode_depth(Payload::from_slice(b"invalid"), &invalid,
            &mut validator).unwrap_err().code, ErrorCode::UnsupportedSchemaFeature);
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn decoded_arena_is_retained_and_its_budget_is_enforced() {
        let profile = ExchangeProfile::default();
        let text = br#"{"e":"depthUpdate","E":0,"s":"\u0042TC","U":0,"u":0,"b":[],"a":[]}"#;
        let slots = TestValidator { calls: 0 }.validate(MessageKind::Depth,
            Payload::from_slice(text), &profile).unwrap();
        let decoded_pointer = slots.decoded.as_ptr();
        let decoded_copy = slots.decoded.to_vec();
        let message = decode_depth(Payload::from_slice(text), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap();
        assert_eq!(message.decoded_bytes().as_ptr(), decoded_pointer);
        assert_eq!(message.decoded_bytes(), decoded_copy.as_slice());
        assert_eq!(message.values().symbol.source, SpanSource::DecodedArena);
        assert_eq!(message.resolve_str(message.values().symbol), Some("BTC"));
        let limited = ExchangeProfile { limits: Limits {
            max_decoded_bytes: 0, ..profile.limits
        }, ..profile };
        assert_eq!(decode(text, &limited).unwrap_err().code, ErrorCode::DecodedArenaOverflow);

        // A public arena mutation may preserve proof lengths. Its strings must
        // still be checked for UTF-8 and wire-value agreement before publication.
        let mut slots = TestValidator { calls: 0 }.validate(MessageKind::Depth,
            Payload::from_slice(text), &profile).unwrap();
        let SlotValue::Str(reference) = slots.slots[SLOT_SYMBOL] else {
            panic!("symbol string slot")
        };
        slots.decoded[reference.span.offset as usize] = 0xff;
        let error = decode_depth(Payload::from_slice(text), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidUtf8);
        assert_eq!(error.field_id, 3);
    }

    #[test]
    fn metadata_constraints_and_missing_members() {
        let profile = ExchangeProfile::default();
        let original = core::str::from_utf8(FIXTURE).unwrap();
        for (text, expected) in [
            (original.replace("depthUpdate", "aggTrade"), ErrorCode::ConstraintViolation),
            (original.replace("BNBBTC", ""), ErrorCode::ConstraintViolation),
            (original.replace("\"U\":157", "\"U\":161"), ErrorCode::ConstraintViolation),
            (original.replace("\"E\":123456789,", ""), ErrorCode::MissingRequiredField),
            (original.replace("\"E\":123456789", "\"E\":\"123456789\""), ErrorCode::TypeMismatch),
            (original.replace("\"u\":160", "\"u\":-1"), ErrorCode::InvalidNumber),
            (original.replace("\"u\":160", "\"u\":1.0"), ErrorCode::InvalidNumber),
            (original.replace("\"u\":160", "\"u\":1e2"), ErrorCode::InvalidNumber),
        ] {
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, expected, "{text}");
        }
    }

    #[test]
    fn strict_tail_and_wrapper_validation_with_untrusted_slots() {
        let profile = ExchangeProfile::default();
        // Use no proof here: exercise independent wire validation with arbitrary
        // slot assertions, including suffixes the adapter might have ignored.
        let captured = capture(Payload::from_slice(FIXTURE), &profile).unwrap();
        for suffix in ["x", "{}", ",", "\0"] {
            let mut bytes = FIXTURE.to_vec();
            bytes.extend_from_slice(suffix.as_bytes());
            let owner = SlotOwner { raw: &bytes, decoded: &captured.decoded, slots: &captured.slots };
            assert!(read_document(Payload::from_slice(&bytes), &owner, &profile,
                owner.depth_values(&profile).unwrap()).is_err());
        }
        let mut permissive = profile;
        permissive.limits.reject_duplicate_keys = false;
        permissive.limits.reject_unknown_keys = false;
        let original = core::str::from_utf8(FIXTURE).unwrap();
        for text in [original.replacen("\"a\":", "\"b\":", 1),
            original.replacen("\"a\":", "\"z\":", 1)] {
            let candidate = capture(Payload::from_slice(text.as_bytes()), &permissive).unwrap();
            // Keep the valid fixture's slot assertions so missing or unknown
            // wire members are diagnosed by traversal rather than slot preflight.
            let owner = SlotOwner { raw: candidate.payload.as_slice(), decoded: &candidate.decoded,
                slots: &captured.slots };
            let error = read_document(candidate.payload.clone(), &owner, &permissive,
                owner.depth_values(&permissive).unwrap()).unwrap_err();
            assert!(matches!(error.code, ErrorCode::DuplicateObjectKey | ErrorCode::UnknownObjectKey));
        }
        let wrapper_profile = ExchangeProfile { wrapper: WrapperPolicy::Either, ..permissive };
        for (text, expected) in [
            (format!(r#"{{"data":{original}}}"#), ErrorCode::MissingRequiredField),
            (format!(r#"{{"stream":"","data":{original}}}"#), ErrorCode::ConstraintViolation),
            (format!(r#"{{"stream":7,"data":{original}}}"#), ErrorCode::TypeMismatch),
            (format!(r#"{{"stream":"x","data":{original},"extra":0}}"#), ErrorCode::UnknownObjectKey),
            (format!(r#"{{"stream":"x","data":{original},"stream":"x"}}"#), ErrorCode::DuplicateObjectKey),
        ] {
            let candidate = capture(Payload::from_slice(text.as_bytes()), &wrapper_profile).unwrap();
            let error = inspect(&candidate, &wrapper_profile).unwrap_err();
            assert_eq!(error.code, expected, "{text}: {error}");
        }
    }

    #[test]
    fn malformed_documents_utf8_and_string_references_fail() {
        let profile = ExchangeProfile::default();
        for bytes in [&b""[..], &b"{"[..], &b"[]"[..], &b"null"[..],
            &b"{\"e\":\"depthUpdate\",}"[..], &b"{\"e\":\"\\uD800\"}"[..],
            &b"{\"e\":\"\xff\"}"[..]] {
            assert!(decode(bytes, &profile).is_err(), "{bytes:?}");
        }
        let slots = [SlotValue::Str(StrRef {
            span: Span32::new(0, 1), source: SpanSource::RawPayload, scalar_len: 1,
        })];
        let owner = SlotOwner { raw: b"\xff", decoded: &[], slots: &slots };
        assert_eq!(owner.string(0).unwrap_err().code, ErrorCode::InvalidUtf8);
        let slots = [SlotValue::Str(StrRef {
            span: Span32::new(u32::MAX, 1), source: SpanSource::RawPayload, scalar_len: 0,
        })];
        let owner = SlotOwner { raw: b"", decoded: &[], slots: &slots };
        assert_eq!(owner.string(0).unwrap_err().code, ErrorCode::InternalError);
    }
}
