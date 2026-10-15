//! Checked JSON writers for generated key/value adapters.
//!
//! Scalar formatting uses stack storage. Object writing first checks the whole
//! field plan and resolves every selected value, then emits in caller order.
//! Local validation failures leave the sink untouched; sink failures propagate
//! unchanged and may leave a prefix. Raw spans are never treated as JSON proof.

use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::{
    aot_error, ByteSink, Decimal128, EncodeField, Error, ErrorCode, ErrorSource,
    FieldId, Result, SlotKind, SlotReader, SlotSpec, SlotValue, StrRef,
};

/// Write a quoted JSON string, retaining all non-control UTF-8 bytes exactly.
/// Quotes and backslashes are escaped; controls use short escapes or `\u00xx`.
pub fn write_json_string(value: &str, sink: &mut dyn ByteSink) -> Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    sink.write_byte(b'"')?;
    let bytes = value.as_bytes();
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let escaped: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\x08' => b"\\b",
            b'\x0c' => b"\\f",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0..=0x1f => &[],
            _ => continue,
        };
        if start != index {
            sink.write_bytes(&bytes[start..index])?;
        }
        if escaped.is_empty() {
            sink.write_bytes(&[
                b'\\', b'u', b'0', b'0',
                HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)],
            ])?;
        } else {
            sink.write_bytes(escaped)?;
        }
        start = index + 1;
    }
    if start != bytes.len() {
        sink.write_bytes(&bytes[start..])?;
    }
    sink.write_byte(b'"')
}

/// Write the JSON Boolean literal.
pub fn write_bool(value: bool, sink: &mut dyn ByteSink) -> Result<()> {
    sink.write_bytes(if value { b"true" } else { b"false" })
}

/// Write an unsigned integer in exact base ten, with no leading zeroes.
pub fn write_u64(value: u64, sink: &mut dyn ByteSink) -> Result<()> {
    let mut digits = [0; 39];
    sink.write_bytes(decimal_digits(u128::from(value), &mut digits))
}

/// Write a signed integer, including `i64::MIN`, without signed negation.
pub fn write_i64(value: i64, sink: &mut dyn ByteSink) -> Result<()> {
    let mut digits = [0; 39];
    let digits = decimal_digits(u128::from(value.unsigned_abs()), &mut digits);
    let mut output = [0; 40];
    let start = usize::from(value < 0);
    if start != 0 {
        output[0] = b'-';
    }
    output[start..start + digits.len()].copy_from_slice(digits);
    sink.write_bytes(&output[..start + digits.len()])
}

/// Write a finite float in round-tripping scientific decimal notation.
/// Negative zero is preserved. Nonfinite values fail before touching the sink.
pub fn write_f64(value: f64, sink: &mut dyn ByteSink) -> Result<()> {
    let text = float_text(value)?;
    sink.write_bytes(text.as_bytes())
}

/// Write an exact, unquoted decimal, preserving every fractional scale place.
/// All coefficients and scales are supported, including `i128::MIN` and 255.
pub fn write_decimal(value: Decimal128, sink: &mut dyn ByteSink) -> Result<()> {
    let mut digits = [0; 39];
    let digits = decimal_digits(value.coefficient.unsigned_abs(), &mut digits);
    // The longest spelling is a sign, "0.", and 255 fractional digits.
    let mut output = [b'0'; 258];
    let mut end = usize::from(value.coefficient < 0);
    if end != 0 {
        output[0] = b'-';
    }
    let scale = usize::from(value.scale);
    if scale == 0 {
        output[end..end + digits.len()].copy_from_slice(digits);
        end += digits.len();
    } else if scale < digits.len() {
        let split = digits.len() - scale;
        output[end..end + split].copy_from_slice(&digits[..split]);
        end += split;
        output[end] = b'.';
        end += 1;
        output[end..end + scale].copy_from_slice(&digits[split..]);
        end += scale;
    } else {
        // The initialized buffer supplies both the integer zero and padding.
        output[end + 1] = b'.';
        end += 2 + scale - digits.len();
        output[end..end + digits.len()].copy_from_slice(digits);
        end += digits.len();
    }
    sink.write_bytes(&output[..end])
}

/// Preflight and write an object in the supplied field order.
///
/// Keys, slot indices, and field identifiers must each be unique. Fields may
/// select a subset of slots. Optional empty slots become `null` unless omitted;
/// present zeroes, false Booleans, and empty strings are always emitted. Required
/// empty slots fail. Raw field specifications are unsupported even if optional
/// and empty, since bounded spans do not establish JSON validity.
///
/// Validation uses O(n) fallibly allocated scratch and O(n log n) comparisons.
/// Every local error has Encode source and field context (schema identifier 0).
/// Sink errors keep their original metadata and may leave partial output.
pub fn write_slots(
    reader: &SlotReader<'_>,
    fields: &[EncodeField<'_>],
    sink: &mut dyn ByteSink,
) -> Result<()> {
    write_from(reader, fields, sink)
}

fn encode_error(code: ErrorCode, detail: &'static str) -> Error {
    aot_error(code, ErrorSource::Encode, detail)
}

fn field_error(mut error: Error, field_id: FieldId) -> Error {
    error.source = ErrorSource::Encode;
    error.with_schema(0, field_id.0)
}

fn decimal_digits(mut value: u128, buffer: &mut [u8; 39]) -> &[u8] {
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buffer[start..];
        }
    }
}

// Finite f64 scientific output needs at most 24 bytes: sign, 17 significant
// digits, decimal point, 'e', exponent sign, and three exponent digits. Core's
// precision-free LowerExp formatter chooses a round-tripping decimal. A checked
// buffer keeps formatting failures local and separate from ByteSink failures.
struct FloatText {
    bytes: [u8; 32],
    len: usize,
}

impl FloatText {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for FloatText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        target.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn float_text(value: f64) -> Result<FloatText> {
    if !value.is_finite() {
        return Err(encode_error(ErrorCode::InvalidNumber, "cannot encode a nonfinite float"));
    }
    let mut text = FloatText { bytes: [0; 32], len: 0 };
    if value == 0.0 && value.is_sign_negative() {
        // Explicitly retain the sign even if core formatting conventions change.
        text.bytes[..4].copy_from_slice(b"-0e0");
        text.len = 4;
    } else {
        write!(&mut text, "{value:e}").map_err(|_| encode_error(
            ErrorCode::InternalError, "finite float exceeded its formatting buffer",
        ))?;
    }
    Ok(text)
}

// Private static dispatch lets tests exercise the production algorithm without
// constructing ValidationProof. The public API accepts only the root reader.
trait SlotAccess<'a> {
    fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue>;
    fn resolve_str(&self, value: StrRef) -> Result<&'a str>;
}

impl<'a> SlotAccess<'a> for SlotReader<'a> {
    fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue> {
        SlotReader::slot(self, index, field_id)
    }

    fn resolve_str(&self, value: StrRef) -> Result<&'a str> {
        SlotReader::resolve_str(self, value)
    }
}

enum PreparedValue<'a> {
    Empty,
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(FloatText),
    Decimal(Decimal128),
    Str(&'a str),
}

fn prepare_value<'a>(reader: &impl SlotAccess<'a>, spec: &SlotSpec) -> Result<PreparedValue<'a>> {
    let value = reader.slot(spec.index, spec.field_id)?;
    if value == SlotValue::Empty && spec.required {
        return Err(encode_error(ErrorCode::MissingRequiredField, "required slot is empty"));
    }
    if spec.kind == SlotKind::Raw {
        return Err(encode_error(
            ErrorCode::UnsupportedSchemaFeature, "raw slots do not certify JSON fragments",
        ));
    }
    match (spec.kind, value) {
        (_, SlotValue::Empty) => Ok(PreparedValue::Empty),
        (SlotKind::Bool, SlotValue::Bool(value)) => Ok(PreparedValue::Bool(value)),
        (SlotKind::U64, SlotValue::U64(value)) => Ok(PreparedValue::U64(value)),
        (SlotKind::I64, SlotValue::I64(value)) => Ok(PreparedValue::I64(value)),
        (SlotKind::F64, SlotValue::F64Bits(bits)) => {
            float_text(f64::from_bits(bits)).map(PreparedValue::F64)
        }
        (SlotKind::Decimal, SlotValue::Decimal(value)) => Ok(PreparedValue::Decimal(value)),
        (SlotKind::Str, SlotValue::Str(value)) => reader.resolve_str(value).map(PreparedValue::Str),
        _ => Err(encode_error(ErrorCode::TypeMismatch, "slot value has the wrong encoder type")),
    }
}

fn validate_fields(fields: &[EncodeField<'_>]) -> Result<()> {
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(fields.len()).map_err(|_| scratch_error(fields))?;
    ordered.extend(0..fields.len());
    // Tie-break by caller position to report the later conflicting field.
    ordered.sort_unstable_by_key(|&index| (fields[index].wire_name, index));
    for pair in ordered.windows(2) {
        let first = &fields[pair[0]];
        let second = &fields[pair[1]];
        if first.wire_name == second.wire_name {
            return Err(field_error(encode_error(
                ErrorCode::DuplicateObjectKey, "encode plan has duplicate object keys",
            ), second.slot.field_id));
        }
    }
    ordered.sort_unstable_by_key(|&index| (fields[index].slot.index, index));
    for pair in ordered.windows(2) {
        let first = &fields[pair[0]];
        let second = &fields[pair[1]];
        if first.slot.index == second.slot.index {
            return Err(field_error(encode_error(
                ErrorCode::SchemaImageInvalid, "encode plan has duplicate slot indices",
            ), second.slot.field_id));
        }
    }
    ordered.sort_unstable_by_key(|&index| (fields[index].slot.field_id.0, index));
    for pair in ordered.windows(2) {
        let first = &fields[pair[0]];
        let second = &fields[pair[1]];
        if first.slot.field_id == second.slot.field_id {
            return Err(field_error(encode_error(
                ErrorCode::SchemaImageInvalid, "encode plan has duplicate field identifiers",
            ), second.slot.field_id));
        }
    }
    Ok(())
}

fn scratch_error(fields: &[EncodeField<'_>]) -> Error {
    field_error(encode_error(
        ErrorCode::BudgetExhausted, "cannot allocate encode preflight scratch",
    ), fields.first().map_or(FieldId(0), |field| field.slot.field_id))
}

fn write_from<'a>(
    reader: &impl SlotAccess<'a>,
    fields: &[EncodeField<'_>],
    sink: &mut dyn ByteSink,
) -> Result<()> {
    validate_fields(fields)?;
    let mut values = Vec::new();
    values.try_reserve_exact(fields.len()).map_err(|_| scratch_error(fields))?;
    for field in fields {
        values.push(prepare_value(reader, &field.slot)
            .map_err(|error| field_error(error, field.slot.field_id))?);
    }

    sink.write_byte(b'{')?;
    let mut first = true;
    for (field, value) in fields.iter().zip(values.iter()) {
        if field.omit_empty && matches!(value, PreparedValue::Empty) {
            continue;
        }
        if !first {
            sink.write_byte(b',')?;
        }
        first = false;
        write_json_string(field.wire_name, sink)?;
        sink.write_byte(b':')?;
        match value {
            PreparedValue::Empty => sink.write_bytes(b"null"),
            PreparedValue::Bool(value) => write_bool(*value, sink),
            PreparedValue::U64(value) => write_u64(*value, sink),
            PreparedValue::I64(value) => write_i64(*value, sink),
            PreparedValue::F64(text) => sink.write_bytes(text.as_bytes()),
            PreparedValue::Decimal(value) => write_decimal(*value, sink),
            PreparedValue::Str(value) => write_json_string(value, sink),
        }?;
    }
    sink.write_byte(b'}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::{String, ToString}, vec};
    use core::cell::Cell;
    use crate::{Span32, SpanSource};

    const KINDS: [SlotKind; 7] = [
        SlotKind::Bool, SlotKind::U64, SlotKind::I64, SlotKind::F64,
        SlotKind::Decimal, SlotKind::Str, SlotKind::Raw,
    ];

    // This is unproven test data, never a ValidatedSlots or ValidationProof.
    // Resolver behavior models SlotReader so the same private preflight and
    // emission path can be exercised without a safe proof producer dependency.
    struct Fixture<'a> {
        values: &'a [SlotValue],
        payload: &'a [u8],
        decoded: &'a [u8],
        reads: Cell<usize>,
        resolutions: Cell<usize>,
        resolver_error: Option<Error>,
    }

    impl<'a> Fixture<'a> {
        fn new(values: &'a [SlotValue], payload: &'a [u8], decoded: &'a [u8]) -> Self {
            Self {
                values, payload, decoded, reads: Cell::new(0),
                resolutions: Cell::new(0), resolver_error: None,
            }
        }
    }

    impl<'a> SlotAccess<'a> for Fixture<'a> {
        fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue> {
            self.reads.set(self.reads.get() + 1);
            usize::try_from(index).ok().and_then(|index| self.values.get(index)).copied()
                .ok_or_else(|| Error::new(
                    ErrorCode::SchemaImageInvalid, ErrorSource::Decode, 0,
                    "fixture slot out of bounds",
                ).with_schema(0, field_id.0))
        }

        fn resolve_str(&self, value: StrRef) -> Result<&'a str> {
            self.resolutions.set(self.resolutions.get() + 1);
            if let Some(error) = &self.resolver_error {
                return Err(error.clone());
            }
            let owner = match value.source {
                SpanSource::RawPayload => self.payload,
                SpanSource::DecodedArena => self.decoded,
            };
            let bytes = value.span.slice(owner).ok_or_else(|| Error::new(
                ErrorCode::SchemaImageInvalid, ErrorSource::Decode, 0,
                "fixture string out of bounds",
            ))?;
            let text = core::str::from_utf8(bytes).map_err(|_| Error::new(
                ErrorCode::InvalidUtf8, ErrorSource::Decode, value.span.offset,
                "fixture invalid UTF-8",
            ))?;
            if usize::try_from(value.scalar_len).ok() != Some(text.chars().count()) {
                return Err(Error::new(
                    ErrorCode::SchemaImageInvalid, ErrorSource::Decode, 0,
                    "fixture scalar count mismatch",
                ));
            }
            Ok(text)
        }
    }

    fn field(name: &str, index: u32, kind: SlotKind) -> EncodeField<'_> {
        EncodeField {
            wire_name: name,
            slot: SlotSpec { index, field_id: FieldId(index), kind, required: true },
            omit_empty: false,
        }
    }

    fn string_ref(source: SpanSource, offset: u32, len: u32, scalar_len: u32) -> StrRef {
        StrRef { source, span: Span32::new(offset, len), scalar_len }
    }

    fn rendered(writer: impl FnOnce(&mut dyn ByteSink) -> Result<()>) -> String {
        let mut output = Vec::new();
        writer(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn assert_local_error(error: &Error, code: ErrorCode, field_id: FieldId) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, field_id.0);
        assert!(!error.detail.is_empty());
    }

    fn sink_error() -> Error {
        Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 97, "injected sink failure")
            .with_schema(81, 73)
    }

    struct LimitedSink {
        bytes: Vec<u8>,
        remaining: usize,
        calls: usize,
    }

    impl LimitedSink {
        fn new(remaining: usize) -> Self {
            Self { bytes: b"prefix:".to_vec(), remaining, calls: 0 }
        }
    }

    impl ByteSink for LimitedSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.calls += 1;
            if additional > self.remaining { Err(sink_error()) } else { Ok(()) }
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.calls += 1;
            let count = bytes.len().min(self.remaining);
            self.bytes.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            if count != bytes.len() { Err(sink_error()) } else { Ok(()) }
        }

        fn len(&self) -> usize {
            self.bytes.len()
        }
    }

    fn rejected<'a>(
        reader: &impl SlotAccess<'a>, fields: &[EncodeField<'_>],
        code: ErrorCode, field_id: FieldId,
    ) -> Error {
        let mut sink = LimitedSink::new(0);
        let error = write_from(reader, fields, &mut sink).unwrap_err();
        assert_local_error(&error, code, field_id);
        assert_eq!(sink.calls, 0, "preflight must not even reserve the sink");
        assert_eq!(sink.bytes, b"prefix:");
        error
    }

    fn assert_sink_boundaries(writer: impl Fn(&mut dyn ByteSink) -> Result<()>) {
        let expected = rendered(|sink| writer(sink));
        for limit in 0..expected.len() {
            let mut sink = LimitedSink::new(limit);
            assert_eq!(writer(&mut sink).unwrap_err(), sink_error());
            assert_eq!(&sink.bytes[..7], b"prefix:");
            assert_eq!(&sink.bytes[7..], &expected.as_bytes()[..limit]);
        }
        let mut sink = LimitedSink::new(expected.len());
        writer(&mut sink).unwrap();
        assert_eq!(&sink.bytes[7..], expected.as_bytes());
        assert_eq!(sink.remaining, 0);
    }

    // Independent JSON number grammar recognition, without an external parser.
    fn assert_number(text: &str) {
        let bytes = text.as_bytes();
        let mut index = usize::from(bytes.first() == Some(&b'-'));
        match bytes.get(index) {
            Some(b'0') => index += 1,
            Some(b'1'..=b'9') => {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_digit) { index += 1; }
            }
            _ => panic!("missing JSON integer: {text}"),
        }
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) { index += 1; }
            assert!(index > start, "missing fraction: {text}");
        }
        if matches!(bytes.get(index), Some(b'e' | b'E')) {
            index += 1;
            if matches!(bytes.get(index), Some(b'+' | b'-')) { index += 1; }
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) { index += 1; }
            assert!(index > start, "missing exponent: {text}");
        }
        assert_eq!(index, bytes.len(), "invalid JSON number: {text}");
    }

    #[test]
    fn strings_escape_every_ascii_control_and_preserve_other_bytes() {
        let controls = concat!(
            "\"\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007",
            "\\b\\t\\n\\u000b\\f\\r\\u000e\\u000f",
            "\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015\\u0016\\u0017",
            "\\u0018\\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f\"",
        );
        let input: String = (0u8..32).map(char::from).collect();
        assert_eq!(rendered(|sink| write_json_string(&input, sink)), controls);
        for (input, expected) in [
            ("", "\"\""),
            ("plain / text", "\"plain / text\""),
            ("\"\\", "\"\\\"\\\\\""),
            ("a\n\t\rb", "\"a\\n\\t\\rb\""),
            ("é中🦀e\u{301}\u{2028}\u{2029}\u{7f}\u{85}", "\"é中🦀e\u{301}\u{2028}\u{2029}\u{7f}\u{85}\""),
            ("\0é\x1f🦀\"", "\"\\u0000é\\u001f🦀\\\"\""),
        ] {
            assert_eq!(rendered(|sink| write_json_string(input, sink)), expected);
        }
        let printable: String = (32u8..=127).map(char::from).collect();
        let expected = format!("\"{}\"", printable.replace('\\', "\\\\").replace('"', "\\\""));
        assert_eq!(rendered(|sink| write_json_string(&printable, sink)), expected);
        let long = "é/中".repeat(4096);
        assert_eq!(rendered(|sink| write_json_string(&long, sink)), format!("\"{long}\""));
    }

    #[test]
    fn booleans_and_integer_extremes_have_exact_spelling() {
        assert_eq!(rendered(|sink| write_bool(true, sink)), "true");
        assert_eq!(rendered(|sink| write_bool(false, sink)), "false");
        for value in [0, 1, 9, 10, 99, 100, u32::MAX as u64, 1 << 63, u64::MAX] {
            let text = rendered(|sink| write_u64(value, sink));
            assert_eq!(text, value.to_string());
            assert_number(&text);
            assert_eq!(text.parse::<u64>().unwrap(), value);
        }
        for value in [i64::MIN, i64::MIN + 1, i32::MIN as i64, -100, -1, 0, 1, i64::MAX] {
            let text = rendered(|sink| write_i64(value, sink));
            assert_eq!(text, value.to_string());
            assert_number(&text);
            assert_eq!(text.parse::<i64>().unwrap(), value);
        }
        assert_eq!(rendered(|sink| write_i64(i64::MIN, sink)), "-9223372036854775808");
        assert_eq!(rendered(|sink| write_u64(u64::MAX, sink)), "18446744073709551615");
        let mut power = 1u64;
        for _ in 0..19 {
            for value in [power - 1, power, power + 1] {
                assert_eq!(rendered(|sink| write_u64(value, sink)), value.to_string());
            }
            power *= 10;
        }
        let mut digits = [0; 39];
        assert_eq!(decimal_digits(u128::MAX, &mut digits), b"340282366920938463463374607431768211455");
    }

    fn assert_float_round_trip(bits: u64) {
        let value = f64::from_bits(bits);
        if !value.is_finite() { return; }
        let text = rendered(|sink| write_f64(value, sink));
        assert_number(&text);
        assert!(text.len() <= 24);
        assert_eq!(text.parse::<f64>().unwrap().to_bits(), bits, "{text}");
    }

    #[test]
    fn floats_round_trip_extremes_exponents_and_deterministic_bit_patterns() {
        for (value, expected) in [(0.0, "0e0"), (-0.0, "-0e0"), (1.0, "1e0"), (-1.25, "-1.25e0")] {
            assert_eq!(rendered(|sink| write_f64(value, sink)), expected);
        }
        for value in [
            f64::MIN, f64::MAX, f64::MIN_POSITIVE, -f64::MIN_POSITIVE,
            f64::EPSILON, 0.1, -0.1, 1e-300, 1e300, 9007199254740991.0,
        ] {
            assert_float_round_trip(value.to_bits());
        }
        for exponent in 0u64..2047 {
            for fraction in [0, 1, (1u64 << 51) - 1, (1u64 << 52) - 1] {
                let bits = (exponent << 52) | fraction;
                assert_float_round_trip(bits);
                assert_float_round_trip(bits | (1u64 << 63));
            }
        }
        let mut bits = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..4096 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            assert_float_round_trip(bits);
        }
    }

    #[test]
    fn nonfinite_floats_fail_before_any_sink_access() {
        for bits in [
            0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
            0x7ff0_0000_0000_0001, 0xfff0_0000_0000_0001,
            0x7ff8_0000_0000_0042, 0xfff8_0000_0000_0042, u64::MAX,
        ] {
            let mut sink = LimitedSink::new(0);
            let error = write_f64(f64::from_bits(bits), &mut sink).unwrap_err();
            assert_local_error(&error, ErrorCode::InvalidNumber, FieldId(0));
            assert_eq!(sink.calls, 0);
            assert_eq!(sink.bytes, b"prefix:");
        }
    }

    #[test]
    fn float_buffer_rejects_overflow_without_mutating_existing_text() {
        let mut text = FloatText { bytes: [0; 32], len: 0 };
        text.write_str("123").unwrap();
        assert!(text.write_str(&"4".repeat(30)).is_err());
        assert_eq!(text.as_bytes(), b"123");
        text.write_str(&"5".repeat(29)).unwrap();
        assert_eq!(text.len, 32);
        text.write_str("").unwrap();
        assert!(text.write_str("6").is_err());
        assert_eq!(text.len, 32);
    }

    #[test]
    fn decimals_preserve_every_scale_and_full_coefficient_range() {
        for (coefficient, scale, expected) in [
            (0, 0, "0"), (0, 3, "0.000"), (1, 3, "0.001"), (-1, 3, "-0.001"),
            (123, 1, "12.3"), (123, 3, "0.123"), (1200, 2, "12.00"),
            (i128::MIN, 0, "-170141183460469231731687303715884105728"),
            (i128::MIN, 38, "-1.70141183460469231731687303715884105728"),
            (i128::MAX, 39, "0.170141183460469231731687303715884105727"),
        ] {
            assert_eq!(rendered(|sink| write_decimal(Decimal128::new(coefficient, scale), sink)), expected);
        }
        for coefficient in [i128::MIN, i128::MIN + 1, -100, -1, 0, 1, 10, 12300, i128::MAX] {
            for scale in 0..=u8::MAX {
                let text = rendered(|sink| write_decimal(Decimal128::new(coefficient, scale), sink));
                assert_number(&text);
                assert!(text.len() <= 258);
                assert_eq!(text.starts_with('-'), coefficient < 0);
                if scale == 0 {
                    assert!(!text.contains('.'));
                } else {
                    assert_eq!(text.split_once('.').unwrap().1.len(), usize::from(scale));
                }
                let digits = text.trim_start_matches('-').replace('.', "");
                let magnitude = digits.trim_start_matches('0');
                let magnitude = if magnitude.is_empty() { "0" } else { magnitude };
                assert_eq!(magnitude, coefficient.unsigned_abs().to_string());
                // Removing the point and redundant leading zeroes recovers the
                // exact signed coefficient without a floating-point conversion.
                let signed = if coefficient < 0 { format!("-{magnitude}") } else { magnitude.to_string() };
                assert_eq!(signed.parse::<i128>().unwrap(), coefficient);
            }
        }
        let longest = rendered(|sink| write_decimal(Decimal128::new(-1, 255), sink));
        assert_eq!(longest.len(), 258);
        assert_eq!(longest, format!("-0.{}1", "0".repeat(254)));
    }

    #[test]
    fn scalar_sink_errors_are_unchanged_at_every_byte_boundary() {
        assert_sink_boundaries(|sink| write_json_string("é\0\"\\\n🦀\x1f", sink));
        assert_sink_boundaries(|sink| write_json_string("", sink));
        assert_sink_boundaries(|sink| write_bool(false, sink));
        assert_sink_boundaries(|sink| write_bool(true, sink));
        assert_sink_boundaries(|sink| write_u64(u64::MAX, sink));
        assert_sink_boundaries(|sink| write_i64(i64::MIN, sink));
        assert_sink_boundaries(|sink| write_f64(-f64::MAX, sink));
        assert_sink_boundaries(|sink| write_f64(-0.0, sink));
        assert_sink_boundaries(|sink| write_decimal(Decimal128::new(i128::MIN, 255), sink));
    }

    #[test]
    fn object_emits_all_types_in_caller_order_with_exact_keys_and_numbers() {
        let values = [
            SlotValue::Bool(false), SlotValue::U64(u64::MAX), SlotValue::I64(i64::MIN),
            SlotValue::F64Bits((-0.0f64).to_bits()), SlotValue::Decimal(Decimal128::new(-1200, 3)),
            SlotValue::Str(string_ref(SpanSource::DecodedArena, 1, 5, 2)), SlotValue::Empty,
        ];
        let reader = Fixture::new(&values, b"unrelated", "xé中z".as_bytes());
        let mut fields = [
            field("s\"\n", 5, SlotKind::Str), field("b", 0, SlotKind::Bool),
            field("u", 1, SlotKind::U64), field("i", 2, SlotKind::I64),
            field("f", 3, SlotKind::F64), field("d", 4, SlotKind::Decimal),
            field("", 6, SlotKind::Bool),
        ];
        fields[6].slot.required = false;
        let expected = concat!(
            "{\"s\\\"\\n\":\"é中\",\"b\":false,\"u\":18446744073709551615,",
            "\"i\":-9223372036854775808,\"f\":-0e0,\"d\":-1.200,\"\":null}",
        );
        for _ in 0..2 {
            assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)), expected);
        }
        assert_eq!(reader.reads.get(), 14);
        assert_eq!(reader.resolutions.get(), 2);
        assert_sink_boundaries(|sink| write_from(&reader, &fields, sink));
    }

    #[test]
    fn empty_objects_subsets_and_omission_preserve_comma_rules() {
        let values = [SlotValue::Empty, SlotValue::Bool(false), SlotValue::Empty, SlotValue::RawSpan(Span32::new(u32::MAX, 1))];
        let reader = Fixture::new(&values, &[], &[]);
        assert_eq!(rendered(|sink| write_from(&reader, &[], sink)), "{}");
        assert_eq!(reader.reads.get(), 0);
        let mut fields = [field("a", 0, SlotKind::Str), field("b", 1, SlotKind::Bool), field("c", 2, SlotKind::U64)];
        for f in &mut fields { f.slot.required = false; f.omit_empty = true; }
        assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)), "{\"b\":false}");
        assert_eq!(rendered(|sink| write_from(&reader, &[fields[0], fields[2]], sink)), "{}");
        fields[0].omit_empty = false;
        assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)), "{\"a\":null,\"b\":false}");
        fields[2].omit_empty = false;
        assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)), "{\"a\":null,\"b\":false,\"c\":null}");
        assert_sink_boundaries(|sink| write_from(&reader, &[], sink));
    }

    #[test]
    fn present_false_zero_and_empty_strings_are_never_omitted() {
        let values = [
            SlotValue::Bool(false), SlotValue::U64(0), SlotValue::I64(0),
            SlotValue::F64Bits(0), SlotValue::Decimal(Decimal128::new(0, 2)),
            SlotValue::Str(string_ref(SpanSource::RawPayload, 0, 0, 0)),
        ];
        let reader = Fixture::new(&values, &[], &[]);
        let mut fields: Vec<_> = KINDS[..6].iter().enumerate().map(|(index, &kind)| {
            field(["b", "u", "i", "f", "d", "s"][index], index as u32, kind)
        }).collect();
        for f in &mut fields { f.omit_empty = true; }
        assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)),
            "{\"b\":false,\"u\":0,\"i\":0,\"f\":0e0,\"d\":0.00,\"s\":\"\"}");
    }

    #[test]
    fn required_empty_and_raw_fields_fail_even_when_omitted() {
        for kind in KINDS {
            for omit_empty in [false, true] {
                let reader = Fixture::new(&[SlotValue::Empty], &[], &[]);
                let mut f = field("v", 0, kind);
                f.omit_empty = omit_empty;
                f.slot.field_id = FieldId(u32::MAX);
                rejected(&reader, &[f], ErrorCode::MissingRequiredField, f.slot.field_id);
                f.slot.required = false;
                if kind == SlotKind::Raw {
                    rejected(&reader, &[f], ErrorCode::UnsupportedSchemaFeature, f.slot.field_id);
                } else {
                    let expected = if omit_empty { "{}" } else { "{\"v\":null}" };
                    assert_eq!(rendered(|sink| write_from(&reader, &[f], sink)), expected);
                }
            }
        }
        for span in [Span32::new(0, 4), Span32::new(0, 0), Span32::new(u32::MAX, 1)] {
            let values = [SlotValue::RawSpan(span)];
            let reader = Fixture::new(&values, b"null", &[]);
            rejected(&reader, &[field("raw", 0, SlotKind::Raw)], ErrorCode::UnsupportedSchemaFeature, FieldId(0));
        }
    }

    #[test]
    fn complete_type_matrix_rejects_coercions_and_preflights_late_errors() {
        let variants = [
            SlotValue::Bool(true), SlotValue::U64(1), SlotValue::I64(1),
            SlotValue::F64Bits(1.0f64.to_bits()), SlotValue::Decimal(Decimal128::new(1, 0)),
            SlotValue::Str(string_ref(SpanSource::RawPayload, 0, 1, 1)),
            SlotValue::RawSpan(Span32::new(0, 1)),
        ];
        for (kind_index, kind) in KINDS.into_iter().enumerate() {
            for (value_index, value) in variants.into_iter().enumerate() {
                for required in [false, true] {
                    let values = [SlotValue::Bool(true), value];
                    let reader = Fixture::new(&values, b"1", &[]);
                    let mut last = field("last", 1, kind);
                    last.slot.required = required;
                    last.omit_empty = true;
                    let fields = [field("first", 0, SlotKind::Bool), last];
                    if kind == SlotKind::Raw {
                        rejected(&reader, &fields, ErrorCode::UnsupportedSchemaFeature, FieldId(1));
                    } else if kind_index != value_index {
                        rejected(&reader, &fields, ErrorCode::TypeMismatch, FieldId(1));
                    } else {
                        assert!(rendered(|sink| write_from(&reader, &fields, sink)).starts_with("{\"first\":true,"));
                    }
                }
            }
        }
        for bits in [f64::INFINITY.to_bits(), f64::NEG_INFINITY.to_bits(), f64::NAN.to_bits(), u64::MAX] {
            let values = [SlotValue::Bool(true), SlotValue::F64Bits(bits)];
            let reader = Fixture::new(&values, &[], &[]);
            rejected(&reader, &[field("ok", 0, SlotKind::Bool), field("bad", 1, SlotKind::F64)], ErrorCode::InvalidNumber, FieldId(1));
        }
    }

    #[test]
    fn duplicates_are_rejected_before_access_including_omitted_fields() {
        let values = [SlotValue::Empty, SlotValue::Empty];
        let reader = Fixture::new(&values, &[], &[]);
        for name in ["", "same", "\"\n\\", "é🦀"] {
            let mut fields = [field(name, 0, SlotKind::Bool), field(name, 1, SlotKind::Bool)];
            for f in &mut fields { f.slot.required = false; f.omit_empty = true; }
            fields[1].slot.field_id = FieldId(u32::MAX);
            rejected(&reader, &fields, ErrorCode::DuplicateObjectKey, FieldId(u32::MAX));
        }
        let mut fields = [field("a", 0, SlotKind::Bool), field("b", 1, SlotKind::Bool)];
        fields[1].slot.index = 0;
        rejected(&reader, &fields, ErrorCode::SchemaImageInvalid, FieldId(1));
        fields[1].slot.index = 1;
        fields[1].slot.field_id = fields[0].slot.field_id;
        rejected(&reader, &fields, ErrorCode::SchemaImageInvalid, FieldId(0));
        assert_eq!(reader.reads.get(), 0);
        assert_eq!(reader.resolutions.get(), 0);
    }

    #[test]
    fn distinct_unicode_and_escaped_key_spellings_remain_distinct() {
        let values = [SlotValue::Bool(true); 4];
        let reader = Fixture::new(&values, &[], &[]);
        let fields = [
            field("é", 0, SlotKind::Bool), field("e\u{301}", 1, SlotKind::Bool),
            field("\n", 2, SlotKind::Bool), field("\\n", 3, SlotKind::Bool),
        ];
        assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)),
            "{\"é\":true,\"e\u{301}\":true,\"\\n\":true,\"\\\\n\":true}");
    }

    #[test]
    fn slot_bounds_errors_keep_full_width_field_context() {
        let reader = Fixture::new(&[SlotValue::Bool(true)], &[], &[]);
        for index in [1, u32::MAX] {
            let mut f = field("missing", index, SlotKind::Bool);
            f.slot.field_id = FieldId(u32::MAX);
            f.slot.required = false;
            f.omit_empty = true;
            rejected(&reader, &[f], ErrorCode::SchemaImageInvalid, f.slot.field_id);
        }
    }

    #[test]
    fn strings_resolve_both_owners_and_exact_end_empty_ranges() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for (offset, len, scalars, expected) in [(1, 6, 2, "é🦀"), (7, 0, 0, "")] {
                let values = [SlotValue::Str(string_ref(source, offset, len, scalars))];
                let owner = "xé🦀".as_bytes();
                let reader = match source {
                    SpanSource::RawPayload => Fixture::new(&values, owner, b"\xff"),
                    SpanSource::DecodedArena => Fixture::new(&values, b"\xff", owner),
                };
                let fields = [field("s", 0, SlotKind::Str)];
                assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)), format!("{{\"s\":\"{expected}\"}}"));
                assert_eq!(reader.reads.get(), 1);
                assert_eq!(reader.resolutions.get(), 1);
            }
        }
    }

    #[test]
    fn invalid_string_spans_utf8_and_scalar_counts_fail_before_output() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for (offset, len, count, code) in [
                (8, 0, 0, ErrorCode::SchemaImageInvalid),
                (6, 2, 1, ErrorCode::SchemaImageInvalid),
                (u32::MAX, 1, 0, ErrorCode::SchemaImageInvalid),
                (0, u32::MAX, 0, ErrorCode::SchemaImageInvalid),
                (1, 1, 1, ErrorCode::InvalidUtf8),
                (2, 1, 1, ErrorCode::InvalidUtf8),
                (1, 6, 6, ErrorCode::SchemaImageInvalid),
                (1, 6, u32::MAX, ErrorCode::SchemaImageInvalid),
                (7, 0, 1, ErrorCode::SchemaImageInvalid),
            ] {
                let values = [SlotValue::Bool(true), SlotValue::Str(string_ref(source, offset, len, count))];
                let owner = "xé🦀".as_bytes();
                let reader = Fixture::new(&values, owner, owner);
                let fields = [field("ok", 0, SlotKind::Bool), field("bad", 1, SlotKind::Str)];
                let error = rejected(&reader, &fields, code, FieldId(1));
                if code == ErrorCode::InvalidUtf8 { assert_eq!(error.byte_offset, offset); }
            }
        }
    }

    #[test]
    fn resolver_errors_change_only_encode_and_field_context() {
        let values = [SlotValue::Str(string_ref(SpanSource::RawPayload, 0, 0, 0))];
        let mut reader = Fixture::new(&values, &[], &[]);
        let original = Error::new(ErrorCode::InvalidUtf8, ErrorSource::Decode, 97, "resolver detail")
            .with_schema(42, 11);
        reader.resolver_error = Some(original.clone());
        let mut f = field("s", 0, SlotKind::Str);
        f.slot.field_id = FieldId(u32::MAX);
        let actual = rejected(&reader, &[f], original.code, f.slot.field_id);
        let expected = Error { source: ErrorSource::Encode, schema_id: 0, field_id: u32::MAX, ..original };
        assert_eq!(actual, expected);
    }

    #[test]
    fn large_reversed_plan_preserves_order_and_detects_interior_duplicates() {
        let values = vec![SlotValue::Bool(true); 2048];
        let names: Vec<_> = (0..values.len()).map(|index| format!("k{index}")).collect();
        let reader = Fixture::new(&values, &[], &[]);
        let mut fields: Vec<_> = (0..values.len()).rev().map(|index| {
            let mut f = field(&names[index], index as u32, SlotKind::Bool);
            f.slot.field_id = FieldId(u32::MAX - index as u32);
            f
        }).collect();
        let expected = format!("{{{}}}", names.iter().rev().map(|name| format!("\"{name}\":true")).collect::<Vec<_>>().join(","));
        assert_eq!(rendered(|sink| write_from(&reader, &fields, sink)), expected);
        assert_eq!(reader.reads.get(), 2048);
        fields[1024].slot.field_id = fields[1023].slot.field_id;
        rejected(&reader, &fields, ErrorCode::SchemaImageInvalid, fields[1024].slot.field_id);
        assert_eq!(reader.reads.get(), 2048);
    }

    #[test]
    fn public_signatures_match_the_frozen_contract() {
        let _: fn(&str, &mut dyn ByteSink) -> Result<()> = write_json_string;
        let _: fn(bool, &mut dyn ByteSink) -> Result<()> = write_bool;
        let _: fn(u64, &mut dyn ByteSink) -> Result<()> = write_u64;
        let _: fn(i64, &mut dyn ByteSink) -> Result<()> = write_i64;
        let _: fn(f64, &mut dyn ByteSink) -> Result<()> = write_f64;
        let _: fn(Decimal128, &mut dyn ByteSink) -> Result<()> = write_decimal;
        let _: fn(&SlotReader<'_>, &[EncodeField<'_>], &mut dyn ByteSink) -> Result<()> = write_slots;
    }
}
