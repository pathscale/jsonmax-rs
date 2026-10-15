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
        ParsedMessage::new_proven(slots.payload, slots.decoded, values, slots.proof)
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, format, vec};
    use jmax_abi::buffer::ValidationProof;

    const FIXTURE: &[u8] = br#"{"lastUpdateId":1027024,"bids":[["4.00000000","431.00000000"]],"asks":[["4.00000200","0.00000000"]]}"#;
    const EMPTY: &[u8] = br#"{"lastUpdateId":0,"bids":[],"asks":[]}"#;

    struct Captured {
        payload: Payload,
        decoded: Box<[u8]>,
        slots: Vec<SlotValue>,
    }

    // Extract assertions without issuing a proof. Strict semantic inspection
    // below is required before this test adapter can attest to these owners.
    fn capture(payload: Payload, profile: &ExchangeProfile) -> Result<Captured> {
        let mut cursor = walk::from_payload(payload.clone(), profile.limits, SkipMode::Strict)?;
        let mut slots = vec![SlotValue::Empty; SLOT_COUNT];
        let mut pending = None;
        loop {
            match walk::next(&mut cursor)? {
                ScanStep::Finished => break,
                ScanStep::NeedMore => panic!("complete snapshot input requested more bytes"),
                ScanStep::Emit(Event::Key { str_ref }) => {
                    let name = cursor_string(&cursor, str_ref)?;
                    pending = FIELD_NAMES.iter().position(|candidate| *candidate == name);
                }
                ScanStep::Emit(Event::Scalar { kind, span, decoded }) => {
                    if let Some(index) = pending.take() {
                        slots[index] = match kind {
                            ScalarKind::Number => SlotValue::U64(read_u64(cursor.raw_bytes(), span)?),
                            ScalarKind::String => SlotValue::Str(decoded.expect("cursor string reference")),
                            ScalarKind::True => SlotValue::Bool(true),
                            ScalarKind::False => SlotValue::Bool(false),
                            ScalarKind::Null => SlotValue::RawSpan(span),
                        };
                    }
                }
                ScanStep::Emit(Event::ObjectStart { .. } | Event::ArrayStart { .. }) => {
                    if let Some(index) = pending.take() {
                        let span = match skip::subtree(&mut cursor, SkipMode::Strict)? {
                            SkipStep::Complete { value, .. } => value.span,
                            SkipStep::NeedMore => panic!("complete snapshot subtree requested more bytes"),
                        };
                        slots[index] = SlotValue::RawSpan(span);
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

    fn inspect(captured: &Captured, profile: &ExchangeProfile) -> Result<SnapshotValues> {
        let owner = SlotOwner { raw: captured.payload.as_slice(), slots: &captured.slots };
        owner.validate()?;
        Ok(read_document(captured.payload.clone(), profile.wrapper, profile.limits, false,
            |cursor, first| read_members(cursor, first, &owner, profile))
            .map_err(snapshot_context)?.value)
    }

    struct TestValidator { calls: usize }

    impl ExchangeValidator for TestValidator {
        fn validate(
            &mut self,
            kind: MessageKind,
            payload: Payload,
            profile: &ExchangeProfile,
        ) -> Result<ValidatedSlots> {
            self.calls += 1;
            assert_eq!(kind, MessageKind::Snapshot);
            profiles::validate_profile(profile)?;
            let captured = capture(payload, profile)?;
            inspect(&captured, profile)?;
            // SAFETY: Extraction strictly traversed the complete input, and
            // inspect checked wrapper/schema membership, all asserted slots and
            // spans, book shapes and decimal policies. The cursor created the
            // decoded arena with valid UTF-8 and scalar counts. Checked lengths
            // and the same immutable owners accompany this schema fingerprint.
            let proof = unsafe {
                ValidationProof::issue_unchecked(
                    profiles::schema_fingerprint(kind, profile)?,
                    u32::try_from(captured.payload.len()).unwrap(),
                    u32::try_from(captured.decoded.len()).unwrap(),
                )
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

    fn decode(bytes: &[u8], profile: &ExchangeProfile) -> Result<SnapshotMessage> {
        decode_snapshot(Payload::from_slice(bytes), profile, &mut TestValidator { calls: 0 })
    }

    fn unwrap(bytes: &[u8], policy: WrapperPolicy) -> Result<UnwrappedPayload> {
        unwrap_payload(Payload::from_slice(bytes), policy, Limits::strict_default())
    }

    fn document(id: &str, bids: &str, asks: &str) -> String {
        format!(r#"{{"lastUpdateId":{id},"bids":{bids},"asks":{asks}}}"#)
    }

    fn fixture_slots(profile: &ExchangeProfile) -> ValidatedSlots {
        TestValidator { calls: 0 }.validate(MessageKind::Snapshot,
            Payload::from_slice(FIXTURE), profile).unwrap()
    }

    fn assert_bad_slots(mutator: impl FnOnce(&mut ValidatedSlots), expected: ErrorCode) {
        let profile = ExchangeProfile::default();
        let mut slots = fixture_slots(&profile);
        mutator(&mut slots);
        let error = decode_snapshot(Payload::from_slice(FIXTURE), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap_err();
        assert_eq!(error.code, expected, "{error}");
        assert_eq!(error.schema_id, SNAPSHOT_SCHEMA_ID.0);
    }

    #[test]
    fn frozen_schema_and_slot_layout() {
        assert_eq!([SLOT_LAST_UPDATE_ID, SLOT_BIDS, SLOT_ASKS, SLOT_COUNT], [0, 1, 2, 3]);
        let schema = schema_ir();
        assert_eq!(schema.name, "spot_rest_depth_v1");
        assert_eq!(schema.schema_id, SNAPSHOT_SCHEMA_ID);
        assert_eq!(schema.required_count, 3);
        assert!(schema.reject_unknown);
        assert_eq!(schema.fields.len(), SLOT_COUNT);
        for (index, field) in schema.fields.iter().enumerate() {
            assert_eq!(field.field_id, FieldId(index as u32 + 1));
            assert_eq!(field.name, FIELD_NAMES[index]);
            assert!(field.required);
            assert_eq!(field.scalar_kind,
                if index == SLOT_LAST_UPDATE_ID { Some(ScalarKind::Number) } else { None });
            assert!(field.format.is_none());
            assert!(field.min_len.is_none() && field.max_len.is_none());
            assert!(field.min_i64.is_none() && field.max_i64.is_none());
        }
    }

    #[test]
    fn standard_snapshot_retains_payload_and_exact_zero_quantity() {
        let profile = ExchangeProfile::default();
        for payload in [Payload::from_slice(FIXTURE), Payload::from_vec(FIXTURE.to_vec())] {
            let pointer = payload.as_slice().as_ptr();
            let message = decode_snapshot(payload, &profile, &mut TestValidator { calls: 0 }).unwrap();
            assert_eq!(message.raw_bytes(), FIXTURE);
            assert_eq!(message.raw_bytes().as_ptr(), pointer);
            assert_eq!(message.values().last_update_id, 1027024);
            assert_eq!(message.values().bids, [BookLevel {
                price: Decimal128::new(400000000, 8),
                quantity: Decimal128::new(43100000000, 8),
            }]);
            assert_eq!(message.values().asks, [BookLevel {
                price: Decimal128::new(400000200, 8), quantity: Decimal128::new(0, 8),
            }]);
            assert_eq!(message.proof().schema_fingerprint(),
                profiles::schema_fingerprint(MessageKind::Snapshot, &profile).unwrap());
        }
    }

    #[test]
    fn empty_books_source_order_duplicate_levels_and_negative_zero() {
        let profile = ExchangeProfile::default();
        let values = decode(EMPTY, &profile).unwrap().into_values();
        assert_eq!(values.last_update_id, 0);
        assert!(values.bids.is_empty() && values.asks.is_empty());
        let text = document("1", r#"[["2","0"],["1","-0.00000000"],["2","0"]]"#, "[]");
        let values = decode(text.as_bytes(), &profile).unwrap().into_values();
        assert_eq!(values.bids.len(), 3);
        assert_eq!(values.bids[0], values.bids[2]);
        assert_eq!(values.bids[1].price, Decimal128::new(100000000, 8));
        assert_eq!(values.bids[1].quantity, Decimal128::new(0, 8));
    }

    #[test]
    fn strict_u64_boundaries_without_signed_or_floating_coercions() {
        let profile = ExchangeProfile::default();
        let text = format!(r#"{{"asks":[],"bids":[],"lastUpdateId":{}}}"#, u64::MAX);
        assert_eq!(decode(text.as_bytes(), &profile).unwrap().values().last_update_id, u64::MAX);
        for input in ["", "00", "01", "-0", "-1", "+1", "1.0", "1e0", "18446744073709551616"] {
            assert!(read_u64(input.as_bytes(), Span32::new(0, input.len() as u32)).is_err(), "{input}");
        }
        for input in ["-0", "-1", "1.0", "1e0"] {
            let text = document(input, "[]", "[]");
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, ErrorCode::InvalidNumber);
        }
        let error = read_u64(b"18446744073709551616", Span32::new(0, 20)).unwrap_err();
        assert_eq!(error.code, ErrorCode::NumberOverflow);
        assert_eq!(error.byte_offset, 19);
        assert_eq!(error.field_id, 1);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(read_u64(b"x42", Span32::new(1, 2)).unwrap(), 42);
        assert_eq!(read_u64(b"0", Span32::new(u32::MAX, 1)).unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(read_u64(b"0", Span32::new(0, 2)).unwrap_err().code, ErrorCode::InternalError);
    }

    #[test]
    fn reordered_and_escaped_fields_and_decimal_strings() {
        let text = br#"{"asks":[],"\u0062ids":[["\u0031.25","\u0030"]],"last\u0055pdateId":7}"#;
        let values = decode(text, &ExchangeProfile::default()).unwrap().into_values();
        assert_eq!(values.last_update_id, 7);
        assert_eq!(values.bids[0], BookLevel {
            price: Decimal128::new(125000000, 8), quantity: Decimal128::new(0, 8),
        });
    }

    #[test]
    fn original_combined_coordinates_and_both_wrapper_orders() {
        let inner = core::str::from_utf8(FIXTURE).unwrap();
        for text in [format!(r#"{{"stream":"btc@depth","data":{inner}}}"#),
            format!(r#"{{"data":{inner},"str\u0065am":"btc\u0040depth"}}"#)] {
            for wrapper in [WrapperPolicy::CombinedStream, WrapperPolicy::Either] {
                let profile = ExchangeProfile { wrapper, ..ExchangeProfile::default() };
                let slots = TestValidator { calls: 0 }.validate(MessageKind::Snapshot,
                    Payload::from_slice(text.as_bytes()), &profile).unwrap();
                let SlotValue::RawSpan(bids) = slots.slots[SLOT_BIDS] else { panic!("bids span") };
                assert_eq!(bids.slice(text.as_bytes()), Some(&br#"[["4.00000000","431.00000000"]]"#[..]));
                assert!(bids.offset > 20);
                let pointer = slots.payload.as_slice().as_ptr();
                let message = decode_snapshot(Payload::from_slice(text.as_bytes()), &profile,
                    &mut ReturnedSlots(Some(slots))).unwrap();
                assert_eq!(message.raw_bytes(), text.as_bytes());
                assert_eq!(message.raw_bytes().as_ptr(), pointer);
                assert_eq!(message.values().last_update_id, 1027024);
            }
        }
        let either = ExchangeProfile { wrapper: WrapperPolicy::Either, ..ExchangeProfile::default() };
        assert!(decode(FIXTURE, &either).is_ok());
        let combined = ExchangeProfile { wrapper: WrapperPolicy::CombinedStream, ..either };
        assert_eq!(decode(FIXTURE, &combined).unwrap_err().code, ErrorCode::UnknownObjectKey);
        let wrapped = format!(r#"{{"stream":"x","data":{inner}}}"#);
        assert_eq!(decode(wrapped.as_bytes(), &ExchangeProfile::default()).unwrap_err().code,
            ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn every_book_level_is_checked_for_shape_type_and_decimal_semantics() {
        let profile = ExchangeProfile::default();
        for (book, expected) in [
            (r#"{}"#, ErrorCode::TypeMismatch),
            (r#"[0]"#, ErrorCode::TypeMismatch),
            (r#"[[]]"#, ErrorCode::TypeMismatch),
            (r#"[["1"]]"#, ErrorCode::TypeMismatch),
            (r#"[["1","0","2"]]"#, ErrorCode::ConstraintViolation),
            (r#"[[1,"0"]]"#, ErrorCode::TypeMismatch),
            (r#"[["1",null]]"#, ErrorCode::TypeMismatch),
            (r#"[["0","1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["-0","1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["-1","1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["1","-1"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["01","0"]]"#, ErrorCode::InvalidNumber),
            (r#"[["+1","0"]]"#, ErrorCode::InvalidNumber),
            (r#"[[".1","0"]]"#, ErrorCode::InvalidNumber),
            (r#"[["1.","0"]]"#, ErrorCode::InvalidNumber),
            (r#"[["1e0","0"]]"#, ErrorCode::InvalidNumber),
            (r#"[["0.000000001","0"]]"#, ErrorCode::DecimalScaleMismatch),
            (r#"[["1","0"],["2","-1"]]"#, ErrorCode::ConstraintViolation),
        ] {
            for asks in [false, true] {
                let text = if asks { document("0", "[]", book) } else { document("0", book, "[]") };
                let error = decode(text.as_bytes(), &profile).unwrap_err();
                assert_eq!(error.code, expected, "{text}: {error}");
                assert_eq!(error.schema_id, SNAPSHOT_SCHEMA_ID.0);
                assert_eq!(error.field_id, if asks { 3 } else { 2 });
            }
        }
    }

    #[test]
    fn decimal_precision_scale_ticks_exponents_and_truncation() {
        let mut profile = ExchangeProfile::default();
        profile.price.target_scale = 2;
        profile.price.max_precision = 3;
        profile.price.tick_step = 5;
        profile.quantity.target_scale = 0;
        let text = document("0", r#"[["1.25","0"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap().values().bids[0].price, Decimal128::new(125, 2));
        for (price, expected) in [("1.26", ErrorCode::TickMultipleViolation),
            ("10.00", ErrorCode::DecimalPrecisionOverflow),
            ("1.251", ErrorCode::DecimalScaleMismatch)] {
            let text = document("0", &format!(r#"[["{price}","0"]]"#), "[]");
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, expected);
        }
        profile.price.reject_inexact_rescale = false;
        let text = document("0", r#"[["1.259","0"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap().values().bids[0].price, Decimal128::new(125, 2));
        profile.price.allow_exponent = true;
        let text = document("0", r#"[["125e-2","0"]]"#, "[]");
        assert!(decode(text.as_bytes(), &profile).is_ok());
        profile.price.target_scale = 0;
        profile.price.max_precision = 38;
        profile.price.tick_step = 0;
        let text = document("0", r#"[["99999999999999999999999999999999999999","0"]]"#, "[]");
        assert!(decode(text.as_bytes(), &profile).is_ok());
        let text = document("0", r#"[["999999999999999999999999999999999999999","0"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, ErrorCode::NumberOverflow);
        profile.quantity.target_scale = 2;
        profile.quantity.tick_step = 10;
        let text = document("0", r#"[["1","0.01"]]"#, "[]");
        assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, ErrorCode::TickMultipleViolation);
    }

    #[test]
    fn profile_and_payload_fail_before_validator_invocation() {
        let mut validator = TestValidator { calls: 0 };
        let profile = ExchangeProfile::default();
        let invalid = ExchangeProfile { version: 2, ..profile };
        assert_eq!(decode_snapshot(Payload::from_slice(b"invalid"), &invalid,
            &mut validator).unwrap_err().code, ErrorCode::UnsupportedSchemaFeature);
        assert_eq!(validator.calls, 0);
        let invalid = ExchangeProfile { max_levels: 0, ..profile };
        assert!(decode_snapshot(Payload::from_slice(FIXTURE), &invalid, &mut validator).is_err());
        assert_eq!(validator.calls, 0);
        let limited = ExchangeProfile { limits: Limits {
            max_payload_bytes: FIXTURE.len() as u32 - 1, ..profile.limits
        }, ..profile };
        assert_eq!(decode_snapshot(Payload::from_slice(FIXTURE), &limited,
            &mut validator).unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert_eq!(validator.calls, 0);
        let exact = ExchangeProfile { limits: Limits {
            max_payload_bytes: FIXTURE.len() as u32, ..profile.limits
        }, ..profile };
        assert!(decode_snapshot(Payload::from_slice(FIXTURE), &exact, &mut validator).is_ok());
        assert_eq!(validator.calls, 1);
    }

    #[test]
    fn side_counts_and_nested_syntax_budgets_have_inclusive_boundaries() {
        let mut profile = ExchangeProfile::default();
        profile.max_levels = 1;
        assert!(decode(FIXTURE, &profile).is_ok());
        let two = document("0", r#"[["1","0"],["2","0"]]"#, "[]");
        assert_eq!(decode(two.as_bytes(), &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile.max_levels = 2;
        profile.limits.max_array_items = 2;
        assert!(decode(two.as_bytes(), &profile).is_ok());
        profile.limits.max_array_items = 1;
        assert_eq!(decode(FIXTURE, &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile = ExchangeProfile::default();
        profile.limits.max_depth = 3;
        assert!(decode(FIXTURE, &profile).is_ok());
        profile.limits.max_depth = 2;
        assert_eq!(decode(FIXTURE, &profile).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        profile = ExchangeProfile::default();
        profile.limits.max_fields_per_object = 3;
        assert!(decode(EMPTY, &profile).is_ok());
        profile.limits.max_fields_per_object = 2;
        assert_eq!(decode(EMPTY, &profile).unwrap_err().code, ErrorCode::ConstraintViolation);
        profile = ExchangeProfile::default();
        profile.limits.max_number_digits = 1;
        assert!(decode(EMPTY, &profile).is_ok());
        assert!(decode(FIXTURE, &profile).is_err());
    }

    #[test]
    fn corrupted_slots_lengths_owners_and_fingerprints_are_rejected() {
        assert_bad_slots(|s| { s.slots.pop(); }, ErrorCode::MissingRequiredField);
        assert_bad_slots(|s| s.slots.push(SlotValue::Empty), ErrorCode::TypeMismatch);
        for index in 0..SLOT_COUNT {
            assert_bad_slots(|s| s.slots[index] = SlotValue::Empty, ErrorCode::MissingRequiredField);
        }
        assert_bad_slots(|s| s.slots[0] = SlotValue::I64(1027024), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[0] = SlotValue::U64(1), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[1] = SlotValue::Bool(false), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[2] = SlotValue::Str(StrRef {
            span: Span32::new(0, 1), source: SpanSource::RawPayload, scalar_len: 1,
        }), ErrorCode::TypeMismatch);
        assert_bad_slots(|s| s.slots[1] = SlotValue::RawSpan(Span32::new(u32::MAX, 2)), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[1] = SlotValue::RawSpan(Span32::new(0, 1)), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots[1] = SlotValue::RawSpan(Span32::new(0, 0)), ErrorCode::InternalError);
        assert_bad_slots(|s| s.slots.swap(1, 2), ErrorCode::InternalError);
        assert_bad_slots(|s| { s.payload = Payload::from_slice(EMPTY); }, ErrorCode::InternalError);
        assert_bad_slots(|s| {
            let mut bytes = FIXTURE.to_vec();
            bytes[0] = b'[';
            s.payload = Payload::from_vec(bytes);
        }, ErrorCode::InternalError);
        assert_bad_slots(|s| { s.decoded = vec![b'x'].into_boxed_slice(); }, ErrorCode::InternalError);
        let profile = ExchangeProfile::default();
        let other = ExchangeProfile { max_levels: 1, ..profile };
        let error = decode_snapshot(Payload::from_slice(FIXTURE), &profile,
            &mut ReturnedSlots(Some(fixture_slots(&other)))).unwrap_err();
        assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
    }

    #[test]
    fn decoded_arena_is_preserved_and_budget_checked() {
        let profile = ExchangeProfile::default();
        let text = br#"{"last\u0055pdateId":0,"bids":[["\u0031","0"]],"asks":[]}"#;
        let slots = TestValidator { calls: 0 }.validate(MessageKind::Snapshot,
            Payload::from_slice(text), &profile).unwrap();
        assert!(!slots.decoded.is_empty());
        let pointer = slots.decoded.as_ptr();
        let decoded = slots.decoded.to_vec();
        let message = decode_snapshot(Payload::from_slice(text), &profile,
            &mut ReturnedSlots(Some(slots))).unwrap();
        assert_eq!(message.decoded_bytes().as_ptr(), pointer);
        assert_eq!(message.decoded_bytes(), decoded.as_slice());
        let limited = ExchangeProfile { limits: Limits {
            max_decoded_bytes: 0, ..profile.limits
        }, ..profile };
        assert_eq!(decode(text, &limited).unwrap_err().code, ErrorCode::DecodedArenaOverflow);
        assert!(decode(EMPTY, &limited).is_ok());
    }

    #[test]
    fn validator_errors_keep_canonical_source_offset_and_context() {
        struct Failing(Error);
        impl ExchangeValidator for Failing {
            fn validate(&mut self, _: MessageKind, _: Payload, _: &ExchangeProfile) -> Result<ValidatedSlots> {
                Err(self.0.clone())
            }
        }
        let expected = Error::new(ErrorCode::Cancelled, ErrorSource::SchemaVm, 17,
            "test validator failure").with_schema(91, 8);
        let actual = decode_snapshot(Payload::from_slice(FIXTURE), &ExchangeProfile::default(),
            &mut Failing(expected.clone())).unwrap_err();
        assert_eq!(actual, expected);
    }

    #[test]
    fn missing_wrong_unknown_and_duplicate_fields_fail_independently_of_limits_flags() {
        let mut profile = ExchangeProfile::default();
        profile.limits.reject_duplicate_keys = false;
        profile.limits.reject_unknown_keys = false;
        for (text, expected) in [
            (r#"{"bids":[],"asks":[]}"#, ErrorCode::MissingRequiredField),
            (r#"{"lastUpdateId":0,"asks":[]}"#, ErrorCode::MissingRequiredField),
            (r#"{"lastUpdateId":0,"bids":[]}"#, ErrorCode::MissingRequiredField),
            (r#"{"lastUpdateId":"0","bids":[],"asks":[]}"#, ErrorCode::TypeMismatch),
            (r#"{"lastUpdateId":true,"bids":[],"asks":[]}"#, ErrorCode::TypeMismatch),
            (r#"{"lastUpdateId":0,"bids":{},"asks":[]}"#, ErrorCode::TypeMismatch),
            (r#"{"lastUpdateId":0,"bids":[],"asks":[],"extra":0}"#, ErrorCode::UnknownObjectKey),
            (r#"{"lastUpdateId":0,"bids":[],"asks":[],"bids":[]}"#, ErrorCode::DuplicateObjectKey),
            (r#"{"lastUpdateId":0,"bids":[],"asks":[],"\u0062ids":[]}"#, ErrorCode::DuplicateObjectKey),
        ] {
            assert_eq!(decode(text.as_bytes(), &profile).unwrap_err().code, expected, "{text}");
        }
    }

    #[test]
    fn independent_wire_traversal_rejects_metadata_and_tail_with_no_new_proof() {
        let profile = ExchangeProfile::default();
        let captured = capture(Payload::from_slice(FIXTURE), &profile).unwrap();
        let owner = SlotOwner { raw: FIXTURE, slots: &captured.slots };
        let original = core::str::from_utf8(FIXTURE).unwrap();
        for suffix in ["x", "{}", ",", "\0", "\u{a0}"] {
            let text = format!("{original}{suffix}");
            assert!(read_document(Payload::from_slice(text.as_bytes()), profile.wrapper,
                profile.limits, false, |cursor, first| read_members(cursor, first, &owner, &profile)).is_err());
        }
        let text = original.replace("1027024", "1027025");
        let error = read_document(Payload::from_slice(text.as_bytes()), profile.wrapper,
            profile.limits, false, |cursor, first| read_members(cursor, first, &owner, &profile))
            .err().expect("wire and slot differ");
        assert_eq!(error.code, ErrorCode::InternalError);
        assert_eq!(error.field_id, 1);
    }

    #[test]
    fn utility_bare_preserves_storage_whitespace_and_has_no_stream() {
        let text = b" \n {\"lastUpdateId\":0,\"bids\":[],\"asks\":[]}\t\r ";
        for policy in [WrapperPolicy::Bare, WrapperPolicy::Either] {
            for payload in [Payload::from_slice(text), Payload::from_vec(text.to_vec())] {
                let pointer = payload.as_slice().as_ptr();
                let value = unwrap_payload(payload, policy, Limits::strict_default()).unwrap();
                assert_eq!(value.payload.as_slice().as_ptr(), pointer);
                assert_eq!(value.payload.as_slice(), text);
                assert!(value.stream.is_none());
            }
        }
        assert!(unwrap(b"{}", WrapperPolicy::Bare).is_ok());
        assert!(unwrap(b"{}", WrapperPolicy::Either).is_ok());
    }

    #[test]
    fn utility_wrapped_copies_exact_data_bytes_and_owns_decoded_stream() {
        let inner = r#"{ "lastUpdateId" : 0, "bids":[], "asks":[] }"#;
        for text in [format!(" \n{{\"stream\":\"bt\\u0063@depth\",\"data\": \n{inner} \t}}\r"),
            format!(r#"{{"data":{inner},"str\u0065am":"btc@depth"}}"#)] {
            for policy in [WrapperPolicy::CombinedStream, WrapperPolicy::Either] {
                let payload = Payload::from_slice(text.as_bytes());
                let observer = payload.clone();
                let value = unwrap_payload(payload, policy, Limits::strict_default()).unwrap();
                assert_eq!(value.stream.as_deref(), Some("btc@depth"));
                assert_eq!(value.payload.as_slice(), inner.as_bytes());
                assert_ne!(value.payload.as_slice().as_ptr(), observer.as_slice().as_ptr());
                drop(observer);
                let message = decode_snapshot(value.payload, &ExchangeProfile::default(),
                    &mut TestValidator { calls: 0 }).unwrap();
                assert_eq!(message.values().last_update_id, 0);
                assert_eq!(value.stream.as_deref(), Some("btc@depth"));
            }
        }
        let value = unwrap(br#"{"stream":"x","data":{}}"#, WrapperPolicy::CombinedStream).unwrap();
        assert_eq!(value.payload.as_slice(), b"{}");
    }

    #[test]
    fn utility_validates_structure_without_issuing_schema_or_decimal_validation() {
        let bytes = br#"{"stream":"x","data":{"lastUpdateId":-1,"bids":[["0","-1"]],"asks":[],"extra":{"nested":[true,false,null]}}}"#;
        let value = unwrap(bytes, WrapperPolicy::CombinedStream).unwrap();
        assert_eq!(value.stream.as_deref(), Some("x"));
        assert!(decode(value.payload.as_slice(), &ExchangeProfile::default()).is_err());
        // Wrapper names inside the data object are ordinary unvalidated keys.
        let value = unwrap(br#"{"stream":"x","data":{"data":{},"stream":"inner"}}"#,
            WrapperPolicy::CombinedStream).unwrap();
        assert_eq!(value.payload.as_slice(), br#"{"data":{},"stream":"inner"}"#);
    }

    #[test]
    fn malformed_wrappers_never_fall_back_to_bare() {
        let mut limits = Limits::strict_default();
        limits.reject_duplicate_keys = false;
        limits.reject_unknown_keys = false;
        for (text, expected) in [
            (r#"{}"#, ErrorCode::MissingRequiredField),
            (r#"{"stream":"x"}"#, ErrorCode::MissingRequiredField),
            (r#"{"data":{}}"#, ErrorCode::MissingRequiredField),
            (r#"{"stream":"","data":{}}"#, ErrorCode::ConstraintViolation),
            (r#"{"stream":null,"data":{}}"#, ErrorCode::TypeMismatch),
            (r#"{"stream":1,"data":{}}"#, ErrorCode::TypeMismatch),
            (r#"{"stream":{},"data":{}}"#, ErrorCode::TypeMismatch),
            (r#"{"stream":"x","data":[]}"#, ErrorCode::TypeMismatch),
            (r#"{"stream":"x","data":null}"#, ErrorCode::TypeMismatch),
            (r#"{"stream":"x","data":"{}"}"#, ErrorCode::TypeMismatch),
            (r#"{"stream":"x","data":{},"extra":0}"#, ErrorCode::UnknownObjectKey),
            (r#"{"extra":0,"stream":"x","data":{}}"#, ErrorCode::UnknownObjectKey),
            (r#"{"stream":"x","data":{},"stream":"y"}"#, ErrorCode::DuplicateObjectKey),
            (r#"{"stream":"x","data":{},"str\u0065am":"y"}"#, ErrorCode::DuplicateObjectKey),
            (r#"{"data":{},"data":{},"stream":"x"}"#, ErrorCode::DuplicateObjectKey),
        ] {
            for policy in [WrapperPolicy::CombinedStream, WrapperPolicy::Either] {
                if text == "{}" && policy == WrapperPolicy::Either { continue; }
                let error = unwrap_payload(Payload::from_slice(text.as_bytes()), policy, limits).unwrap_err();
                assert_eq!(error.code, expected, "{text}: {error}");
                assert_eq!(error.source, ErrorSource::Decode);
                assert_eq!(error.schema_id, 0);
            }
        }
        assert_eq!(unwrap(br#"{"stream":"x","data":{}}"#, WrapperPolicy::Bare)
            .unwrap_err().code, ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn typed_wrappers_require_exact_members_even_with_permissive_key_flags() {
        let profile = ExchangeProfile {
            wrapper: WrapperPolicy::Either,
            limits: Limits { reject_duplicate_keys: false, reject_unknown_keys: false,
                ..Limits::strict_default() },
            ..ExchangeProfile::default()
        };
        let inner = core::str::from_utf8(FIXTURE).unwrap();
        for (text, expected) in [
            (format!(r#"{{"data":{inner}}}"#), ErrorCode::MissingRequiredField),
            (format!(r#"{{"stream":"","data":{inner}}}"#), ErrorCode::ConstraintViolation),
            (format!(r#"{{"stream":false,"data":{inner}}}"#), ErrorCode::TypeMismatch),
            (format!(r#"{{"stream":"x","data":{inner},"z":0}}"#), ErrorCode::UnknownObjectKey),
            (format!(r#"{{"z":0,"stream":"x","data":{inner}}}"#), ErrorCode::UnknownObjectKey),
            (format!(r#"{{"stream":"x","data":{inner},"stream":"y"}}"#), ErrorCode::DuplicateObjectKey),
        ] {
            let captured = capture(Payload::from_slice(text.as_bytes()), &profile).unwrap();
            let error = inspect(&captured, &profile).unwrap_err();
            assert_eq!(error.code, expected, "{text}: {error}");
            assert_eq!(error.schema_id, SNAPSHOT_SCHEMA_ID.0);
        }
    }

    #[test]
    fn utility_limits_count_wrapper_depth_fields_strings_and_all_array_items() {
        let text = br#"{"stream":"abcd","data":{"x":[1,2]}}"#;
        let base = Limits::strict_default();
        let exact = Limits { max_payload_bytes: text.len() as u32,
            max_depth: 3, max_fields_per_object: 2, max_string_bytes: 6,
            max_number_digits: 1, max_array_items: 2, ..base };
        assert!(unwrap_payload(Payload::from_slice(text), WrapperPolicy::CombinedStream, exact).is_ok());
        for (limits, expected) in [
            (Limits { max_payload_bytes: text.len() as u32 - 1, ..exact }, ErrorCode::PayloadTooLarge),
            (Limits { max_depth: 2, ..exact }, ErrorCode::DepthLimitExceeded),
            (Limits { max_fields_per_object: 1, ..exact }, ErrorCode::ConstraintViolation),
            (Limits { max_array_items: 1, ..exact }, ErrorCode::ConstraintViolation),
        ] {
            assert_eq!(unwrap_payload(Payload::from_slice(text), WrapperPolicy::CombinedStream,
                limits).unwrap_err().code, expected);
        }
        let limits = Limits { max_string_bytes: 3, ..base };
        assert!(unwrap_payload(Payload::from_slice(text), WrapperPolicy::CombinedStream, limits).is_err());
        let limits = Limits { max_number_digits: 0, ..base };
        assert!(unwrap_payload(Payload::from_slice(text), WrapperPolicy::CombinedStream, limits).is_err());
        let limits = Limits { max_depth: 0, ..base };
        assert_eq!(unwrap_payload(Payload::from_slice(b"{}"), WrapperPolicy::Bare, limits)
            .unwrap_err().code, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn unicode_stream_scalar_boundaries_and_decoded_arena_limits() {
        let bytes = br#"{"stream":"\u00e9\uD83D\uDE00","data":{}}"#;
        let value = unwrap(bytes, WrapperPolicy::CombinedStream).unwrap();
        assert_eq!(value.stream.as_deref(), Some("é😀"));
        assert_eq!(value.stream.as_ref().unwrap().len(), 6);
        assert_eq!(value.stream.as_ref().unwrap().chars().count(), 2);
        let limits = Limits { max_decoded_bytes: 0, ..Limits::strict_default() };
        assert_eq!(unwrap_payload(Payload::from_slice(bytes), WrapperPolicy::CombinedStream, limits)
            .unwrap_err().code, ErrorCode::DecodedArenaOverflow);
        assert!(unwrap_payload(Payload::from_slice(br#"{"stream":"plain","data":{}}"#),
            WrapperPolicy::CombinedStream, limits).is_ok());
        let limits = Limits { max_decoded_bytes: 6, ..Limits::strict_default() };
        assert!(unwrap_payload(Payload::from_slice(bytes), WrapperPolicy::CombinedStream, limits).is_ok());
        let limits = Limits { max_decoded_bytes: 5, ..Limits::strict_default() };
        assert_eq!(unwrap_payload(Payload::from_slice(bytes), WrapperPolicy::CombinedStream, limits)
            .unwrap_err().code, ErrorCode::DecodedArenaOverflow);
    }

    #[test]
    fn utility_checks_unselected_nested_values_duplicate_policy_and_suffixes() {
        for bytes in [
            &br#"{"stream":"x","data":{"hidden":[1,]}}"#[..],
            &br#"{"stream":"x","data":{"hidden":"\uD800"}}"#[..],
            &b"{\"stream\":\"x\",\"data\":{\"hidden\":\"\xff\"}}"[..],
            &br#"{"stream":"x","data":{},}"#[..],
        ] {
            assert!(unwrap(bytes, WrapperPolicy::CombinedStream).is_err(), "{bytes:?}");
        }
        let duplicate = br#"{"stream":"x","data":{"x":0,"\u0078":1}}"#;
        assert_eq!(unwrap(duplicate, WrapperPolicy::CombinedStream).unwrap_err().code,
            ErrorCode::DuplicateObjectKey);
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        assert!(unwrap_payload(Payload::from_slice(duplicate), WrapperPolicy::CombinedStream, limits).is_ok());
        for suffix in ["x", "{}", "[]", ",", "\0", "\u{a0}"] {
            let text = format!(r#"{{"stream":"x","data":{{}}}}{suffix}"#);
            assert!(unwrap(text.as_bytes(), WrapperPolicy::CombinedStream).is_err());
        }
        assert!(unwrap(b"{\"stream\":\"x\",\"data\":{}} \t\r\n", WrapperPolicy::CombinedStream).is_ok());
    }

    #[test]
    fn malformed_json_wrong_roots_and_invalid_utf8_are_never_accepted() {
        let profile = ExchangeProfile::default();
        for bytes in [
            &b""[..], &b"{"[..], &b"[]"[..], &b"null"[..], &b"0"[..],
            &b"true"[..], &br#""object""#[..], &b"{\"lastUpdateId\":0,}"[..],
            &b"{\"lastUpdateId\":00,\"bids\":[],\"asks\":[]}"[..],
            &b"{\"lastUpdateId\":0,\"bids\":[[\"\xff\",\"0\"]],\"asks\":[]}"[..],
        ] {
            assert!(decode(bytes, &profile).is_err(), "{bytes:?}");
            for policy in [WrapperPolicy::Bare, WrapperPolicy::CombinedStream, WrapperPolicy::Either] {
                assert!(unwrap(bytes, policy).is_err(), "{bytes:?}");
            }
        }
    }
}
