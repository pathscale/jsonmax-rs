//! Exact, allocation-free quoted fixed-point decimal output.

use super::*;

/// Return the exact quoted ASCII length, including the sign and decimal point.
///
/// Scale zero has no point. A positive scale always contributes exactly that
/// many fractional digits, and the integer part always has at least one digit.
/// The result is at most [`MAX_DECIMAL_TEXT_LEN`] for every `Decimal128`.
pub fn encoded_len(value: Decimal128) -> usize {
    let digits = usize::from(coefficient_precision(value.coefficient));
    let sign = usize::from(value.coefficient < 0);
    let scale = usize::from(value.scale);
    if scale == 0 {
        2 + sign + digits
    } else {
        // Two quotes, one point, and enough digits for a nonempty integer.
        3 + sign + core::cmp::max(digits, scale + 1)
    }
}

/// Append a complete quoted fixed-point representation to `sink`.
///
/// Formatting finishes in a bounded local buffer before touching the sink.
/// This function reserves the exact encoded length once, then calls
/// [`ByteSink::write_bytes`] once. Any sink error is returned unchanged. A
/// failed reservation prevents the write; a failed write may have made partial
/// progress according to the sink's own semantics.
pub fn write_quoted(value: Decimal128, sink: &mut dyn ByteSink) -> Result<()> {
    let mut buffer = [0_u8; MAX_DECIMAL_TEXT_LEN];
    let bytes = format_quoted(value, &mut buffer);
    sink.reserve(bytes.len())?;
    sink.write_bytes(bytes)
}

/// Write into the start of `output` and return only the written prefix.
///
/// A short destination returns [`ErrorCode::BudgetExhausted`] with source
/// [`ErrorSource::Encode`] at offset zero, without changing any byte. On
/// success, bytes beyond the returned prefix retain their original contents.
pub fn write_to_slice<'a>(
    value: Decimal128,
    output: &'a mut [u8],
) -> Result<&'a [u8]> {
    let len = encoded_len(value);
    if output.len() < len {
        return Err(Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Encode,
            0,
            "decimal output buffer is too short",
        ));
    }

    let mut buffer = [0_u8; MAX_DECIMAL_TEXT_LEN];
    let bytes = format_quoted(value, &mut buffer);
    output[..len].copy_from_slice(bytes);
    Ok(&output[..len])
}

// Write backward into the buffer and return its initialized suffix. An i128
// magnitude has at most 39 digits. With positive scale s, the representation
// has max(digits, s + 1) digit bytes, one point, at most one minus and two
// quotes. Since s <= 255, at most 260 bytes are needed. With scale zero,
// at most 42 bytes are needed. These bounds prove every cursor decrement and
// index below is valid, including the final opening quote at index zero for
// a negative coefficient at scale 255.
fn format_quoted(
    value: Decimal128,
    buffer: &mut [u8; MAX_DECIMAL_TEXT_LEN],
) -> &[u8] {
    let mut magnitude = value.coefficient.unsigned_abs();
    let mut cursor = buffer.len() - 1;
    buffer[cursor] = b'"';

    if value.scale != 0 {
        for _ in 0..value.scale {
            cursor -= 1;
            // The remainder is in 0..=9, so narrowing and ASCII addition fit.
            buffer[cursor] = b'0' + (magnitude % 10) as u8;
            magnitude /= 10;
        }
        cursor -= 1;
        buffer[cursor] = b'.';
    }

    // Emit one integer digit even after the fractional digits consume the
    // entire magnitude. Unsigned division handles i128::MIN without negation.
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value.coefficient < 0 {
        cursor -= 1;
        buffer[cursor] = b'-';
    }
    cursor -= 1;
    buffer[cursor] = b'"';
    &buffer[cursor..]
}
