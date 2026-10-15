//! REST depth snapshots and strict, owner-independent combined-stream unwrapping.

use super::*;
use jmax_abi::syntax::{Event, ScanStep};
use jmax_cursor::{skip, walk, Cursor, SkipMode, SkipStep};

pub const SLOT_LAST_UPDATE_ID: usize = 0;
pub const SLOT_BIDS: usize = 1;
pub const SLOT_ASKS: usize = 2;
pub const SLOT_COUNT: usize = 3;

const FIELD_NAMES: [&str; SLOT_COUNT] = ["lastUpdateId", "bids", "asks"];

/// Default-profile IR. The validator must additionally enforce unsigned IDs,
/// exact two-string book levels, and the supplied profile's decimal policies.
pub fn schema_ir() -> SchemaIr {
    SchemaIr {
        name: String::from("spot_rest_depth_v1"),
        schema_id: SNAPSHOT_SCHEMA_ID,
        required_count: SLOT_COUNT as u16,
        reject_unknown: true,
        fields: FIELD_NAMES.iter().enumerate().map(|(index, name)| SchemaFieldIr {
            field_id: FieldId(index as u32 + 1),
            name: String::from(*name),
            required: true,
            scalar_kind: if index == SLOT_LAST_UPDATE_ID {
                Some(ScalarKind::Number)
            } else {
                None
            },
            format: None,
            min_len: None,
            max_len: None,
            min_i64: None,
            max_i64: None,
        }).collect(),
    }
}

/// Decode a snapshot while preserving the complete validator owner and spans.
/// Publicly mutable slot assertions are independently checked against the wire
/// before their proof is used to publish an immutable message.
pub fn decode_snapshot(
    payload: Payload,
    profile: &ExchangeProfile,
    validator: &mut dyn ExchangeValidator,
) -> Result<SnapshotMessage> {
    profiles::validate_profile(profile)?;
    check_payload_len(&payload, profile.limits).map_err(snapshot_context)?;
    let fingerprint = profiles::schema_fingerprint(MessageKind::Snapshot, profile)?;
    let slots = validator.validate(MessageKind::Snapshot, payload.clone(), profile)?;
    ensure_slot_owner(&slots, payload.as_slice()).map_err(snapshot_context)?;
    if slots.proof.schema_fingerprint() != fingerprint {
        return Err(snapshot_error(ErrorCode::SchemaImageInvalid, 0, 0,
            "snapshot slot fingerprint differs from profile"));
    }
    if slots.decoded.len() > profile.limits.max_decoded_bytes as usize {
        return Err(snapshot_error(ErrorCode::DecodedArenaOverflow, 0, 0,
            "snapshot decoded arena exceeds byte limit"));
    }
    let owner = SlotOwner { raw: slots.payload.as_slice(), slots: &slots.slots };
    owner.validate()?;
    let values = read_document(
        slots.payload.clone(), profile.wrapper, profile.limits, false,
        |cursor, first| read_members(cursor, first, &owner, profile),
    ).map_err(snapshot_context)?.value;

    // SAFETY: The validator's original payload, decoded arena and proof are
    // moved intact. Owner bytes, proof lengths, fingerprint and the exact slot
    // layout were checked. Strict traversal through EOF independently validated
    // the entire wrapper and snapshot, matched all slot values and absolute
    // book spans to the wire, and checked every book level and decimal policy.
    // SnapshotValues contains only a u64 and owned Decimal128 vectors; no StrRef,
    // cursor, scratch, arena or temporary-payload reference is emitted.
    Ok(unsafe {
        ParsedMessage::new_proven(slots.payload, slots.decoded.into_boxed_slice(), values, slots.proof)
    })
}

/// Validate JSON and wrapper structure without issuing exchange validation.
///
/// Bare input retains its original allocation and whitespace. A combined stream
/// produces an independent payload containing exactly the lexical data object,
/// plus an owned, decoded stream name. This utility does not check an exchange
/// schema or book semantics. Decode its result separately with a Bare profile.
pub fn unwrap_payload(
    payload: Payload,
    policy: WrapperPolicy,
    limits: Limits,
) -> Result<UnwrappedPayload> {
    check_payload_len(&payload, limits)?;
    let document = read_document(payload.clone(), policy, limits, true, |cursor, first| {
        read_untyped_members(cursor, first, policy)
    })?;
    match document.data_span {
        None => Ok(UnwrappedPayload { payload, stream: None }),
        Some(span) => {
            let bytes = span.slice(payload.as_slice()).ok_or_else(|| envelope_error(
                ErrorCode::InternalError, span.offset, "combined stream data span is invalid"))?;
            let mut copy = Vec::new();
            copy.try_reserve_exact(bytes.len()).map_err(|_| envelope_error(
                ErrorCode::BudgetExhausted, span.offset, "combined stream data allocation failed"))?;
            copy.extend_from_slice(bytes);
            Ok(UnwrappedPayload { payload: Payload::from_vec(copy), stream: document.stream })
        }
    }
}

fn envelope_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, offset, detail)
}

fn snapshot_error(code: ErrorCode, offset: u32, field: u32, detail: &'static str) -> Error {
    envelope_error(code, offset, detail).with_schema(SNAPSHOT_SCHEMA_ID.0, field)
}

fn snapshot_context(error: Error) -> Error {
    snapshot_field_context(error, 0)
}

fn snapshot_field_context(error: Error, field: u32) -> Error {
    if error.source == ErrorSource::Decode && error.schema_id == 0 {
        let field = if error.field_id == 0 { field } else { error.field_id };
        error.with_schema(SNAPSHOT_SCHEMA_ID.0, field)
    } else {
        error
    }
}

fn check_payload_len(payload: &Payload, limits: Limits) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| envelope_error(
        ErrorCode::PayloadTooLarge, u32::MAX, "exchange payload exceeds ABI length"))?;
    if len > limits.max_payload_bytes {
        return Err(envelope_error(ErrorCode::PayloadTooLarge, 0,
            "exchange payload exceeds byte limit"));
    }
    Ok(())
}

struct SlotOwner<'a> {
    raw: &'a [u8],
    slots: &'a [SlotValue],
}

impl SlotOwner<'_> {
    fn required(&self, index: usize) -> Result<SlotValue> {
        match self.slots.get(index).copied() {
            None | Some(SlotValue::Empty) => Err(snapshot_error(
                ErrorCode::MissingRequiredField, 0, index as u32 + 1,
                "missing required snapshot slot")),
            Some(value) => Ok(value),
        }
    }

    fn last_update_id(&self) -> Result<u64> {
        match self.required(SLOT_LAST_UPDATE_ID)? {
            SlotValue::U64(value) => Ok(value),
            _ => Err(snapshot_error(ErrorCode::TypeMismatch, 0, 1,
                "snapshot update ID slot must be an unsigned integer")),
        }
    }

    fn book_span(&self, index: usize) -> Result<Span32> {
        match self.required(index)? {
            SlotValue::RawSpan(span) if span.slice(self.raw).is_some() => Ok(span),
            SlotValue::RawSpan(span) => Err(snapshot_error(ErrorCode::InternalError,
                span.offset, index as u32 + 1, "snapshot book span is out of bounds")),
            _ => Err(snapshot_error(ErrorCode::TypeMismatch, 0, index as u32 + 1,
                "snapshot book slot must be a raw span")),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.slots.len() > SLOT_COUNT {
            return Err(snapshot_error(ErrorCode::TypeMismatch, 0, 0,
                "snapshot validator returned extra slots"));
        }
        self.last_update_id()?;
        self.book_span(SLOT_BIDS)?;
        self.book_span(SLOT_ASKS)?;
        Ok(())
    }
}

fn next_event(cursor: &mut Cursor) -> Result<Event> {
    match walk::next(cursor)? {
        ScanStep::Emit(event) => Ok(event),
        ScanStep::Finished | ScanStep::NeedMore => Err(envelope_error(
            ErrorCode::UnexpectedEof, cursor.state().byte_offset,
            "expected another exchange JSON event")),
    }
}

fn cursor_string(cursor: &Cursor, reference: StrRef) -> Result<&str> {
    let text = cursor.resolve_str(reference).ok_or_else(|| envelope_error(
        ErrorCode::InternalError, cursor.state().byte_offset,
        "exchange cursor string does not resolve"))?;
    if text.chars().count() != reference.scalar_len as usize {
        return Err(envelope_error(ErrorCode::InternalError, cursor.state().byte_offset,
            "exchange cursor string scalar count is incorrect"));
    }
    Ok(text)
}

struct ReadDocument<T> {
    value: T,
    stream: Option<String>,
    data_span: Option<Span32>,
}

/// `members` starts after the object's opening event and must consume its end.
/// All traversal uses the complete original payload, preserving absolute spans.
fn read_document<T, F>(
    payload: Payload,
    policy: WrapperPolicy,
    limits: Limits,
    retain_stream: bool,
    mut members: F,
) -> Result<ReadDocument<T>>
where
    F: FnMut(&mut Cursor, Event) -> Result<T>,
{
    let mut cursor = walk::from_payload(payload, limits, SkipMode::Strict)?;
    if !matches!(next_event(&mut cursor)?, Event::ObjectStart { .. }) {
        return Err(envelope_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
            "exchange document must be an object"));
    }
    let first = next_event(&mut cursor)?;
    let wrapped = match policy {
        WrapperPolicy::Bare => false,
        WrapperPolicy::CombinedStream => true,
        WrapperPolicy::Either => match first {
            Event::Key { str_ref } => matches!(cursor_string(&cursor, str_ref)?, "stream" | "data"),
            _ => false,
        },
    };
    let document = if wrapped {
        read_wrapper(&mut cursor, first, retain_stream, &mut members)?
    } else {
        ReadDocument { value: members(&mut cursor, first)?, stream: None, data_span: None }
    };
    match walk::next(&mut cursor)? {
        ScanStep::Finished => Ok(document),
        ScanStep::NeedMore => Err(envelope_error(ErrorCode::UnexpectedEof,
            cursor.state().byte_offset, "exchange document did not finish")),
        ScanStep::Emit(_) => Err(envelope_error(ErrorCode::TrailingContent,
            cursor.state().byte_offset, "trailing exchange document content")),
    }
}

fn read_wrapper<T, F>(
    cursor: &mut Cursor,
    mut event: Event,
    retain_stream: bool,
    members: &mut F,
) -> Result<ReadDocument<T>>
where
    F: FnMut(&mut Cursor, Event) -> Result<T>,
{
    let mut seen = 0_u8;
    let mut value = None;
    let mut stream = None;
    let mut data_span = None;
    loop {
        let key = match event {
            Event::ObjectEnd { .. } => break,
            Event::Key { str_ref } => str_ref,
            _ => return Err(envelope_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                "combined stream wrapper requires object keys")),
        };
        let bit = match cursor_string(cursor, key)? {
            "stream" => 1,
            "data" => 2,
            _ => return Err(envelope_error(ErrorCode::UnknownObjectKey,
                cursor.state().byte_offset, "unknown combined stream wrapper key")),
        };
        if seen & bit != 0 {
            return Err(envelope_error(ErrorCode::DuplicateObjectKey,
                cursor.state().byte_offset, "duplicate combined stream wrapper key"));
        }
        seen |= bit;
        match (bit, next_event(cursor)?) {
            (1, Event::Scalar { kind: ScalarKind::String, span, decoded: Some(reference) }) => {
                let text = cursor_string(cursor, reference)?;
                if text.is_empty() {
                    return Err(envelope_error(ErrorCode::ConstraintViolation, span.offset,
                        "combined stream name must be nonempty"));
                }
                if retain_stream {
                    let mut copy = String::new();
                    copy.try_reserve_exact(text.len()).map_err(|_| envelope_error(
                        ErrorCode::BudgetExhausted, span.offset, "combined stream name allocation failed"))?;
                    copy.push_str(text);
                    stream = Some(copy);
                }
            }
            (2, Event::ObjectStart { offset: start }) => {
                let first = next_event(cursor)?;
                value = Some(members(cursor, first)?);
                let close = match cursor.current() {
                    Some(Event::ObjectEnd { offset }) => offset,
                    _ => return Err(envelope_error(ErrorCode::InternalError,
                        cursor.state().byte_offset, "combined stream data reader did not finish its object")),
                };
                let len = close.checked_sub(start).and_then(|len| len.checked_add(1))
                    .ok_or_else(|| envelope_error(ErrorCode::InternalError, start,
                        "combined stream data span overflows"))?;
                let span = Span32::new(start, len);
                if span.slice(cursor.raw_bytes()).is_none() {
                    return Err(envelope_error(ErrorCode::InternalError, start,
                        "combined stream data span is out of bounds"));
                }
                data_span = Some(span);
            }
            (1, _) => return Err(envelope_error(ErrorCode::TypeMismatch,
                cursor.state().byte_offset, "combined stream name must be a string")),
            _ => return Err(envelope_error(ErrorCode::TypeMismatch,
                cursor.state().byte_offset, "combined stream data must be an object")),
        }
        event = next_event(cursor)?;
    }
    if seen != 3 {
        return Err(envelope_error(ErrorCode::MissingRequiredField, cursor.state().byte_offset,
            "combined stream wrapper requires stream and data"));
    }
    Ok(ReadDocument {
        value: value.ok_or_else(|| envelope_error(ErrorCode::MissingRequiredField,
            cursor.state().byte_offset, "combined stream wrapper is missing data"))?,
        stream,
        data_span,
    })
}

/// The utility has no kind-specific schema. Validate each arbitrary value,
/// enforcing wrapper markers even if an unrelated top-level key preceded them.
/// This prevents Either from accepting a malformed wrapper as bare input.
fn read_untyped_members(
    cursor: &mut Cursor,
    mut event: Event,
    policy: WrapperPolicy,
) -> Result<()> {
    // When entered for combined data, the root is still open below this object.
    let wrapped = cursor.state().depth > 1;
    loop {
        match event {
            Event::ObjectEnd { .. } => return Ok(()),
            Event::Key { str_ref } => {
                if !wrapped && matches!(cursor_string(cursor, str_ref)?, "stream" | "data") {
                    let detail = match policy {
                        WrapperPolicy::Bare => "combined stream wrapper is forbidden by Bare policy",
                        _ => "combined stream wrapper contains an additional key",
                    };
                    return Err(envelope_error(ErrorCode::UnknownObjectKey,
                        cursor.state().byte_offset, detail));
                }
                next_event(cursor)?;
                match skip::subtree(cursor, SkipMode::Strict)? {
                    SkipStep::Complete { .. } => {}
                    SkipStep::NeedMore => return Err(envelope_error(ErrorCode::UnexpectedEof,
                        cursor.state().byte_offset, "exchange object value is incomplete")),
                }
            }
            _ => return Err(envelope_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                "exchange object requires field keys")),
        }
        event = next_event(cursor)?;
    }
}

fn read_members(
    cursor: &mut Cursor,
    mut event: Event,
    owner: &SlotOwner<'_>,
    profile: &ExchangeProfile,
) -> Result<SnapshotValues> {
    let mut seen = 0_u8;
    let mut bids = Vec::new();
    let mut asks = Vec::new();
    loop {
        let key = match event {
            Event::ObjectEnd { .. } => break,
            Event::Key { str_ref } => str_ref,
            _ => return Err(snapshot_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                0, "snapshot object requires field keys")),
        };
        let name = cursor_string(cursor, key)?;
        let index = FIELD_NAMES.iter().position(|candidate| *candidate == name)
            .ok_or_else(|| snapshot_error(ErrorCode::UnknownObjectKey,
                cursor.state().byte_offset, 0, "unknown snapshot object key"))?;
        let field = index as u32 + 1;
        let bit = 1_u8 << index;
        if seen & bit != 0 {
            return Err(snapshot_error(ErrorCode::DuplicateObjectKey, cursor.state().byte_offset,
                field, "duplicate snapshot object key"));
        }
        seen |= bit;
        let value = next_event(cursor).map_err(|error| snapshot_field_context(error, field))?;
        if index == SLOT_LAST_UPDATE_ID {
            let span = match value {
                Event::Scalar { kind: ScalarKind::Number, span, .. } => span,
                _ => return Err(snapshot_error(ErrorCode::TypeMismatch, cursor.state().byte_offset,
                    field, "snapshot update ID must be a number")),
            };
            if read_u64(cursor.raw_bytes(), span)? != owner.last_update_id()? {
                return Err(snapshot_error(ErrorCode::InternalError, span.offset, field,
                    "snapshot update ID slot differs from wire value"));
            }
        } else {
            let (span, levels) = depth::read_book_levels(cursor, profile, SNAPSHOT_SCHEMA_ID, FieldId(field))?;
            if span != owner.book_span(index)? {
                return Err(snapshot_error(ErrorCode::InternalError, span.offset, field,
                    "snapshot book slot differs from wire span"));
            }
            if index == SLOT_BIDS { bids = levels; } else { asks = levels; }
        }
        event = next_event(cursor)?;
    }
    for index in 0..SLOT_COUNT {
        if seen & (1_u8 << index) == 0 {
            return Err(snapshot_error(ErrorCode::MissingRequiredField, cursor.state().byte_offset,
                index as u32 + 1, "missing required snapshot object key"));
        }
    }
    Ok(SnapshotValues { last_update_id: owner.last_update_id()?, bids, asks })
}

fn read_u64(raw: &[u8], span: Span32) -> Result<u64> {
    let bytes = span.slice(raw).ok_or_else(|| snapshot_error(ErrorCode::InternalError,
        span.offset, 1, "snapshot update ID span is out of bounds"))?;
    if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
        return Err(snapshot_error(ErrorCode::InvalidNumber, span.offset, 1,
            "snapshot update ID requires unsigned decimal digits"));
    }
    let mut value = 0_u64;
    for (index, &byte) in bytes.iter().enumerate() {
        // Checked span bounds prove each visited absolute coordinate fits u32.
        let offset = span.offset + index as u32;
        if !byte.is_ascii_digit() {
            return Err(snapshot_error(ErrorCode::InvalidNumber, offset, 1,
                "snapshot update ID requires unsigned decimal digits"));
        }
        value = value.checked_mul(10).and_then(|n| n.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| snapshot_error(ErrorCode::NumberOverflow, offset, 1,
                "snapshot update ID exceeds u64"))?;
    }
    Ok(value)
}
