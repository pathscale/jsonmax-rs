//! Allocation-free validation of complete quoted decimal tokens.

use super::*;

/// Validate one complete quoted decimal and borrow its digit ranges.
///
/// The integer part is mandatory and cannot have redundant leading zeros.
/// Fractions and enabled exponents require at least one digit. String escapes,
/// whitespace, non-ASCII digits, and bytes after the closing quote are rejected.
/// A lexical minus is preserved even when all coefficient digits are zero.
///
/// Errors have source [`ErrorSource::Lexer`]. Incomplete valid prefixes report
/// [`ErrorCode::UnexpectedEof`] at the absolute end; invalid bytes and exponent
/// overflow report [`ErrorCode::InvalidNumber`] at the first offending byte.
/// Unrepresentable lengths or absolute ends report
/// [`ErrorCode::PayloadTooLarge`] at `offset` before inspecting the grammar.
pub fn lex_quoted<'a>(
    input: &'a [u8],
    offset: u32,
    allow_exponent: bool,
) -> Result<DecimalLexeme<'a>> {
    let span = checked_span(input.len(), offset)?;

    // Every position below is at most input.len(). The validated span proves
    // that both this narrowing conversion and the absolute addition are safe.
    let error = |code: ErrorCode, at: usize, detail: &'static str| {
        Error::new(code, ErrorSource::Lexer, offset + at as u32, detail)
    };
    let required_byte = |at: usize| {
        input.get(at).copied().ok_or_else(|| {
            error(ErrorCode::UnexpectedEof, at, "incomplete quoted decimal")
        })
    };

    if required_byte(0)? != b'"' {
        return Err(error(ErrorCode::InvalidNumber, 0, "expected opening quote"));
    }
    let mut cursor = 1;
    let negative = input.get(cursor).copied() == Some(b'-');
    if negative {
        cursor += 1;
    }

    let integer_start = cursor;
    match required_byte(cursor)? {
        b'0' => {
            cursor += 1;
            if matches!(input.get(cursor).copied(), Some(b'0'..=b'9')) {
                return Err(error(
                    ErrorCode::InvalidNumber,
                    cursor,
                    "decimal integer has a leading zero",
                ));
            }
        }
        b'1'..=b'9' => {
            cursor += 1;
            while matches!(input.get(cursor).copied(), Some(b'0'..=b'9')) {
                cursor += 1;
            }
        }
        _ => {
            return Err(error(
                ErrorCode::InvalidNumber,
                cursor,
                "expected decimal integer digit",
            ));
        }
    }
    let integer_end = cursor;

    let mut fraction_start = cursor;
    let mut fraction_end = cursor;
    if input.get(cursor).copied() == Some(b'.') {
        cursor += 1;
        fraction_start = cursor;
        if !required_byte(cursor)?.is_ascii_digit() {
            return Err(error(
                ErrorCode::InvalidNumber,
                cursor,
                "expected decimal fractional digit",
            ));
        }
        cursor += 1;
        while matches!(input.get(cursor).copied(), Some(b'0'..=b'9')) {
            cursor += 1;
        }
        fraction_end = cursor;
    }

    let mut exponent = 0_i32;
    if matches!(input.get(cursor).copied(), Some(b'e' | b'E')) {
        if !allow_exponent {
            return Err(error(
                ErrorCode::InvalidNumber,
                cursor,
                "decimal exponent is disabled",
            ));
        }
        cursor += 1;
        let exponent_negative = input.get(cursor).copied() == Some(b'-');
        if matches!(input.get(cursor).copied(), Some(b'+' | b'-')) {
            cursor += 1;
        }
        if !required_byte(cursor)?.is_ascii_digit() {
            return Err(error(
                ErrorCode::InvalidNumber,
                cursor,
                "expected decimal exponent digit",
            ));
        }
        // The magnitude stays bounded throughout accumulation. Leading zeros
        // therefore impose no separate spelling-length limit.
        while let Some(digit @ b'0'..=b'9') = input.get(cursor).copied() {
            exponent = exponent
                .checked_mul(10)
                .and_then(|value| value.checked_add(i32::from(digit - b'0')))
                .filter(|value| *value <= MAX_EXPONENT_ABS)
                .ok_or_else(|| {
                    error(
                        ErrorCode::InvalidNumber,
                        cursor,
                        "decimal exponent exceeds its absolute limit",
                    )
                })?;
            cursor += 1;
        }
        if exponent_negative {
            // The magnitude is at most MAX_EXPONENT_ABS, including zero.
            exponent = -exponent;
        }
    }

    if required_byte(cursor)? != b'"' {
        return Err(error(
            ErrorCode::InvalidNumber,
            cursor,
            "expected closing quote",
        ));
    }
    cursor += 1;
    if cursor != input.len() {
        return Err(error(
            ErrorCode::InvalidNumber,
            cursor,
            "bytes follow quoted decimal",
        ));
    }

    Ok(DecimalLexeme {
        raw: input,
        span,
        negative,
        integer: &input[integer_start..integer_end],
        fraction: &input[fraction_start..fraction_end],
        exponent,
    })
}

// Keeping range validation separate also lets tests cover oversized lengths
// without allocating a multi-gigabyte input or constructing an invalid slice.
fn checked_span(len: usize, offset: u32) -> Result<Span32> {
    let range_error = || {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Lexer,
            offset,
            "quoted decimal range exceeds 32-bit offsets",
        )
    };
    let len = u32::try_from(len).map_err(|_| range_error())?;
    offset.checked_add(len).ok_or_else(range_error)?;
    Ok(Span32::new(offset, len))
}
