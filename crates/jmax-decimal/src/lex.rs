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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error(
        input: &[u8],
        offset: u32,
        allow_exponent: bool,
        code: ErrorCode,
        at: u32,
    ) {
        let error = lex_quoted(input, offset, allow_exponent).unwrap_err();
        assert_eq!(error.code, code, "input: {:?}", input);
        assert_eq!(error.source, ErrorSource::Lexer, "input: {:?}", input);
        assert_eq!(error.byte_offset, at, "input: {:?}", input);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn fixed_point_vectors_preserve_digits_sign_and_span() {
        let vectors: &[(&[u8], bool, &[u8], &[u8])] = &[
            (b"\"0\"", false, b"0", b""),
            (b"\"-0\"", true, b"0", b""),
            (b"\"1\"", false, b"1", b""),
            (b"\"9876543210\"", false, b"9876543210", b""),
            (b"\"0.00\"", false, b"0", b"00"),
            (b"\"-0.000\"", true, b"0", b"000"),
            (b"\"123.004500\"", false, b"123", b"004500"),
            (b"\"-123.004500\"", true, b"123", b"004500"),
            (
                b"\"170141183460469231731687303715884105728\"",
                false,
                b"170141183460469231731687303715884105728",
                b"",
            ),
        ];
        for &(input, negative, integer, fraction) in vectors {
            for allow_exponent in [false, true] {
                let lexeme = lex_quoted(input, 71, allow_exponent).unwrap();
                assert_eq!(lexeme.raw(), input);
                assert_eq!(lexeme.span(), Span32::new(71, input.len() as u32));
                assert_eq!(lexeme.is_negative(), negative);
                assert_eq!(lexeme.integer_digits(), integer);
                assert_eq!(lexeme.fractional_digits(), fraction);
                assert_eq!(lexeme.exponent(), 0);
            }
        }
    }

    #[test]
    fn exponent_vectors_include_limits_signs_and_leading_zeros() {
        let vectors: &[(&[u8], i32)] = &[
            (b"\"1e0\"", 0),
            (b"\"1E+0\"", 0),
            (b"\"-0e-0000\"", 0),
            (b"\"1e00009\"", 9),
            (b"\"-12.30E+0042\"", 42),
            (b"\"1e-0042\"", -42),
            (b"\"1e32767\"", MAX_EXPONENT_ABS),
            (b"\"1E+32767\"", MAX_EXPONENT_ABS),
            (b"\"1e-32767\"", -MAX_EXPONENT_ABS),
            (b"\"1e0000032767\"", MAX_EXPONENT_ABS),
        ];
        for &(input, exponent) in vectors {
            let lexeme = lex_quoted(input, 19, true).unwrap();
            assert_eq!(lexeme.raw(), input);
            assert_eq!(lexeme.exponent(), exponent);
            assert_eq!(lexeme.span().end(), Some(19 + input.len() as u32));
        }
        let lexeme = lex_quoted(b"\"-12.30E+0042\"", 0, true).unwrap();
        assert!(lexeme.is_negative());
        assert_eq!(lexeme.integer_digits(), b"12");
        assert_eq!(lexeme.fractional_digits(), b"30");
    }

    #[test]
    fn slices_borrow_the_original_input() {
        let input = *b"\"-123.0045e-2\"";
        let lexeme = lex_quoted(&input, 8, true).unwrap();
        assert_eq!(lexeme.raw().as_ptr(), input.as_ptr());
        assert_eq!(lexeme.integer_digits().as_ptr(), input[2..].as_ptr());
        assert_eq!(lexeme.fractional_digits().as_ptr(), input[6..].as_ptr());
        assert_eq!(lexeme.integer_digits(), b"123");
        assert_eq!(lexeme.fractional_digits(), b"0045");
        assert_eq!(lexeme.exponent(), -2);
        let integer_only = lex_quoted(b"\"1e2\"", 0, true).unwrap();
        assert!(integer_only.fractional_digits().is_empty());
    }

    #[test]
    fn every_proper_prefix_of_valid_tokens_reports_eof_at_end() {
        let tokens: &[&[u8]] = &[
            b"\"0\"",
            b"\"-0.001\"",
            b"\"123456789.0123456789\"",
            b"\"12e32767\"",
            b"\"-1.2E-0032767\"",
            b"\"1e+00000\"",
        ];
        for &token in tokens {
            for end in 0..token.len() {
                assert_error(
                    &token[..end],
                    101,
                    true,
                    ErrorCode::UnexpectedEof,
                    101 + end as u32,
                );
            }
        }
        for end in 0..b"\"-12.34\"".len() {
            assert_error(
                &b"\"-12.34\""[..end],
                7,
                false,
                ErrorCode::UnexpectedEof,
                7 + end as u32,
            );
        }
    }

    #[test]
    fn malformed_tokens_report_the_first_offending_byte() {
        let vectors: &[(&[u8], u32)] = &[
            (b"0", 0),
            (b"'1'", 0),
            (b" \"1\"", 0),
            (b"\"\"", 1),
            (b"\"-\"", 2),
            (b"\"+1\"", 1),
            (b"\"--1\"", 2),
            (b"\".1\"", 1),
            (b"\"-.1\"", 2),
            (b"\"00\"", 2),
            (b"\"01\"", 2),
            (b"\"-01\"", 3),
            (b"\"01", 2),
            (b"\"1.\"", 3),
            (b"\"1.e2\"", 3),
            (b"\"1..2\"", 3),
            (b"\"1.2.3\"", 4),
            (b"\"1e\"", 3),
            (b"\"1e+\"", 4),
            (b"\"1e-\"", 4),
            (b"\"1e+-2\"", 4),
            (b"\"1e--2\"", 4),
            (b"\"1ee2\"", 3),
            (b"\"1e2e3\"", 4),
            (b"\"1e2.0\"", 4),
            (b"\"1_0\"", 2),
            (b"\"1,0\"", 2),
            (b"\"1+2\"", 2),
            (b"\"NaN\"", 1),
            (b"\"Infinity\"", 1),
            (b"\" 1\"", 1),
            (b"\"1 \"", 2),
            (b"\"1\t\"", 2),
            (b"\"1\n\"", 2),
            (b"\"1\0\"", 2),
            (b"\"1\\u0030\"", 2),
            (b"\"\\u0031\"", 1),
            (b"\"1\\\"\"", 2),
            (b"\"1\" ", 3),
            (b"\"1\"\"", 3),
            (b"\"1\"\"2\"", 3),
            (b"\"1\"\0", 3),
            (b"\"\xff\"", 1),
            ("\"\u{0661}\"".as_bytes(), 1),
            ("\"1.\u{ff11}\"".as_bytes(), 3),
            ("\"1e\u{0661}\"".as_bytes(), 3),
        ];
        for &(input, at) in vectors {
            assert_error(input, 503, true, ErrorCode::InvalidNumber, 503 + at);
        }
    }

    #[test]
    fn disabled_exponents_fail_at_the_marker_even_when_incomplete_or_zero() {
        let vectors: &[(&[u8], u32)] = &[
            (b"\"1e", 2),
            (b"\"1E+", 2),
            (b"\"1e0\"", 2),
            (b"\"1e-0\"", 2),
            (b"\"-0.0E+000\"", 5),
        ];
        for &(input, at) in vectors {
            assert_error(input, 31, false, ErrorCode::InvalidNumber, 31 + at);
        }
    }

    #[test]
    fn excessive_exponents_fail_on_the_first_digit_above_the_limit() {
        let vectors: &[(&[u8], u32)] = &[
            (b"\"1e32768\"", 7),
            (b"\"1e-32768\"", 8),
            (b"\"1e+32768\"", 8),
            (b"\"1e327670\"", 8),
            (b"\"1e9999999999999999999999999999999\"", 7),
            (b"\"1e00032768\"", 10),
            (b"\"1e32768", 7),
            (b"\"1e32768x\"", 7),
        ];
        for &(input, at) in vectors {
            assert_error(input, 211, true, ErrorCode::InvalidNumber, 211 + at);
        }
    }

    #[test]
    fn long_exponent_zero_prefixes_have_no_artificial_length_limit() {
        let mut input = [b'0'; 4096];
        input[0] = b'"';
        input[1] = b'1';
        input[2] = b'e';
        input[3] = b'-';
        input[4095] = b'"';
        assert_eq!(lex_quoted(&input, 0, true).unwrap().exponent(), 0);
        input[4090..4095].copy_from_slice(b"32767");
        assert_eq!(
            lex_quoted(&input, 0, true).unwrap().exponent(),
            -MAX_EXPONENT_ABS,
        );
        input[4094] = b'8';
        assert_error(&input, 0, true, ErrorCode::InvalidNumber, 4094);
    }

    #[test]
    fn lexing_does_not_impose_coefficient_or_writer_capacity_limits() {
        let mut input = [b'9'; 512];
        input[0] = b'"';
        input[511] = b'"';
        let lexeme = lex_quoted(&input, 0, false).unwrap();
        assert_eq!(lexeme.integer_digits(), &input[1..511]);
        assert!(lexeme.fractional_digits().is_empty());
        input[1] = b'0';
        input[2] = b'.';
        let lexeme = lex_quoted(&input, 0, false).unwrap();
        assert_eq!(lexeme.integer_digits(), b"0");
        assert_eq!(lexeme.fractional_digits(), &input[3..511]);
    }

    #[test]
    fn absolute_range_boundaries_are_validated_before_the_grammar() {
        let lexeme = lex_quoted(b"\"0\"", u32::MAX - 3, false).unwrap();
        assert_eq!(lexeme.span(), Span32::new(u32::MAX - 3, 3));
        assert_eq!(lexeme.span().end(), Some(u32::MAX));
        assert_error(
            b"\"0\"",
            u32::MAX - 2,
            false,
            ErrorCode::PayloadTooLarge,
            u32::MAX - 2,
        );
        assert_error(b"x", u32::MAX, true, ErrorCode::PayloadTooLarge, u32::MAX);
        assert_error(b"", u32::MAX, false, ErrorCode::UnexpectedEof, u32::MAX);
        assert_error(
            b"\"0",
            u32::MAX - 2,
            false,
            ErrorCode::UnexpectedEof,
            u32::MAX,
        );
        assert_error(
            b"\"x\"",
            u32::MAX - 3,
            false,
            ErrorCode::InvalidNumber,
            u32::MAX - 2,
        );
        assert_eq!(
            checked_span(0, u32::MAX).unwrap(),
            Span32::new(u32::MAX, 0),
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn oversized_lengths_are_rejected_without_constructing_a_slice() {
        let maximum = u32::MAX as usize;
        assert_eq!(
            checked_span(maximum, 0).unwrap(),
            Span32::new(0, u32::MAX),
        );
        for (len, offset) in [(maximum, 1), (maximum + 1, 0), (usize::MAX, 17)] {
            let error = checked_span(len, offset).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            assert_eq!(error.source, ErrorSource::Lexer);
            assert_eq!(error.byte_offset, offset);
        }
    }
}
