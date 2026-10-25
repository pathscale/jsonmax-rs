//! Allocation-free integer and finite binary64 output with staged sink writes.

use super::*;
use core::fmt;

/// Append the minimal base-10 spelling of any unsigned 64-bit integer.
///
/// At most [`MAX_U64_TEXT_BYTES`] bytes are appended. The complete spelling is
/// prepared before reserving its exact length and issuing one sink write.
/// Sink errors retain their original metadata; a failed write may leave a prefix.
pub fn write_u64(value: u64, sink: &mut dyn ByteSink) -> Result<()> {
    let mut bytes = [0_u8; MAX_U64_TEXT_BYTES];
    let start = decimal_digits(value, &mut bytes);
    publish(&bytes[start..], sink)
}

/// Append the minimal base-10 spelling of any signed 64-bit integer.
///
/// At most [`MAX_I64_TEXT_BYTES`] bytes are appended, including the minus sign.
/// The unsigned magnitude also represents `i64::MIN` without signed negation.
/// Reservation and write failures propagate unchanged.
pub fn write_i64(value: i64, sink: &mut dyn ByteSink) -> Result<()> {
    let mut bytes = [0_u8; MAX_I64_TEXT_BYTES];
    let mut start = decimal_digits(value.unsigned_abs(), &mut bytes);
    if value < 0 {
        // A signed magnitude has at most 19 digits in this 20-byte buffer.
        start -= 1;
        bytes[start] = b'-';
    }
    publish(&bytes[start..], sink)
}

/// Append a finite binary64 value using shortest round-trip significant digits.
///
/// Core's precision-free `Display` formatting uses ordinary decimal notation.
/// This deliberately avoids explicit exponents, satisfying every nonnegative
/// `max_exponent_abs`, including zero. Decimal placement may require leading or
/// trailing zeros, but the significant digits are not padded to fixed precision.
/// All finite values fit [`NumberText`], including the smallest subnormal.
/// Allowed negative zero retains its minus sign, and quoted-integer permission
/// has no effect on output.
///
/// An invalid exponent policy is rejected first. Every NaN and infinity is then
/// rejected, even if the read-side policy permits non-finite conversion results.
/// These errors and forbidden negative zero leave the sink untouched. Formatting
/// and grammar validation also finish before the exact reservation and write.
/// Sink errors propagate unchanged; a failed write may leave a prefix.
pub fn write_f64(
    value: f64,
    policy: &NumericPolicy,
    sink: &mut dyn ByteSink,
) -> Result<()> {
    if policy.max_exponent_abs < 0 {
        return Err(encode_error(
            ErrorCode::ConstraintViolation,
            "maximum absolute exponent must be nonnegative",
        ));
    }
    if !value.is_finite() {
        return Err(encode_error(
            ErrorCode::NumberOverflow,
            "JSON numbers cannot represent a non-finite binary64 value",
        ));
    }
    if policy.reject_negative_zero && value == 0.0 && value.is_sign_negative() {
        return Err(encode_error(
            ErrorCode::ConstraintViolation,
            "negative zero is forbidden by the numeric policy",
        ));
    }

    let text = format_ordinary(format_args!("{value}"))?;
    publish(text.as_bytes(), sink)
}

// Both callers supply 20 bytes. Every u64 has at most 20 decimal digits, so
// each decrement has room. The loop writes zero once as well. Remainders are
// at most nine, making the narrowing conversion and ASCII addition exact.
fn decimal_digits(mut value: u64, bytes: &mut [u8]) -> usize {
    let mut start = bytes.len();
    loop {
        start -= 1;
        bytes[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return start;
        }
    }
}

// This is only called with the finite f64 Display formatter in production.
// Keeping formatting fallible also handles capacity exhaustion or an unexpected
// formatter failure without exposing any part of the local scratch to a sink.
fn format_ordinary(arguments: fmt::Arguments<'_>) -> Result<NumberText> {
    let mut text = NumberText::new();
    fmt::write(&mut text, arguments).map_err(|_| {
        encode_error(ErrorCode::InternalError, "number formatting exceeded scratch or failed")
    })?;
    validate_ordinary(text.as_bytes())?;
    Ok(text)
}

// Validate the entire ordinary spelling, not just a valid prefix. No tokens
// are constructed here and no read-side policy or Lexer error is introduced.
fn validate_ordinary(bytes: &[u8]) -> Result<()> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    let point = digits.iter().position(|&byte| byte == b'.');
    let integer = &digits[..point.unwrap_or(digits.len())];
    let valid_integer = !integer.is_empty()
        && integer.iter().all(u8::is_ascii_digit)
        && (integer.len() == 1 || integer[0] != b'0');
    let valid_fraction = match point {
        Some(index) => {
            // position found an existing byte, so index + 1 is in bounds.
            let fraction = &digits[index + 1..];
            !fraction.is_empty() && fraction.iter().all(u8::is_ascii_digit)
        }
        None => true,
    };
    if valid_integer && valid_fraction {
        Ok(())
    } else {
        Err(encode_error(
            ErrorCode::InternalError,
            "number formatter produced an invalid ordinary JSON spelling",
        ))
    }
}

fn publish(bytes: &[u8], sink: &mut dyn ByteSink) -> Result<()> {
    sink.reserve(bytes.len())?;
    sink.write_bytes(bytes)
}

fn encode_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}
