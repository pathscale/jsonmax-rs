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
