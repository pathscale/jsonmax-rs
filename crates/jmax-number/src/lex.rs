//! Allocation-free recognition of strict JSON numbers and permitted quoted integers.

use super::*;

/// Lex one number beginning exactly at `offset`, without skipping whitespace.
///
/// The returned spelling borrows `input`; its span also includes wrapper quotes
/// when quoted integers are enabled. A successful token ends at EOF or a strict
/// JSON number delimiter. Validation of the surrounding JSON belongs to the
/// caller, so bytes after that delimiter are not inspected.
///
/// Input length and caller policy are validated before scanning. Grammar and
/// token boundaries are validated before exponent and negative-zero constraints.
/// Every error uses absolute source offsets and [`ErrorSource::Lexer`].
pub fn lex_number<'a>(
    input: &'a [u8],
    offset: u32,
    policy: &NumericPolicy,
) -> Result<NumberToken<'a>> {
    let _t = jmax_trace::span!(NUMBER_LEX);
    let input_len = checked_input_len(input)?;
    if policy.max_exponent_abs < 0 {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            offset,
            "maximum absolute exponent must be nonnegative",
        ));
    }
    if offset >= input_len {
        return Err(Error::new(
            ErrorCode::UnexpectedEof,
            ErrorSource::Lexer,
            offset,
            "expected a number at the requested offset",
        ));
    }

    // offset < input_len <= u32::MAX and input_len == input.len(). Thus this
    // conversion fits usize even on a target whose usize is narrower than u32.
    // Every subsequent cursor advance consumes an existing byte, so cursors,
    // differences, and all narrowing conversions remain within these bounds.
    let start = offset as usize;
    let mut cursor = start;
    let quoted = input[cursor] == b'"';
    if quoted {
        if !policy.allow_Quoted_int {
            return Err(invalid_number(cursor, "quoted integers are disabled"));
        }
        cursor += 1;
    }
    let spelling_start = cursor;
    let negative = input.get(cursor) == Some(&b'-');
    if negative {
        cursor += 1;
    }

    let integer_start = cursor;
    let mut nonzero_significand = false;
    match input.get(cursor).copied() {
        Some(b'0') => {
            cursor += 1;
            if input.get(cursor).is_some_and(u8::is_ascii_digit) {
                return Err(invalid_number(cursor, "leading zero in integer part"));
            }
        }
        Some(b'1'..=b'9') => {
            nonzero_significand = true;
            cursor += 1;
            while input.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
        }
        _ => return Err(invalid_number(cursor, "expected an integer digit")),
    }
    let integer_digits = (cursor - integer_start) as u32;
    let mut fraction_digits = 0;
    let mut exponent = 0;
    let mut exponent_start = 0;
    let mut exponent_exceeds_policy = false;
    let mut kind = if negative {
        NumberLexKind::SignedInt
    } else {
        NumberLexKind::UnsignedInt
    };
    let spelling_end;

    if quoted {
        spelling_end = cursor;
        match input.get(cursor) {
            Some(b'"') => cursor += 1,
            None => {
                return Err(Error::new(
                    ErrorCode::UnexpectedEof,
                    ErrorSource::Lexer,
                    input_len,
                    "expected the closing quote of an integer",
                ));
            }
            _ => return Err(invalid_number(cursor, "expected the closing quote of an integer")),
        }
        kind = NumberLexKind::QuotedDecimal;
    } else {
        if input.get(cursor) == Some(&b'.') {
            kind = NumberLexKind::FloatDecimal;
            cursor += 1;
            let fraction_start = cursor;
            while let Some(&byte) = input.get(cursor) {
                if !byte.is_ascii_digit() {
                    break;
                }
                nonzero_significand |= byte != b'0';
                cursor += 1;
            }
            if cursor == fraction_start {
                return Err(invalid_number(cursor, "expected a fraction digit"));
            }
            fraction_digits = (cursor - fraction_start) as u32;
        }

        if matches!(input.get(cursor), Some(b'e' | b'E')) {
            kind = NumberLexKind::FloatDecimal;
            cursor += 1;
            let exponent_negative = input.get(cursor) == Some(&b'-');
            if matches!(input.get(cursor), Some(b'+' | b'-')) {
                cursor += 1;
            }
            exponent_start = cursor;
            let limit = i32::from(policy.max_exponent_abs);
            let mut magnitude = 0_i32;
            while let Some(&byte) = input.get(cursor) {
                if !byte.is_ascii_digit() {
                    break;
                }
                if !exponent_exceeds_policy {
                    // Leading zeros do not consume a digit budget. Once the
                    // magnitude exceeds the policy, keep scanning for grammar
                    // errors without growing the accumulator any further.
                    match magnitude
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(i32::from(byte - b'0')))
                    {
                        Some(value) if value <= limit => magnitude = value,
                        _ => exponent_exceeds_policy = true,
                    }
                }
                cursor += 1;
            }
            if cursor == exponent_start {
                return Err(invalid_number(cursor, "expected an exponent digit"));
            }
            // magnitude is bounded by the nonnegative i16 policy limit, so
            // negation is representable. An excessive exponent is rejected
            // after the boundary check and never appears in a returned token.
            exponent = if exponent_negative { -magnitude } else { magnitude };
        }
        spelling_end = cursor;
    }

    if let Some(&byte) = input.get(cursor) {
        if !is_number_delimiter(byte) {
            return Err(invalid_number(cursor, "invalid byte after a number"));
        }
    }
    if exponent_exceeds_policy {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            exponent_start as u32,
            "explicit exponent exceeds the policy limit",
        ));
    }
    if policy.reject_negative_zero && negative && !nonzero_significand {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            spelling_start as u32,
            "negative zero is forbidden by the numeric policy",
        ));
    }

    Ok(NumberToken {
        bytes: &input[spelling_start..spelling_end],
        span: Span32::new(offset, (cursor - start) as u32),
        kind,
        negative,
        integer_digits,
        fraction_digits,
        exponent,
    })
}

// Only called with cursors bounded by the previously validated input length.
fn invalid_number(offset: usize, detail: &'static str) -> Error {
    Error::new(ErrorCode::InvalidNumber, ErrorSource::Lexer, offset as u32, detail)
}
