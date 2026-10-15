//! Buffered typed encoding and rechecked, schema-ordered slot output.
//!
//! The caller's destination is touched only after the complete message has
//! passed validation. An error from that destination can still leave a prefix.

use super::*;
use core::cmp::Ordering;
use jmax_abi::span::{Span32, SpanSource, StrRef};
use jmax_abi::syntax::ScalarKind;

/// Runs the ABI encoder, validates its entire output, then publishes it.
///
/// A custom encoder is responsible for its own computation and termination.
/// Its output storage and the subsequently accepted JSON are bounded here.
pub fn encode<T: JsonEncode + ?Sized>(
    value: &T,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    let bytes = encode_to_vec(value, limits)?;
    publish(&bytes, sink, limits)
}

/// Returns the encoder's validated owned buffer without copying its contents.
pub fn encode_to_vec<T: JsonEncode + ?Sized>(
    value: &T,
    limits: EncodeLimits,
) -> Result<Vec<u8>> {
    let mut temporary = VecSink::new(limits.max_output_bytes);
    value.encode_json(&mut temporary)?;
    raw::validate(temporary.as_slice(), limits)?;
    Ok(temporary.into_vec())
}

/// Emits present fields in schema order, rechecking current values and owners.
///
/// Field position, rather than FieldId, indexes the slots. The proof is neither
/// trusted as a schema match nor replaced with newly manufactured evidence.
/// Named formats are unsupported by this version of the ABI adapter.
pub fn encode_slots(
    slots: &ValidatedSlots,
    schema: &SchemaIr,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    encode_values(slots.payload.as_slice(), &slots.decoded, &slots.slots, schema, sink, limits)
}

fn publish(bytes: &[u8], sink: &mut dyn ByteSink, limits: EncodeLimits) -> Result<()> {
    let mut bounded = LimitedSink::new(sink, limits.max_output_bytes);
    sink::append(&mut bounded, bytes, limits.max_output_bytes)
}

fn field_error(field: &SchemaFieldIr, schema: &SchemaIr, code: ErrorCode, detail: &'static str) -> Error {
    encode_error(code, detail).with_schema(schema.schema_id.0, field.field_id.0)
}

/// Kept independent of proof construction so every owner/value path can also
/// be exercised by safe tests. This is the complete implementation used above.
fn encode_values(
    payload: &[u8],
    decoded: &[u8],
    values: &[SlotValue],
    schema: &SchemaIr,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    preflight(schema)?;
    if values.len() != schema.fields.len() {
        return Err(encode_error(ErrorCode::ConstraintViolation, "slot count does not match schema")
            .with_schema(schema.schema_id.0, 0));
    }
    let mut temporary = VecSink::new(limits.max_output_bytes);
    let mut scratch = VecSink::new(limits.max_output_bytes);
    let mut writer = JsonWriter::new(&mut temporary, limits);
    writer.begin_object().map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    // begin_object established that a depth allowance of one exists.
    let value_limits = EncodeLimits { max_depth: limits.max_depth - 1, ..limits };
    for (field, &value) in schema.fields.iter().zip(values) {
        let context = |error: Error| error.with_schema(schema.schema_id.0, field.field_id.0);
        if value == SlotValue::Empty {
            if field.required {
                return Err(field_error(field, schema, ErrorCode::MissingRequiredField,
                    "required output field is absent"));
            }
            continue;
        }
        let fragment = render_slot(value, payload, decoded, &mut scratch, value_limits)
            .map_err(context)?;
        check_field(fragment, field).map_err(context)?;
        writer.key(&field.name).map_err(context)?;
        writer.raw(fragment).map_err(context)?;
    }
    writer.end_object().map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    writer.finish().map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    raw::validate(temporary.as_slice(), limits)
        .map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    // Preserve all external error metadata, including its original source.
    publish(temporary.as_slice(), sink, limits)
}

fn preflight(schema: &SchemaIr) -> Result<()> {
    // Formats are unsupported regardless of requiredness or other metadata.
    for field in &schema.fields {
        if field.format.is_some() {
            return Err(field_error(field, schema, ErrorCode::UnsupportedSchemaFeature,
                "named formats are not supported by typed encoding"));
        }
    }
    let mut required = 0u16;
    for field in &schema.fields {
        let invalid = |detail| field_error(field, schema, ErrorCode::ConstraintViolation, detail);
        if field.required {
            required = required.checked_add(1)
                .ok_or_else(|| invalid("required field count exceeds the ABI range"))?;
        }
        let length = field.min_len.is_some() || field.max_len.is_some();
        let integer = field.min_i64.is_some() || field.max_i64.is_some();
        if (length && field.scalar_kind != Some(ScalarKind::String))
            || (integer && field.scalar_kind != Some(ScalarKind::Number))
        {
            return Err(invalid("constraint family does not match the scalar kind"));
        }
        if matches!((field.min_len, field.max_len), (Some(min), Some(max)) if min > max)
            || matches!((field.min_i64, field.max_i64), (Some(min), Some(max)) if min > max)
        {
            return Err(invalid("schema bounds are reversed"));
        }
    }
    if required != schema.required_count {
        return Err(encode_error(ErrorCode::ConstraintViolation, "schema required count is inaccurate")
            .with_schema(schema.schema_id.0, 0));
    }
    // Sort references, leaving field order intact. Unstable sorting needs no
    // additional allocation and avoids quadratic duplicate scans.
    let mut fields: Vec<&SchemaFieldIr> = Vec::new();
    fields.try_reserve(schema.fields.len()).map_err(|_| {
        encode_error(ErrorCode::BudgetExhausted, "schema preflight allocation failed")
            .with_schema(schema.schema_id.0, 0)
    })?;
    fields.extend(schema.fields.iter());
    fields.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    for pair in fields.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(field_error(pair[1], schema, ErrorCode::ConstraintViolation,
                "schema field names are not unique"));
        }
    }
    fields.sort_unstable_by_key(|field| field.field_id.0);
    for pair in fields.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(field_error(pair[1], schema, ErrorCode::ConstraintViolation,
                "schema field identifiers are not unique"));
        }
    }
    Ok(())
}

fn resolve_string<'a>(reference: StrRef, payload: &'a [u8], decoded: &'a [u8]) -> Result<&'a str> {
    let owner = match reference.source {
        SpanSource::RawPayload => payload,
        SpanSource::DecodedArena => decoded,
    };
    let bytes = reference.span.slice(owner).ok_or_else(|| {
        encode_error(ErrorCode::ConstraintViolation, "string span is outside its owner")
    })?;
    let text = core::str::from_utf8(bytes).map_err(|error| {
        Error::new(ErrorCode::InvalidUtf8, ErrorSource::Encode,
            u32::try_from(error.valid_up_to()).unwrap_or(u32::MAX), "invalid slot string UTF-8")
    })?;
    if text.chars().count() != reference.scalar_len as usize {
        return Err(encode_error(ErrorCode::ConstraintViolation, "slot string scalar count is inaccurate"));
    }
    Ok(text)
}

/// Render through the canonical writer rather than independently formatting
/// numbers or decimals. Raw spans retain their exact spelling and whitespace.
fn render_slot<'a>(
    value: SlotValue,
    payload: &'a [u8],
    decoded: &[u8],
    scratch: &'a mut VecSink,
    limits: EncodeLimits,
) -> Result<RawFragment<'a>> {
    if let SlotValue::RawSpan(span) = value {
        let bytes = span.slice(payload).ok_or_else(|| {
            encode_error(ErrorCode::ConstraintViolation, "raw span is outside the payload")
        })?;
        return raw::validate(bytes, limits);
    }
    scratch.bytes.clear();
    let mut writer = JsonWriter::new(scratch, limits);
    match value {
        SlotValue::Bool(value) => writer.boolean(value)?,
        SlotValue::U64(value) => writer.u64(value)?,
        SlotValue::I64(value) => writer.i64(value)?,
        SlotValue::F64Bits(bits) => writer.f64(f64::from_bits(bits))?,
        SlotValue::Decimal(value) => writer.decimal(value)?,
        SlotValue::Str(reference) => writer.string(resolve_string(reference, payload, decoded)?)?,
        SlotValue::Empty | SlotValue::RawSpan(_) => {
            return Err(encode_error(ErrorCode::InternalError, "slot has no standalone writer representation"));
        }
    }
    writer.finish()?;
    raw::validate(scratch.as_slice(), limits)
}

fn check_field(fragment: RawFragment<'_>, field: &SchemaFieldIr) -> Result<()> {
    let expected = match field.scalar_kind {
        None => return Ok(()),
        Some(ScalarKind::String) => FragmentKind::String,
        Some(ScalarKind::Number) => FragmentKind::Number,
        Some(ScalarKind::True | ScalarKind::False) => FragmentKind::Boolean,
        Some(ScalarKind::Null) => FragmentKind::Null,
    };
    if fragment.kind() != expected {
        return Err(encode_error(ErrorCode::TypeMismatch, "output scalar category does not match schema"));
    }
    let token = trim_json_whitespace(fragment.as_bytes());
    if field.min_len.is_some() || field.max_len.is_some() {
        // String fragments have already been strictly validated, including
        // quotes and surrogate pairing. Measurement counts decoded scalars.
        let content_len = u32::try_from(token.len() - 2).map_err(|_| {
            encode_error(ErrorCode::ConstraintViolation, "string length exceeds the schema counter range")
        })?;
        let measured = jmax_string::length::decoded_length(token, Span32::new(1, content_len))
            .map_err(|mut error| { error.source = ErrorSource::Encode; error })?;
        if field.min_len.is_some_and(|min| measured.scalar_len < min)
            || field.max_len.is_some_and(|max| measured.scalar_len > max)
        {
            return Err(encode_error(ErrorCode::ConstraintViolation, "string length is outside schema bounds"));
        }
    }
    if field.min_i64.is_some() || field.max_i64.is_some() {
        let digits = token.strip_prefix(b"-").unwrap_or(token);
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(encode_error(ErrorCode::TypeMismatch, "integer schema requires integer JSON grammar"));
        }
        if field.min_i64.is_some_and(|min| compare_integer(token, min) == Ordering::Less)
            || field.max_i64.is_some_and(|max| compare_integer(token, max) == Ordering::Greater)
        {
            return Err(encode_error(ErrorCode::ConstraintViolation, "integer is outside schema bounds"));
        }
    }
    Ok(())
}

fn trim_json_whitespace(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Compare a validated integer token to i64 without narrowing or rounding.
/// Arbitrarily long raw integers can satisfy a one-sided bound. Negative zero
/// compares equal to zero, while its original JSON spelling remains intact.
fn compare_integer(token: &[u8], bound: i64) -> Ordering {
    let digits = token.strip_prefix(b"-").unwrap_or(token);
    let negative = token.first() == Some(&b'-') && digits != b"0";
    let bound_negative = bound < 0;
    if negative != bound_negative {
        return if negative { Ordering::Less } else { Ordering::Greater };
    }
    let mut magnitude = bound.unsigned_abs();
    let mut buffer = [0u8; 20];
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 { break; }
    }
    let other = &buffer[start..];
    let order = digits.len().cmp(&other.len()).then_with(|| digits.cmp(other));
    if negative { order.reverse() } else { order }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec};
    use jmax_abi::schema::{FieldId, SchemaId};

    struct Literal<'a>(&'a [u8]);

    impl JsonEncode for Literal<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(self.0)
        }
    }

    fn external_error() -> Error {
        Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 17, "injected failure")
            .with_schema(23, 29)
    }

    #[derive(Default)]
    struct Destination {
        bytes: Vec<u8>,
        reservations: usize,
        writes: usize,
        fail_reserve: bool,
        fail_write: bool,
        short_write: bool,
    }

    impl ByteSink for Destination {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reservations += 1;
            if self.fail_reserve { return Err(external_error()); }
            ByteSink::reserve(&mut self.bytes, additional)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.writes += 1;
            if self.fail_write {
                ByteSink::write_bytes(&mut self.bytes, &bytes[..bytes.len().min(2)])?;
                return Err(external_error());
            }
            if self.short_write { return Ok(()); }
            ByteSink::write_bytes(&mut self.bytes, bytes)
        }

        fn len(&self) -> usize { self.bytes.len() }
    }

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Encode);
        assert!(!error.detail.is_empty());
    }

    fn field(id: u32, name: &str, kind: Option<ScalarKind>) -> SchemaFieldIr {
        SchemaFieldIr {
            field_id: FieldId(id), name: String::from(name), required: false,
            scalar_kind: kind, format: None, min_len: None, max_len: None,
            min_i64: None, max_i64: None,
        }
    }

    fn schema(fields: Vec<SchemaFieldIr>) -> SchemaIr {
        SchemaIr {
            name: String::from("typed vectors"), schema_id: SchemaId(91),
            required_count: fields.iter().filter(|field| field.required).count() as u16,
            reject_unknown: true, fields,
        }
    }

    fn render(payload: &[u8], decoded: &[u8], values: &[SlotValue], schema: &SchemaIr) -> Vec<u8> {
        let mut sink = Vec::new();
        encode_values(payload, decoded, values, schema, &mut sink, EncodeLimits::default()).unwrap();
        raw::validate(&sink, EncodeLimits::default()).unwrap();
        sink
    }

    fn reject(
        payload: &[u8], decoded: &[u8], values: &[SlotValue], schema: &SchemaIr,
        limits: EncodeLimits, code: ErrorCode, field_id: u32,
    ) -> Error {
        let mut destination = Destination { bytes: b"prefix".to_vec(), ..Destination::default() };
        let error = encode_values(payload, decoded, values, schema, &mut destination, limits).unwrap_err();
        assert_error(error.clone(), code);
        assert_eq!(error.schema_id, schema.schema_id.0);
        assert_eq!(error.field_id, field_id);
        assert_eq!(destination.bytes, b"prefix");
        assert_eq!((destination.reservations, destination.writes), (0, 0));
        error
    }

    fn raw_value(bytes: &[u8]) -> SlotValue {
        SlotValue::RawSpan(Span32::new(0, bytes.len() as u32))
    }

    #[test]
    fn generic_encoding_accepts_every_root_and_preserves_exact_bytes() {
        let vectors: &[&[u8]] = &[
            b"null", b"true", b"false", b"-0", b"18446744073709551616",
            b"1.25e+009", br#""\u0000\uD83D\uDE00""#,
            b"[]", b"{}", b" \t{\"x\":[1,true,null],\"x\":false}\r\n",
        ];
        for &bytes in vectors {
            let value = Literal(bytes);
            let erased: &dyn JsonEncode = &value;
            let limits = EncodeLimits { max_output_bytes: bytes.len(), ..EncodeLimits::default() };
            assert_eq!(encode_to_vec(erased, limits).unwrap(), bytes);
            let mut destination = Destination { bytes: b"prefix:".to_vec(), ..Destination::default() };
            encode(erased, &mut destination, limits).unwrap();
            assert_eq!(&destination.bytes[..7], b"prefix:");
            assert_eq!(&destination.bytes[7..], bytes);
            assert_eq!((destination.reservations, destination.writes), (1, 1));
        }
    }

    #[test]
    fn generic_encoding_rejects_malformed_and_multiple_root_output_before_publication() {
        let vectors: &[(&[u8], ErrorCode)] = &[
            (b"", ErrorCode::UnexpectedEof), (b" \t\r\n", ErrorCode::UnexpectedEof),
            (b"null true", ErrorCode::TrailingContent), (b"{}[]", ErrorCode::TrailingContent),
            (b"[0,]", ErrorCode::InvalidSyntax), (b"{\"a\":}", ErrorCode::InvalidSyntax),
            (b"[0", ErrorCode::UnexpectedEof), (b"01", ErrorCode::InvalidNumber),
            (b"NaN", ErrorCode::InvalidNumber), (b"+1", ErrorCode::InvalidNumber),
            (b"\xef\xbb\xbfnull", ErrorCode::InvalidSyntax), (b"\"\xff\"", ErrorCode::InvalidUtf8),
            (br#""\q""#, ErrorCode::InvalidEscape),
            (br#""\uDC00""#, ErrorCode::LoneSurrogate),
            (b"\"\n\"", ErrorCode::ControlCharInString),
        ];
        for &(bytes, code) in vectors {
            let mut destination = Destination::default();
            let error = encode(&Literal(bytes), &mut destination, EncodeLimits::default()).unwrap_err();
            assert_error(error, code);
            assert_eq!((destination.reservations, destination.writes), (0, 0));
            assert!(destination.bytes.is_empty());
        }
    }

    #[test]
    fn generic_limits_are_inclusive_and_enforced_before_destination_calls() {
        let default = EncodeLimits::default();
        let vectors: &[(&[u8], EncodeLimits, ErrorCode)] = &[
            (b"null", EncodeLimits { max_output_bytes: 3, ..default }, ErrorCode::PayloadTooLarge),
            (b"[]", EncodeLimits { max_depth: 0, ..default }, ErrorCode::DepthLimitExceeded),
            (b"[0,1]", EncodeLimits { max_container_items: 1, ..default }, ErrorCode::BudgetExhausted),
            (br#""\uD83D\uDE00""#, EncodeLimits { max_string_bytes: 3, ..default }, ErrorCode::BudgetExhausted),
            (b"100", EncodeLimits { max_number_bytes: 2, ..default }, ErrorCode::BudgetExhausted),
        ];
        for &(bytes, limits, code) in vectors {
            let mut destination = Destination::default();
            assert_error(encode(&Literal(bytes), &mut destination, limits).unwrap_err(), code);
            assert_eq!((destination.reservations, destination.writes), (0, 0));
        }
        assert_eq!(encode_to_vec(&Literal(b"null"), EncodeLimits {
            max_output_bytes: 4, max_depth: 0, max_container_items: 0,
            max_string_bytes: 0, max_number_bytes: 0,
        }).unwrap(), b"null");
        assert_error(encode_to_vec(&Literal(b""), EncodeLimits {
            max_output_bytes: 0, ..default
        }).unwrap_err(), ErrorCode::UnexpectedEof);
    }

    struct FailedEncoder;

    impl JsonEncode for FailedEncoder {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(b"null")?;
            Err(external_error())
        }
    }

    #[test]
    fn trait_failure_after_a_valid_value_is_preserved_without_publication() {
        let mut destination = Destination::default();
        assert_eq!(encode(&FailedEncoder, &mut destination, EncodeLimits::default()).unwrap_err(), external_error());
        assert_eq!(encode_to_vec(&FailedEncoder, EncodeLimits::default()).unwrap_err(), external_error());
        assert_eq!((destination.reservations, destination.writes), (0, 0));
    }

    #[test]
    fn external_sink_errors_and_partial_writes_are_preserved_by_both_paths() {
        let schema = schema(vec![field(7, "v", None)]);
        for slots in [false, true] {
            for fail_reserve in [false, true] {
                let mut destination = Destination {
                    bytes: b"prefix:".to_vec(), fail_reserve, fail_write: !fail_reserve,
                    ..Destination::default()
                };
                let result = if slots {
                    encode_values(b"", b"", &[SlotValue::Bool(true)], &schema,
                        &mut destination, EncodeLimits::default())
                } else {
                    encode(&Literal(b"null"), &mut destination, EncodeLimits::default())
                };
                assert_eq!(result.unwrap_err(), external_error());
                assert_eq!(destination.reservations, 1);
                assert_eq!(destination.writes, usize::from(!fail_reserve));
                assert_eq!(&destination.bytes[..7], b"prefix:");
                assert_eq!(&destination.bytes[7..], if fail_reserve { &b""[..] }
                    else if slots { &b"{\""[..] } else { &b"nu"[..] });
            }
        }
    }

    #[test]
    fn a_sink_cannot_claim_success_after_a_short_write() {
        let mut destination = Destination { short_write: true, ..Destination::default() };
        assert_error(encode(&Literal(b"null"), &mut destination, EncodeLimits::default()).unwrap_err(),
            ErrorCode::InternalError);
    }

    #[test]
    fn slot_order_is_schema_position_and_empty_is_absent() {
        let mut first = field(99, "z", Some(ScalarKind::True));
        first.required = true;
        let schema = schema(vec![first, field(1, "omit", None),
            field(u32::MAX, "", Some(ScalarKind::Null)), field(40, "a", Some(ScalarKind::Number))]);
        let values = [SlotValue::Bool(false), SlotValue::Empty, raw_value(b" null "), SlotValue::U64(5)];
        assert_eq!(render(b" null ", b"", &values, &schema), b"{\"z\":false,\"\": null ,\"a\":5}");
        let mut permissive = schema.clone();
        permissive.reject_unknown = false;
        assert_eq!(render(b" null ", b"", &values, &permissive), render(b" null ", b"", &values, &schema));
    }

    #[test]
    fn empty_schema_and_all_optional_empty_slots_emit_empty_object() {
        assert_eq!(render(b"unrelated", b"", &[], &schema(vec![])), b"{}");
        assert_eq!(render(b"", b"", &[SlotValue::Empty], &schema(vec![field(8, "v", None)])), b"{}");
        let schema = schema(vec![field(8, "v", None)]);
        reject(b"", b"", &[], &schema, EncodeLimits::default(), ErrorCode::ConstraintViolation, 0);
        reject(b"", b"", &[SlotValue::Empty, SlotValue::Empty], &schema,
            EncodeLimits::default(), ErrorCode::ConstraintViolation, 0);
    }

    #[test]
    fn missing_required_field_cannot_publish_an_earlier_valid_field() {
        let mut required = field(27, "required", None);
        required.required = true;
        let schema = schema(vec![field(4, "first", None), required]);
        reject(b"", b"", &[SlotValue::Bool(true), SlotValue::Empty], &schema,
            EncodeLimits::default(), ErrorCode::MissingRequiredField, 27);
    }

    #[test]
    fn all_numeric_and_boolean_slots_use_canonical_writer_output() {
        let vectors: &[(SlotValue, ScalarKind, &[u8])] = &[
            (SlotValue::Bool(true), ScalarKind::False, b"{\"v\":true}"),
            (SlotValue::Bool(false), ScalarKind::True, b"{\"v\":false}"),
            (SlotValue::U64(u64::MAX), ScalarKind::Number, b"{\"v\":18446744073709551615}"),
            (SlotValue::I64(i64::MIN), ScalarKind::Number, b"{\"v\":-9223372036854775808}"),
            (SlotValue::F64Bits((-0.0f64).to_bits()), ScalarKind::Number, b"{\"v\":-0}"),
            (SlotValue::F64Bits(1.25f64.to_bits()), ScalarKind::Number, b"{\"v\":1.25}"),
            (SlotValue::Decimal(Decimal128::new(123, 2)), ScalarKind::String, b"{\"v\":\"1.23\"}"),
            (SlotValue::Decimal(Decimal128::new(-1, 3)), ScalarKind::String, b"{\"v\":\"-0.001\"}"),
            (SlotValue::Decimal(Decimal128::new(0, 2)), ScalarKind::String, b"{\"v\":\"0.00\"}"),
            (SlotValue::Decimal(Decimal128::new(i128::MIN, 0)), ScalarKind::String,
                b"{\"v\":\"-170141183460469231731687303715884105728\"}"),
            (SlotValue::Decimal(Decimal128::new(i128::MIN, 38)), ScalarKind::String,
                b"{\"v\":\"-1.70141183460469231731687303715884105728\"}"),
        ];
        for &(value, kind, expected) in vectors {
            assert_eq!(render(b"", b"", &[value], &schema(vec![field(8, "v", Some(kind))])), expected);
        }
    }

    #[test]
    fn nonfinite_float_bits_and_invalid_decimal_scales_are_rejected() {
        let schema = schema(vec![field(8, "v", None)]);
        for bits in [f64::NAN.to_bits(), f64::INFINITY.to_bits(), f64::NEG_INFINITY.to_bits(),
            0x7ff0_0000_0000_0001, 0xfff8_0000_0000_0001]
        {
            reject(b"", b"", &[SlotValue::F64Bits(bits)], &schema,
                EncodeLimits::default(), ErrorCode::InvalidNumber, 8);
        }
        for scale in [39, u8::MAX] {
            reject(b"", b"", &[SlotValue::Decimal(Decimal128::new(0, scale))], &schema,
                EncodeLimits::default(), ErrorCode::DecimalScaleMismatch, 8);
        }
    }

    #[test]
    fn owned_string_spans_resolve_exact_sources_and_escape_decoded_contents() {
        let payload = "xé".as_bytes();
        let decoded = "\n😀\"\\".as_bytes();
        let schema = schema(vec![field(1, "raw", Some(ScalarKind::String)),
            field(2, "decoded", Some(ScalarKind::String)), field(3, "end", Some(ScalarKind::String))]);
        let values = [
            SlotValue::Str(StrRef { span: Span32::new(1, 2), source: SpanSource::RawPayload, scalar_len: 1 }),
            SlotValue::Str(StrRef { span: Span32::new(0, decoded.len() as u32), source: SpanSource::DecodedArena, scalar_len: 4 }),
            SlotValue::Str(StrRef { span: Span32::new(payload.len() as u32, 0), source: SpanSource::RawPayload, scalar_len: 0 }),
        ];
        assert_eq!(render(payload, decoded, &values, &schema),
            "{\"raw\":\"é\",\"decoded\":\"\\n😀\\\"\\\\\",\"end\":\"\"}".as_bytes());
    }

    #[test]
    fn invalid_string_owners_spans_utf8_and_scalar_metadata_fail_without_panics() {
        let schema = schema(vec![field(8, "v", Some(ScalarKind::String))]);
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for span in [Span32::new(u32::MAX, 2), Span32::new(4, 0), Span32::new(0, 4)] {
                let value = SlotValue::Str(StrRef { span, source, scalar_len: 0 });
                reject(b"abc", b"abc", &[value], &schema,
                    EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
            }
            for (span, code) in [(Span32::new(0, 1), ErrorCode::InvalidUtf8),
                (Span32::new(0, 2), ErrorCode::ConstraintViolation)]
            {
                let value = SlotValue::Str(StrRef { span, source, scalar_len: 2 });
                reject("é".as_bytes(), "é".as_bytes(), &[value], &schema,
                    EncodeLimits::default(), code, 8);
            }
        }
    }

    #[test]
    fn scalar_categories_are_checked_against_actual_output() {
        let values = [SlotValue::Bool(true), SlotValue::U64(0),
            SlotValue::Decimal(Decimal128::new(0, 0)), raw_value(b"null")];
        let kinds = [ScalarKind::True, ScalarKind::Number, ScalarKind::String, ScalarKind::Null];
        for (value_index, value) in values.into_iter().enumerate() {
            for (kind_index, kind) in kinds.into_iter().enumerate() {
                let schema = schema(vec![field(8, "v", Some(kind))]);
                if value_index == kind_index {
                    render(b"null", b"", &[value], &schema);
                } else {
                    reject(b"null", b"", &[value], &schema, EncodeLimits::default(), ErrorCode::TypeMismatch, 8);
                }
            }
        }
        for bytes in [b"[]".as_slice(), b"{}".as_slice()] {
            for kind in kinds {
                reject(bytes, b"", &[raw_value(bytes)], &schema(vec![field(8, "v", Some(kind))]),
                    EncodeLimits::default(), ErrorCode::TypeMismatch, 8);
            }
            let output = render(bytes, b"", &[raw_value(bytes)], &schema(vec![field(8, "v", None)]));
            assert_eq!(&output[5..output.len() - 1], bytes);
        }
    }

    #[test]
    fn raw_values_are_revalidated_with_exact_diagnostics_and_field_context() {
        let schema = schema(vec![field(8, "v", None)]);
        for bytes in [b"[0,]".as_slice(), b"null null".as_slice(), br#""\uD800x""#.as_slice(),
            b"\"\xff\"".as_slice(), b"".as_slice()]
        {
            let expected = raw::validate(bytes, EncodeLimits::default()).unwrap_err();
            let actual = reject(bytes, b"", &[raw_value(bytes)], &schema,
                EncodeLimits::default(), expected.code, 8);
            assert_eq!(actual.byte_offset, expected.byte_offset);
            assert_eq!(actual.detail, expected.detail);
        }
        for span in [Span32::new(0, 5), Span32::new(5, 0), Span32::new(u32::MAX, 1)] {
            reject(b"null", b"", &[SlotValue::RawSpan(span)], &schema,
                EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
    }

    #[test]
    fn raw_strings_count_unicode_scalars_instead_of_escape_bytes_or_utf16_units() {
        let mut field = field(8, "v", Some(ScalarKind::String));
        field.min_len = Some(2);
        field.max_len = Some(2);
        let schema = schema(vec![field]);
        for bytes in [br#" "\uD83D\uDE00\u0061" "#.as_slice(), "\"😀a\"".as_bytes(),
            br#""\n\u0000""#.as_slice(), "\"é\"".as_bytes()]
        {
            render(bytes, b"", &[raw_value(bytes)], &schema);
        }
        for bytes in [br#""\uD83D\uDE00""#.as_slice(), b"\"abc\"".as_slice(), b"\"\"".as_slice()] {
            reject(bytes, b"", &[raw_value(bytes)], &schema,
                EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
    }

    #[test]
    fn decimal_string_bounds_apply_to_fixed_scale_output() {
        let mut field = field(8, "v", Some(ScalarKind::String));
        field.min_len = Some(5);
        field.max_len = Some(5);
        let schema = schema(vec![field]);
        assert_eq!(render(b"", b"", &[SlotValue::Decimal(Decimal128::new(-123, 2))], &schema),
            b"{\"v\":\"-1.23\"}");
        for value in [Decimal128::new(123, 2), Decimal128::new(-123, 3)] {
            reject(b"", b"", &[SlotValue::Decimal(value)], &schema,
                EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
    }

    #[test]
    fn integer_bounds_are_exact_at_signed_unsigned_and_float_precision_edges() {
        let mut field = field(8, "v", Some(ScalarKind::Number));
        field.min_i64 = Some(i64::MIN);
        field.max_i64 = Some(i64::MAX);
        let schema = schema(vec![field]);
        for value in [SlotValue::I64(i64::MIN), SlotValue::I64(i64::MAX), SlotValue::U64(i64::MAX as u64)] {
            render(b"", b"", &[value], &schema);
        }
        for value in [SlotValue::U64(i64::MAX as u64 + 1), SlotValue::U64(u64::MAX)] {
            reject(b"", b"", &[value], &schema, EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
        for bytes in [b"-9223372036854775809".as_slice(), b"9223372036854775808".as_slice(),
            b"999999999999999999999999999999999999999999999".as_slice()]
        {
            reject(bytes, b"", &[raw_value(bytes)], &schema,
                EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
        let mut precise = schema.clone();
        precise.fields[0].min_i64 = Some(9_007_199_254_740_993);
        precise.fields[0].max_i64 = Some(9_007_199_254_740_993);
        render(b"", b"", &[SlotValue::U64(9_007_199_254_740_993)], &precise);
        reject(b"", b"", &[SlotValue::F64Bits(9_007_199_254_740_992.0f64.to_bits())], &precise,
            EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
    }

    #[test]
    fn integer_grammar_rejects_fraction_and_exponent_even_when_mathematically_integral() {
        let mut field = field(8, "v", Some(ScalarKind::Number));
        field.min_i64 = Some(0);
        let schema = schema(vec![field]);
        for bytes in [b"1.0".as_slice(), b"1e0".as_slice(), b"1E+00".as_slice(), b"-0.0".as_slice()] {
            reject(bytes, b"", &[raw_value(bytes)], &schema,
                EncodeLimits::default(), ErrorCode::TypeMismatch, 8);
        }
        for bytes in [b"0".as_slice(), b"-0".as_slice(), b" 1 \n".as_slice(),
            b"999999999999999999999999999999999999999999999999999".as_slice()]
        {
            render(bytes, b"", &[raw_value(bytes)], &schema);
        }
        let mut negative = schema.clone();
        negative.fields[0].min_i64 = None;
        negative.fields[0].max_i64 = Some(-1);
        let huge = b"-999999999999999999999999999999999999999999999999999";
        render(huge, b"", &[raw_value(huge)], &negative);
    }

    #[test]
    fn exact_integer_comparison_vectors_include_negative_zero_and_extremes() {
        let vectors: &[(&[u8], i64, Ordering)] = &[
            (b"-0", 0, Ordering::Equal), (b"0", -1, Ordering::Greater),
            (b"-0", 1, Ordering::Less), (b"-1", 0, Ordering::Less),
            (b"-10", -9, Ordering::Less), (b"-9", -10, Ordering::Greater),
            (b"9223372036854775807", i64::MAX, Ordering::Equal),
            (b"-9223372036854775808", i64::MIN, Ordering::Equal),
            (b"18446744073709551615", i64::MAX, Ordering::Greater),
            (b"-18446744073709551615", i64::MIN, Ordering::Less),
        ];
        for &(token, bound, order) in vectors { assert_eq!(compare_integer(token, bound), order); }
        for bound in [-100i64, -1, 0, 1, 100, i64::MIN, i64::MAX] {
            let mut scratch = VecSink::new(64);
            let fragment = render_slot(SlotValue::I64(bound), b"", b"", &mut scratch, EncodeLimits::default()).unwrap();
            assert_eq!(compare_integer(fragment.as_bytes(), bound), Ordering::Equal);
        }
    }

    #[test]
    fn schema_preflight_rejects_duplicate_names_and_ids_globally() {
        for fields in [
            vec![field(1, "", None), field(2, "middle", None), field(3, "", None)],
            vec![field(7, "a", None), field(2, "b", None), field(7, "c", None)],
        ] {
            let schema = schema(fields);
            let mut destination = Destination::default();
            let error = encode_values(b"", b"", &[SlotValue::Empty; 3], &schema,
                &mut destination, EncodeLimits::default()).unwrap_err();
            assert_error(error.clone(), ErrorCode::ConstraintViolation);
            assert_eq!(error.schema_id, 91);
            assert!([1, 3, 7].contains(&error.field_id));
            assert_eq!((destination.reservations, destination.writes), (0, 0));
        }
    }

    #[test]
    fn schema_preflight_rejects_inaccurate_required_count() {
        let mut schema = schema(vec![field(8, "v", None)]);
        schema.required_count = 1;
        reject(b"", b"", &[SlotValue::Empty], &schema,
            EncodeLimits::default(), ErrorCode::ConstraintViolation, 0);
        schema.required_count = 0;
        schema.fields[0].required = true;
        reject(b"", b"", &[SlotValue::Bool(true)], &schema,
            EncodeLimits::default(), ErrorCode::ConstraintViolation, 0);
    }

    #[test]
    fn every_named_format_is_rejected_even_on_absent_optional_fields() {
        for format in ["", "uuid", "date-time", "unknown"] {
            let mut field = field(8, "v", Some(ScalarKind::String));
            field.format = Some(String::from(format));
            reject(b"", b"", &[SlotValue::Empty], &schema(vec![field]),
                EncodeLimits::default(), ErrorCode::UnsupportedSchemaFeature, 8);
        }
    }

    #[test]
    fn unsupported_formats_are_not_hidden_by_other_malformed_schema_metadata() {
        let mut malformed = field(1, "bad", None);
        malformed.min_len = Some(1);
        let mut formatted = field(8, "v", None);
        formatted.format = Some(String::new());
        reject(b"", b"", &[SlotValue::Empty; 2], &schema(vec![malformed, formatted]),
            EncodeLimits::default(), ErrorCode::UnsupportedSchemaFeature, 8);
    }

    #[test]
    fn required_count_overflow_is_rejected_before_duplicate_scanning() {
        let mut template = field(8, "v", None);
        template.required = true;
        let schema = SchemaIr {
            schema_id: SchemaId(91), required_count: u16::MAX,
            fields: vec![template; usize::from(u16::MAX) + 1], ..SchemaIr::default()
        };
        let error = preflight(&schema).unwrap_err();
        assert_error(error.clone(), ErrorCode::ConstraintViolation);
        assert_eq!((error.schema_id, error.field_id), (91, 8));
        assert_eq!(error.detail, "required field count exceeds the ABI range");
    }

    #[test]
    fn zero_length_bounds_accept_empty_strings_and_reject_single_scalars() {
        let mut field = field(8, "v", Some(ScalarKind::String));
        field.min_len = Some(0);
        field.max_len = Some(0);
        let schema = schema(vec![field]);
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            let empty = SlotValue::Str(StrRef { span: Span32::new(0, 0), source, scalar_len: 0 });
            assert_eq!(render(b"", b"", &[empty], &schema), b"{\"v\":\"\"}");
            let scalar = SlotValue::Str(StrRef { span: Span32::new(0, 4), source, scalar_len: 1 });
            reject("😀".as_bytes(), "😀".as_bytes(), &[scalar], &schema,
                EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
        assert_eq!(render(b"\"\"", b"", &[raw_value(b"\"\"")], &schema), b"{\"v\":\"\"}");
    }

    #[test]
    fn scalar_constraint_families_and_ordered_bounds_are_preflighted() {
        for kind in [None, Some(ScalarKind::Null), Some(ScalarKind::True), Some(ScalarKind::False),
            Some(ScalarKind::Number), Some(ScalarKind::String)]
        {
            for integer in [false, true] {
                let allowed = if integer { Some(ScalarKind::Number) } else { Some(ScalarKind::String) };
                if kind == allowed { continue; }
                let mut field = field(8, "v", kind);
                if integer { field.min_i64 = Some(0); } else { field.max_len = Some(1); }
                reject(b"", b"", &[SlotValue::Empty], &schema(vec![field]),
                    EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
            }
        }
        let mut length = field(8, "v", Some(ScalarKind::String));
        length.min_len = Some(2);
        length.max_len = Some(1);
        let mut integer = field(8, "v", Some(ScalarKind::Number));
        integer.min_i64 = Some(1);
        integer.max_i64 = Some(0);
        for field in [length, integer] {
            reject(b"", b"", &[SlotValue::Empty], &schema(vec![field]),
                EncodeLimits::default(), ErrorCode::ConstraintViolation, 8);
        }
    }

    #[test]
    fn slot_resource_boundaries_include_object_syntax_keys_and_raw_nesting() {
        let schema = schema(vec![field(8, "v", None)]);
        let default = EncodeLimits::default();
        let vectors = [
            (b"[]".as_slice(), EncodeLimits { max_depth: 1, ..default }, ErrorCode::DepthLimitExceeded, 8),
            (b"[0,1]".as_slice(), EncodeLimits { max_container_items: 1, ..default }, ErrorCode::BudgetExhausted, 8),
            (br#""\uD83D\uDE00""#.as_slice(), EncodeLimits { max_string_bytes: 3, ..default }, ErrorCode::BudgetExhausted, 8),
            (b"100".as_slice(), EncodeLimits { max_number_bytes: 2, ..default }, ErrorCode::BudgetExhausted, 8),
            (b"null".as_slice(), EncodeLimits { max_output_bytes: 9, ..default }, ErrorCode::PayloadTooLarge, 0),
        ];
        for (bytes, limits, code, field_id) in vectors {
            reject(bytes, b"", &[raw_value(bytes)], &schema, limits, code, field_id);
        }
        for (limits, expected_field) in [
            (EncodeLimits { max_container_items: 0, ..default }, 8),
            (EncodeLimits { max_string_bytes: 0, ..default }, 8),
        ] {
            reject(b"", b"", &[SlotValue::Bool(true)], &schema,
                limits, ErrorCode::BudgetExhausted, expected_field);
        }
        reject(b"", b"", &[SlotValue::Empty], &schema,
            EncodeLimits { max_depth: 0, ..default }, ErrorCode::DepthLimitExceeded, 0);
        let mut destination = b"prefix".to_vec();
        encode_values(b"null", b"", &[raw_value(b"null")], &schema, &mut destination,
            EncodeLimits { max_output_bytes: 10, ..default }).unwrap();
        assert_eq!(destination, b"prefix{\"v\":null}");
        let mut destination = Vec::new();
        encode_values(b"[]", b"", &[raw_value(b"[]")], &schema, &mut destination,
            EncodeLimits { max_depth: 2, max_container_items: 1, ..default }).unwrap();
        assert_eq!(destination, b"{\"v\":[]}");
    }

    #[test]
    fn output_limit_accounts_for_final_delimiter_and_escaped_keys() {
        let schema = schema(vec![field(8, "\n", None)]);
        let expected = b"{\"\\n\":true}";
        let default = EncodeLimits::default();
        for limit in [0, 1, expected.len() - 1] {
            let mut destination = Destination::default();
            let error = encode_values(b"", b"", &[SlotValue::Bool(true)], &schema,
                &mut destination, EncodeLimits { max_output_bytes: limit, ..default }).unwrap_err();
            assert_error(error, ErrorCode::PayloadTooLarge);
            assert_eq!((destination.reservations, destination.writes), (0, 0));
        }
        let mut destination = Vec::new();
        encode_values(b"", b"", &[SlotValue::Bool(true)], &schema, &mut destination,
            EncodeLimits { max_output_bytes: expected.len(), ..default }).unwrap();
        assert_eq!(destination, expected);
    }

    #[test]
    fn writer_number_and_decimal_string_limits_are_rechecked() {
        let schema = schema(vec![field(8, "v", None)]);
        let default = EncodeLimits::default();
        for (value, limits) in [
            (SlotValue::U64(100), EncodeLimits { max_number_bytes: 2, ..default }),
            (SlotValue::I64(-1), EncodeLimits { max_number_bytes: 1, ..default }),
            (SlotValue::Decimal(Decimal128::new(123, 2)), EncodeLimits { max_number_bytes: 3, ..default }),
            (SlotValue::Decimal(Decimal128::new(123, 2)), EncodeLimits { max_string_bytes: 3, ..default }),
        ] {
            reject(b"", b"", &[value], &schema, limits, ErrorCode::BudgetExhausted, 8);
        }
    }

    #[test]
    fn public_slots_entry_point_keeps_the_frozen_signature_without_issuing_proofs() {
        // ABI proofs can only be issued through an unsafe validator boundary.
        // This crate forbids unsafe code and must never manufacture evidence.
        // All value tests therefore exercise its exact private implementation.
        let entry: fn(&ValidatedSlots, &SchemaIr, &mut dyn ByteSink, EncodeLimits) -> Result<()> = encode_slots;
        let _ = entry;
    }
}
