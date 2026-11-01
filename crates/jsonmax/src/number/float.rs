//! Correctly rounded, allocation-free conversion of complete decimal tokens.

use super::*;

/// Convert a validated numeric token to IEEE-754 binary64, rounding to nearest
/// with ties to even. Integer and permitted quoted-integer tokens are accepted.
///
/// The supplied policy is checked again even when lexing used a different one.
/// Finite underflow is accepted, including signed zero unless forbidden by the
/// policy. Overflow produces signed infinity only when the policy permits it.
/// All errors use absolute source offsets and [`ErrorSource::Lexer`].
///
/// Core's safe decimal parser consumes the complete spelling and handles long
/// significands, halfway cases, and subnormals without an allocation or a lossy
/// intermediate integer or floating-point scaling step.
pub fn parse_f64(token: &NumberToken<'_>, policy: &NumericPolicy) -> Result<f64> {
    let start = token.span().offset;
    if policy.max_exponent_abs < 0 {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            start,
            "maximum absolute exponent must be nonnegative",
        ));
    }
    if token.is_quoted() && !policy.allow_Quoted_int {
        return Err(Error::new(
            ErrorCode::InvalidNumber,
            ErrorSource::Lexer,
            start,
            "quoted integers are disabled",
        ));
    }

    let limit = i32::from(policy.max_exponent_abs);
    // Compare both signed bounds instead of taking an absolute value. The
    // nonnegative i16 limit can always be negated after widening to i32.
    if token.exponent() < -limit || token.exponent() > limit {
        let raw = token.raw_bytes();
        let marker = raw.iter().position(|&byte| matches!(byte, b'e' | b'E'))
            .ok_or_else(|| invalid_number(start))?;
        // A lexed exponent marker is followed by an optional sign and at least
        // one digit, so these increments stay within the borrowed spelling.
        let mut digit = marker + 1;
        if matches!(raw.get(digit), Some(b'+' | b'-')) {
            digit += 1;
        }
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            spelling_offset(token, digit)?,
            "explicit exponent exceeds the policy limit",
        ));
    }

    let value = parse_decimal(token.raw_bytes(), start)?;
    // Checking the rounded result covers both lexical negative zero and a
    // negative nonzero decimal that underflows to zero. Wrapper quotes shift
    // the minus sign one byte beyond the source token's start.
    if policy.reject_negative_zero && value == 0.0 && value.is_sign_negative() {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            spelling_offset(token, 0)?,
            "negative zero is forbidden by the numeric policy",
        ));
    }
    if policy.reject_non_finite_float && !value.is_finite() {
        return Err(Error::new(
            ErrorCode::NumberOverflow,
            ErrorSource::Lexer,
            start,
            "number rounds outside the finite binary64 range",
        ));
    }
    Ok(value)
}

/// Convert exclusively to [`ConvertedNumber::F64Bits`], retaining the exact
/// result bits, including signed zero and any infinity permitted by the policy.
pub fn convert_float(
    token: &NumberToken<'_>,
    policy: &NumericPolicy,
) -> Result<ConvertedNumber> {
    parse_f64(token, policy).map(|value| ConvertedNumber::F64Bits(value.to_bits()))
}

// Lex owns grammar validation. This helper accepts only the token's unquoted
// spelling in production; core's broader grammar is never exposed as a public
// entry point. Safe UTF-8 and parser failures retain the token-start diagnostic.
fn parse_decimal(raw: &[u8], start: u32) -> Result<f64> {
    let text = core::str::from_utf8(raw).map_err(|_| invalid_number(start))?;
    text.parse::<f64>().map_err(|_| invalid_number(start))
}

fn invalid_number(start: u32) -> Error {
    Error::new(
        ErrorCode::InvalidNumber,
        ErrorSource::Lexer,
        start,
        "numeric spelling could not be converted to binary64",
    )
}

// Lexed spans fit the original input's checked u32 length. Keep the conversion
// and additions checked so error reporting cannot truncate or wrap an offset.
fn spelling_offset(token: &NumberToken<'_>, relative: usize) -> Result<u32> {
    let start = token.span().offset;
    let relative = u32::try_from(relative).map_err(|_| invalid_number(start))?;
    start.checked_add(u32::from(token.is_quoted()))
        .and_then(|offset| offset.checked_add(relative))
        .ok_or_else(|| invalid_number(start))
}
