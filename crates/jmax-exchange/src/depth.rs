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
