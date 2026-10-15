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

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive_policy() -> NumericPolicy {
        NumericPolicy {
            allow_Quoted_int: true,
            reject_negative_zero: false,
            max_exponent_abs: i16::MAX,
            ..NumericPolicy::strict_default()
        }
    }

    fn assert_error(
        input: &[u8],
        offset: u32,
        policy: &NumericPolicy,
        code: ErrorCode,
        error_offset: u32,
    ) {
        let error = lex_number(input, offset, policy).expect_err("input must be rejected");
        assert_eq!(error.code, code, "input: {input:?}, offset: {offset}");
        assert_eq!(error.byte_offset, error_offset, "input: {input:?}");
        assert_eq!(error.source, ErrorSource::Lexer);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn accepted_spellings_preserve_all_token_metadata() {
        use NumberLexKind::{FloatDecimal, QuotedDecimal, SignedInt, UnsignedInt};
        let cases: &[(&[u8], NumberLexKind, bool, u32, u32, i32)] = &[
            (b"0", UnsignedInt, false, 1, 0, 0),
            (b"7", UnsignedInt, false, 1, 0, 0),
            (b"1234567890", UnsignedInt, false, 10, 0, 0),
            (b"-0", SignedInt, true, 1, 0, 0),
            (b"-123", SignedInt, true, 3, 0, 0),
            (b"0.0", FloatDecimal, false, 1, 1, 0),
            (b"-0.000", FloatDecimal, true, 1, 3, 0),
            (b"12.34500", FloatDecimal, false, 2, 5, 0),
            (b"1e0", FloatDecimal, false, 1, 0, 0),
            (b"1E+00308", FloatDecimal, false, 1, 0, 308),
            (b"-12.034E-005", FloatDecimal, true, 2, 3, -5),
            (b"0e-000", FloatDecimal, false, 1, 0, 0),
            (b"\"0\"", QuotedDecimal, false, 1, 0, 0),
            (b"\"-0\"", QuotedDecimal, true, 1, 0, 0),
            (b"\"9876543210\"", QuotedDecimal, false, 10, 0, 0),
            (b"\"-9876543210\"", QuotedDecimal, true, 10, 0, 0),
        ];
        for &(input, kind, negative, integer_digits, fraction_digits, exponent) in cases {
            let token = lex_number(input, 0, &permissive_policy()).unwrap();
            let quoted = kind == QuotedDecimal;
            let raw = if quoted { &input[1..input.len() - 1] } else { input };
            assert_eq!(token.raw_bytes(), raw);
            assert_eq!(token.raw_bytes().as_ptr(), raw.as_ptr());
            assert_eq!(token.span(), Span32::new(0, input.len() as u32));
            assert_eq!(token.span().slice(input), Some(input));
            assert_eq!(token.kind(), kind);
            assert_eq!(token.is_negative(), negative);
            assert_eq!(token.integer_digits(), integer_digits);
            assert_eq!(token.fraction_digits(), fraction_digits);
            assert_eq!(token.exponent(), exponent);
            assert_eq!(token.is_quoted(), quoted);
        }
    }

    #[test]
    fn integer_magnitudes_are_not_limited_by_machine_integer_ranges() {
        let cases: &[&[u8]] = &[
            b"18446744073709551615",
            b"18446744073709551616",
            b"9223372036854775808",
            b"-9223372036854775808",
            b"-9223372036854775809",
            b"999999999999999999999999999999999999999999999999999999999999999999",
            b"\"184467440737095516160000000000000000000\"",
        ];
        for &input in cases {
            let token = lex_number(input, 0, &permissive_policy()).unwrap();
            assert_eq!(token.span().len, input.len() as u32);
            assert_eq!(token.fraction_digits(), 0);
            assert_eq!(token.exponent(), 0);
        }
    }

    #[test]
    fn malformed_numbers_report_the_first_offending_byte() {
        let cases: &[(&[u8], u32)] = &[
            (b"+1", 0), (b".1", 0), (b"NaN", 0), (b"Infinity", 0),
            (b"nan", 0), (b"null", 0), (b"true", 0), (b"false", 0),
            (b"-NaN", 1), (b"-Infinity", 1), (b"--1", 1), (b"-+1", 1),
            (b"00", 1), (b"01", 1), (b"-01", 2), (b"00.1", 1),
            (b"0x10", 1), (b"0XFF", 1), (b"-0x1", 2), (b"0b1", 1),
            (b"1_000", 1), (b"1x", 1), (b"1:2", 1), (b"1/2", 1),
            (b"1[", 1), (b"1{", 1), (b"1\"", 1), (b"1+2", 1), (b"1-2", 1),
            (b"1..0", 2), (b"1.e2", 2), (b"1. 2", 2), (b"1.\t2", 2),
            (b"1.2.3", 3), (b"1e.2", 2), (b"1e 2", 2), (b"1e\n2", 2),
            (b"1e+-2", 3), (b"1e--2", 3), (b"1E++2", 3),
            (b"1e1e2", 3), (b"1e1.0", 3), (b"1e1x", 3),
            (b"- 1", 1), (b"-\t1", 1), (b" 1", 0), (b"\t1", 0),
            (b"\r1", 0), (b"\n1", 0), (b",1", 0), (b"]", 0), (b"}", 0),
            (b"\x001", 0), (b"\xff", 0), (b"1\x00", 1), (b"1\x0b", 1),
            (b"1\x0c", 1), (b"1\xc2\xa0", 1), (b"1\xff", 1),
            (b"-", 1), (b"1.", 2), (b"-0.", 3), (b"1e", 2),
            (b"1E+", 3), (b"1e-", 3), (b"-1.2e+", 6),
        ];
        for &(input, position) in cases {
            assert_error(input, 0, &permissive_policy(), ErrorCode::InvalidNumber, position);
        }
    }

    #[test]
    fn only_documented_delimiters_terminate_complete_tokens() {
        let policy = permissive_policy();
        // Exhaustively classify all 256 appended bytes, accounting for digits
        // that extend the unquoted exponent instead of terminating the token.
        for byte in 0_u8..=u8::MAX {
            let expected = matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}');
            assert_eq!(is_number_delimiter(byte), expected);
            let unquoted = [b'1', b'e', b'0', byte];
            let quoted = [b'"', b'1', b'"', byte];
            if byte.is_ascii_digit() {
                // A digit extends the unquoted exponent rather than forming a boundary.
                let token = lex_number(&unquoted, 0, &policy).unwrap();
                assert_eq!(token.raw_bytes(), &unquoted);
                assert_eq!(token.exponent(), i32::from(byte - b'0'));
            } else if expected {
                let token = lex_number(&unquoted, 0, &policy).unwrap();
                assert_eq!(token.raw_bytes(), b"1e0");
                assert_eq!(token.span(), Span32::new(0, 3));
            } else {
                assert_error(&unquoted, 0, &policy, ErrorCode::InvalidNumber, 3);
            }
            if expected {
                let token = lex_number(&quoted, 0, &policy).unwrap();
                assert_eq!(token.raw_bytes(), b"1");
                assert_eq!(token.span(), Span32::new(0, 3));
            } else {
                assert_error(&quoted, 0, &policy, ErrorCode::InvalidNumber, 3);
            }
        }
    }

    #[test]
    fn delimiters_do_not_require_valid_surrounding_json() {
        let cases: &[&[u8]] = &[b"1 2", b"1,garbage", b"1]x", b"1}x", b"1\n\xff"];
        for &input in cases {
            let token = lex_number(input, 0, &NumericPolicy::strict_default()).unwrap();
            assert_eq!(token.raw_bytes(), b"1");
            assert_eq!(token.span(), Span32::new(0, 1));
        }
    }

    #[test]
    fn quoted_form_accepts_only_strict_integers() {
        let cases: &[(&[u8], u32)] = &[
            (b"\"\"", 1), (b"\"-\"", 2), (b"\"+1\"", 1), (b"\"01\"", 2),
            (b"\"-00\"", 3), (b"\"1.0\"", 2), (b"\"1e0\"", 2),
            (b"\"1E0\"", 2), (b"\"1 2\"", 2), (b"\" 1\"", 1),
            (b"\"1\t\"", 2), (b"\"1\n\"", 2), (b"\"1\\\"", 2),
            (b"\"\\u0031\"", 1), (b"\"1_0\"", 2), (b"\"NaN\"", 1),
            (b"\"Infinity\"", 1), (b"\"0x1\"", 2), (b"\"1\xff\"", 2),
            (b"\"1\"x", 3), (b"\"1\"0", 3), (b"\"1\"\"", 3),
            (b"\"", 1), (b"\"-", 2), (b"\"01", 2),
            (b"\"1.", 2), (b"\"1e", 2), (b"\"1 ", 2),
        ];
        for &(input, position) in cases {
            assert_error(input, 0, &permissive_policy(), ErrorCode::InvalidNumber, position);
        }
        for input in [b"\"0".as_slice(), b"\"-0", b"\"12345", b"\"-12345"] {
            assert_error(
                input, 0, &permissive_policy(), ErrorCode::UnexpectedEof, input.len() as u32,
            );
        }
        for input in [b"\"1\"".as_slice(), b"\"", b"\"bad\"", b"\"-0\""] {
            assert_error(input, 0, &NumericPolicy::strict_default(), ErrorCode::InvalidNumber, 0);
        }
    }

    #[test]
    fn offsets_spans_and_diagnostics_are_absolute() {
        let input = b"prefix:-12.034E-005,tail";
        let original = *input;
        let policy = permissive_policy();
        let saved_policy = policy;
        let token = lex_number(input, 7, &policy).unwrap();
        assert_eq!(token.raw_bytes(), &input[7..19]);
        assert_eq!(token.raw_bytes().as_ptr(), input[7..].as_ptr());
        assert_eq!(token.span(), Span32::new(7, 12));
        assert_eq!(token.span().end(), Some(19));
        assert_eq!(token.integer_digits(), 2);
        assert_eq!(token.fraction_digits(), 3);
        assert_eq!(token.exponent(), -5);
        assert_eq!(*input, original);
        assert_eq!(policy, saved_policy);

        let input = b"x\"-12\"]";
        let token = lex_number(input, 1, &policy).unwrap();
        assert_eq!(token.raw_bytes(), &input[2..5]);
        assert_eq!(token.raw_bytes().as_ptr(), input[2..].as_ptr());
        assert_eq!(token.span(), Span32::new(1, 5));
        assert_eq!(token.span().slice(input), Some(&input[1..6]));
        assert_error(b"xxx01", 3, &policy, ErrorCode::InvalidNumber, 4);
        assert_error(b"xxx1e+", 3, &policy, ErrorCode::InvalidNumber, 6);
        assert_error(b"xxx\"12", 3, &policy, ErrorCode::UnexpectedEof, 6);
        assert_error(b"xxx\"12\"x", 3, &policy, ErrorCode::InvalidNumber, 7);
        assert_error(b"xxx\"1\"", 3, &NumericPolicy::strict_default(), ErrorCode::InvalidNumber, 3);
        // The caller selects the exact start; preceding bytes are not validated.
        let token = lex_number(b"01", 1, &policy).unwrap();
        assert_eq!(token.raw_bytes(), b"1");
        assert_eq!(token.span(), Span32::new(1, 1));
    }

    #[test]
    fn empty_or_out_of_range_start_is_unexpected_eof() {
        let policy = NumericPolicy::strict_default();
        for (input, offset) in [
            (b"".as_slice(), 0), (b"", 1), (b"", u32::MAX),
            (b"1", 1), (b"1", 2), (b"1", u32::MAX),
        ] {
            assert_error(input, offset, &policy, ErrorCode::UnexpectedEof, offset);
        }
    }

    #[test]
    fn invalid_caller_policy_precedes_eof_and_grammar_errors() {
        for limit in [-1, i16::MIN] {
            let policy = NumericPolicy { max_exponent_abs: limit, ..permissive_policy() };
            for (input, offset) in [
                (b"".as_slice(), 0), (b"", u32::MAX), (b"1", 0),
                (b"not a number", 4), (b"\"1\"", 0), (b"1", 2),
            ] {
                assert_error(input, offset, &policy, ErrorCode::ConstraintViolation, offset);
            }
        }
    }

    #[test]
    fn exponent_limits_are_inclusive_and_sign_independent() {
        let policy = NumericPolicy::strict_default();
        for (input, exponent) in [
            (b"1e308".as_slice(), 308), (b"1e+308", 308), (b"1E-308", -308),
            (b"1e000308", 308), (b"1E-000308", -308), (b"0.001e308", 308),
        ] {
            assert_eq!(lex_number(input, 0, &policy).unwrap().exponent(), exponent);
        }
        for (input, position) in [
            (b"1e309".as_slice(), 2), (b"1e+309", 3), (b"1E-309", 3),
            (b"1e000309", 2), (b"1E-000309", 3),
            (b"1e9999999999999999999999999999999999999999", 2),
            (b"prefix1E-9999999999999999999999", 9),
        ] {
            let offset = if input.starts_with(b"prefix") { 6 } else { 0 };
            assert_error(input, offset, &policy, ErrorCode::ConstraintViolation, position);
        }
        let policy = permissive_policy();
        for (input, exponent) in [(b"1e32767".as_slice(), 32767), (b"1e-32767", -32767)] {
            assert_eq!(lex_number(input, 0, &policy).unwrap().exponent(), exponent);
        }
        assert_error(b"1e32768", 0, &policy, ErrorCode::ConstraintViolation, 2);
        assert_error(b"1e-32768", 0, &policy, ErrorCode::ConstraintViolation, 3);
        assert_error(b"1e2147483648", 0, &policy, ErrorCode::ConstraintViolation, 2);
        assert_error(b"1e4294967296", 0, &policy, ErrorCode::ConstraintViolation, 2);
    }

    #[test]
    fn zero_exponent_limit_allows_any_number_of_leading_zeros() {
        let policy = NumericPolicy { max_exponent_abs: 0, ..permissive_policy() };
        for input in [
            b"1".as_slice(), b"1.123", b"1e0", b"1e+000000", b"1E-000000", b"-0e0",
        ] {
            assert_eq!(lex_number(input, 0, &policy).unwrap().exponent(), 0);
        }
        for (input, position) in [
            (b"1e1".as_slice(), 2), (b"1e+0001", 3), (b"1e-0001", 3),
        ] {
            assert_error(input, 0, &policy, ErrorCode::ConstraintViolation, position);
        }
        let mut input = [b'0'; 8192];
        input[..3].copy_from_slice(b"1e-");
        let token = lex_number(&input, 0, &policy).unwrap();
        assert_eq!(token.raw_bytes(), &input);
        assert_eq!(token.exponent(), 0);
        assert_eq!(token.integer_digits(), 1);
        input[8191] = b'1';
        assert_error(&input, 0, &policy, ErrorCode::ConstraintViolation, 3);
        let token = lex_number(&input, 0, &permissive_policy()).unwrap();
        assert_eq!(token.exponent(), -1);
    }

    #[test]
    fn negative_zero_policy_uses_only_significand_digits() {
        let policy = NumericPolicy { allow_Quoted_int: true, ..NumericPolicy::strict_default() };
        for input in [
            b"-0".as_slice(), b"-0.0", b"-0.00000", b"-0e0", b"-0E+308", b"-0.000e-308",
        ] {
            assert_error(input, 0, &policy, ErrorCode::ConstraintViolation, 0);
            assert!(lex_number(input, 0, &permissive_policy()).unwrap().is_negative());
        }
        assert_error(b"\"-0\"", 0, &policy, ErrorCode::ConstraintViolation, 1);
        assert_error(b"abc\"-0\"", 3, &policy, ErrorCode::ConstraintViolation, 4);
        assert_error(b"abc-0.000e1", 3, &policy, ErrorCode::ConstraintViolation, 3);
        for input in [
            b"0".as_slice(), b"0.0", b"0e308", b"\"0\"", b"-1", b"-10.000",
            b"-0.00001", b"-0.01e-308", b"\"-1\"",
        ] {
            assert!(lex_number(input, 0, &policy).is_ok(), "input: {input:?}");
        }
    }

    #[test]
    fn grammar_and_boundaries_precede_value_policy_failures() {
        let policy = NumericPolicy { allow_Quoted_int: true, ..NumericPolicy::strict_default() };
        for (input, position) in [
            (b"-00".as_slice(), 2), (b"-0x", 2), (b"-0.", 3),
            (b"-0e", 3), (b"-0e+", 4), (b"-0.0:", 4), (b"-0e999x", 6),
            (b"1e999x", 5), (b"1e999.", 5), (b"1e999+", 5),
            (b"1e999\"", 5), (b"1e999/", 5), (b"1e999\xff", 5),
            (b"\"-0.0\"", 3), (b"\"-0\"x", 4),
        ] {
            assert_error(input, 0, &policy, ErrorCode::InvalidNumber, position);
        }
        let input = b"1e999999999999999999999999x";
        assert_error(input, 0, &policy, ErrorCode::InvalidNumber, (input.len() - 1) as u32);
        assert_error(b"\"-0", 0, &policy, ErrorCode::UnexpectedEof, 3);
        assert_error(b"-0e999", 0, &policy, ErrorCode::ConstraintViolation, 3);
        assert_error(b"1e999,invalid", 0, &policy, ErrorCode::ConstraintViolation, 2);
    }

    #[test]
    fn long_significands_have_exact_counts_without_a_fixed_digit_cap() {
        let mut input = [b'9'; 8192];
        let token = lex_number(&input, 0, &NumericPolicy::strict_default()).unwrap();
        assert_eq!(token.integer_digits(), 8192);
        assert_eq!(token.raw_bytes(), &input);
        input[..3].copy_from_slice(b"-0.");
        input[3..].fill(b'0');
        input[8191] = b'1';
        let token = lex_number(&input, 0, &NumericPolicy::strict_default()).unwrap();
        assert_eq!(token.integer_digits(), 1);
        assert_eq!(token.fraction_digits(), 8189);
        assert_eq!(token.kind(), NumberLexKind::FloatDecimal);
        assert!(token.is_negative());
        input[8191] = b'0';
        assert_error(&input, 0, &NumericPolicy::strict_default(), ErrorCode::ConstraintViolation, 0);
    }

    #[test]
    fn float_conversion_policy_does_not_impose_a_lexical_magnitude_limit() {
        let input = b"999999999999999999999999999999999999999999999999999999999999999e308";
        let strict = NumericPolicy::strict_default();
        let allow_non_finite = NumericPolicy { reject_non_finite_float: false, ..strict };
        assert_eq!(lex_number(input, 0, &strict), lex_number(input, 0, &allow_non_finite));
        assert!(lex_number(input, 0, &strict).is_ok());
    }

    #[test]
    fn failure_does_not_mutate_input_or_policy() {
        let input = *b"-0e999999999999999999999999x";
        let original = input;
        let policy = NumericPolicy::strict_default();
        let original_policy = policy;
        assert_error(&input, 0, &policy, ErrorCode::InvalidNumber, (input.len() - 1) as u32);
        assert_eq!(input, original);
        assert_eq!(policy, original_policy);
    }
}
