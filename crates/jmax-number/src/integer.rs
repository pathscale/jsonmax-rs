//! Exact, allocation-free integer conversion with checked eight-digit batches.

use super::*;

/// Convert an integer token to an unsigned 64-bit value.
///
/// Every negative token, including an allowed lexical `-0`, overflows. Decimal
/// and exponent spellings are rejected even when their value is integral.
/// Quoted integers retain the policy already established by the lexer.
/// Errors use the token's full source-span start and [`ErrorSource::Lexer`].
pub fn parse_u64(token: &NumberToken<'_>) -> Result<u64> {
    require_integer(token)?;
    if token.is_negative() {
        return Err(overflow(token));
    }
    magnitude(token, u64::MAX)
}

/// Convert an integer token to a signed 64-bit value, including `i64::MIN`.
///
/// Negative zero becomes zero if the lexer admitted it. Float spellings are
/// rejected, and every out-of-range integer reports [`ErrorCode::NumberOverflow`]
/// at the token's full source-span start with [`ErrorSource::Lexer`].
pub fn parse_i64(token: &NumberToken<'_>) -> Result<i64> {
    require_integer(token)?;
    let negative = token.is_negative();
    const MIN_MAGNITUDE: u64 = 1_u64 << 63;
    let limit = if negative { MIN_MAGNITUDE } else { i64::MAX as u64 };
    let value = magnitude(token, limit)?;
    if negative && value == MIN_MAGNITUDE {
        return Ok(i64::MIN);
    }
    let value = i64::try_from(value).map_err(|_| overflow(token))?;
    // The minimum magnitude was handled above. The remaining value lies in
    // 0..=i64::MAX, so its signed negation is representable, including zero.
    Ok(if negative { -value } else { value })
}

/// Convert by lexical sign: negative integers become `I64`, others become `U64`.
///
/// Wrapper quotes do not affect the choice. Range failures never fall back to
/// floating-point conversion, and no new lexical policy is imposed.
pub fn convert_integer(token: &NumberToken<'_>) -> Result<ConvertedNumber> {
    if token.is_negative() {
        parse_i64(token).map(ConvertedNumber::I64)
    } else {
        parse_u64(token).map(ConvertedNumber::U64)
    }
}

fn require_integer(token: &NumberToken<'_>) -> Result<()> {
    match token.kind() {
        NumberLexKind::UnsignedInt | NumberLexKind::SignedInt | NumberLexKind::QuotedDecimal => {
            Ok(())
        }
        NumberLexKind::FloatDecimal => Err(Error::new(
            ErrorCode::InvalidNumber,
            ErrorSource::Lexer,
            token.span().offset,
            "integer conversion requires an integer spelling",
        )),
    }
}

fn overflow(token: &NumberToken<'_>) -> Error {
    Error::new(
        ErrorCode::NumberOverflow,
        ErrorSource::Lexer,
        token.span().offset,
        "integer is outside the destination range",
    )
}

// Called only after require_integer. Lex constructs all tokens, so the raw
// spelling is a nonempty ASCII digit sequence with an optional leading minus;
// wrapper quotes are already excluded. No grammar or policy is reinterpreted.
fn magnitude(token: &NumberToken<'_>, limit: u64) -> Result<u64> {
    let raw = token.raw_bytes();
    let digits = if token.is_negative() { &raw[1..] } else { raw };
    let mut chunks = digits.chunks_exact(8);
    let mut value = 0_u64;
    for chunk in &mut chunks {
        // chunks_exact establishes the bound before copying. The explicit
        // little-endian load neither requires alignment nor depends on the host.
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(chunk);
        value = value
            .checked_mul(100_000_000)
            .and_then(|prefix| prefix.checked_add(eight_digits(bytes)))
            .filter(|&next| next <= limit)
            .ok_or_else(|| overflow(token))?;
    }
    for &byte in chunks.remainder() {
        value = value
            .checked_mul(10)
            .and_then(|prefix| prefix.checked_add(u64::from(byte - b'0')))
            .filter(|&next| next <= limit)
            .ok_or_else(|| overflow(token))?;
    }
    Ok(value)
}

// Reduce eight known ASCII digits in parallel, without wrapping arithmetic.
// Subtraction cannot borrow between byte lanes because each byte is >= '0'.
// Pair lanes are <= 99 in 16 bits; quad lanes are <= 9999 in 32 bits. The
// masks leave enough space for each multiplication and addition, including in
// the highest lane. The final scalar value is <= 99_999_999 and fits in u64.
fn eight_digits(bytes: [u8; 8]) -> u64 {
    let digits = u64::from_le_bytes(bytes) - 0x3030_3030_3030_3030;
    const BYTE_LANES: u64 = 0x00ff_00ff_00ff_00ff;
    const PAIR_LANES: u64 = 0x0000_ffff_0000_ffff;
    let pairs = (digits & BYTE_LANES) * 10 + ((digits >> 8) & BYTE_LANES);
    let quads = (pairs & PAIR_LANES) * 100 + ((pairs >> 16) & PAIR_LANES);
    (quads & 0xffff_ffff) * 10_000 + (quads >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex_number;
    use core::fmt::Write;

    fn policy() -> NumericPolicy {
        NumericPolicy {
            allow_Quoted_int: true,
            reject_negative_zero: false,
            ..NumericPolicy::strict_default()
        }
    }

    fn token(input: &[u8]) -> NumberToken<'_> {
        lex_number(input, 0, &policy()).expect("test spelling must lex")
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode, offset: u32) {
        let error = result.expect_err("conversion must fail");
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Lexer);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn unsigned_vectors_cover_chunk_boundaries_and_full_range() {
        let cases: &[(&[u8], u64)] = &[
            (b"0", 0),
            (b"1", 1),
            (b"9", 9),
            (b"10", 10),
            (b"9999999", 9_999_999),
            (b"12345678", 12_345_678),
            (b"99999999", 99_999_999),
            (b"100000000", 100_000_000),
            (b"123456789", 123_456_789),
            (b"999999999999999", 999_999_999_999_999),
            (b"1234567890123456", 1_234_567_890_123_456),
            (b"9999999999999999", 9_999_999_999_999_999),
            (b"10000000000000000", 10_000_000_000_000_000),
            (b"12345678901234567", 12_345_678_901_234_567),
            (b"9223372036854775807", 9_223_372_036_854_775_807),
            (b"9223372036854775808", 9_223_372_036_854_775_808),
            (b"10000000000000000000", 10_000_000_000_000_000_000),
            (b"18446744073709551614", u64::MAX - 1),
            (b"18446744073709551615", u64::MAX),
        ];
        for &(input, expected) in cases {
            let token = token(input);
            assert_eq!(parse_u64(&token), Ok(expected), "input: {input:?}");
            assert_eq!(convert_integer(&token), Ok(ConvertedNumber::U64(expected)));
        }
    }

    #[test]
    fn signed_vectors_include_both_endpoints_and_negative_zero() {
        let cases: &[(&[u8], i64)] = &[
            (b"0", 0),
            (b"-0", 0),
            (b"1", 1),
            (b"-1", -1),
            (b"-9999999", -9_999_999),
            (b"-12345678", -12_345_678),
            (b"-100000000", -100_000_000),
            (b"-1234567890123456", -1_234_567_890_123_456),
            (b"-10000000000000000", -10_000_000_000_000_000),
            (b"9223372036854775806", i64::MAX - 1),
            (b"9223372036854775807", i64::MAX),
            (b"-9223372036854775807", i64::MIN + 1),
            (b"-9223372036854775808", i64::MIN),
        ];
        for &(input, expected) in cases {
            let token = token(input);
            assert_eq!(parse_i64(&token), Ok(expected), "input: {input:?}");
            if token.is_negative() {
                assert_eq!(convert_integer(&token), Ok(ConvertedNumber::I64(expected)));
                assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 0);
            } else {
                assert_eq!(convert_integer(&token), Ok(ConvertedNumber::U64(expected as u64)));
            }
        }
    }

    #[test]
    fn unsigned_overflow_covers_add_multiply_and_whole_chunk_failures() {
        for input in [
            b"18446744073709551616".as_slice(),
            b"18446744073709551619",
            b"18446744073709551620",
            b"99999999999999999999",
            b"100000000000000000000",
            b"184467440737095516150",
            b"100000000000000000000000",
            b"999999999999999999999999",
            b"1844674407370955161600000000000000",
        ] {
            let token = token(input);
            assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 0);
            assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 0);
            assert_error(convert_integer(&token), ErrorCode::NumberOverflow, 0);
        }
    }

    #[test]
    fn signed_overflow_never_changes_variant_or_rounds_to_float() {
        for input in [
            b"9223372036854775808".as_slice(),
            b"9223372036854775809",
            b"9999999999999999999",
            b"18446744073709551615",
        ] {
            let token = token(input);
            assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 0);
            let unsigned = parse_u64(&token).unwrap();
            assert_eq!(convert_integer(&token), Ok(ConvertedNumber::U64(unsigned)));
        }
        for input in [
            b"-9223372036854775809".as_slice(),
            b"-9999999999999999999",
            b"-18446744073709551615",
            b"-18446744073709551616",
            b"-100000000000000000000000",
        ] {
            let token = token(input);
            assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 0);
            assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 0);
            assert_error(convert_integer(&token), ErrorCode::NumberOverflow, 0);
        }
    }

    #[test]
    fn quoted_integers_follow_sign_and_preserve_lexical_permission() {
        let cases: &[(&[u8], ConvertedNumber)] = &[
            (b"\"0\"", ConvertedNumber::U64(0)),
            (b"\"-0\"", ConvertedNumber::I64(0)),
            (b"\"12345678\"", ConvertedNumber::U64(12_345_678)),
            (b"\"-12345678\"", ConvertedNumber::I64(-12_345_678)),
            (b"\"9223372036854775807\"", ConvertedNumber::U64(i64::MAX as u64)),
            (b"\"9223372036854775808\"", ConvertedNumber::U64(1_u64 << 63)),
            (b"\"18446744073709551615\"", ConvertedNumber::U64(u64::MAX)),
            (b"\"-9223372036854775808\"", ConvertedNumber::I64(i64::MIN)),
        ];
        for &(input, expected) in cases {
            let token = token(input);
            assert_eq!(token.kind(), NumberLexKind::QuotedDecimal);
            assert_eq!(convert_integer(&token), Ok(expected));
            match expected {
                ConvertedNumber::U64(value) => {
                    assert_eq!(parse_u64(&token), Ok(value));
                    match i64::try_from(value) {
                        Ok(signed) => assert_eq!(parse_i64(&token), Ok(signed)),
                        Err(_) => assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 0),
                    }
                }
                ConvertedNumber::I64(value) => {
                    assert_eq!(parse_i64(&token), Ok(value));
                    assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 0);
                }
                _ => panic!("test vector must be an integer"),
            }
        }
    }

    #[test]
    fn float_kind_rejection_precedes_unsigned_sign_or_range_errors() {
        for input in [
            b"0.0".as_slice(), b"1.0", b"1e0", b"1E+0", b"-1e0",
            b"-0.0", b"-0e0", b"0e308", b"1e-308", b"1e308",
            b"18446744073709551616.0", b"-9223372036854775809e0",
        ] {
            let token = token(input);
            assert_eq!(token.kind(), NumberLexKind::FloatDecimal);
            assert_error(parse_u64(&token), ErrorCode::InvalidNumber, 0);
            assert_error(parse_i64(&token), ErrorCode::InvalidNumber, 0);
            assert_error(convert_integer(&token), ErrorCode::InvalidNumber, 0);
        }
    }

    #[test]
    fn diagnostics_use_full_token_start_including_wrapper_quotes() {
        for input in [
            b"prefix:18446744073709551616,".as_slice(),
            b"prefix:-9223372036854775809]",
            b"prefix:\"18446744073709551616\"}",
            b"prefix:\"-9223372036854775809\" ",
        ] {
            let token = lex_number(input, 7, &policy()).unwrap();
            assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 7);
            assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 7);
            assert_error(convert_integer(&token), ErrorCode::NumberOverflow, 7);
        }
        for input in [b"prefix:-0,".as_slice(), b"prefix:\"-0\","] {
            let token = lex_number(input, 7, &policy()).unwrap();
            assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 7);
            assert_eq!(parse_i64(&token), Ok(0));
            assert_eq!(convert_integer(&token), Ok(ConvertedNumber::I64(0)));
        }
        let token = lex_number(b"prefix:-1e0]", 7, &policy()).unwrap();
        assert_error(parse_u64(&token), ErrorCode::InvalidNumber, 7);
        assert_error(parse_i64(&token), ErrorCode::InvalidNumber, 7);
        assert_error(convert_integer(&token), ErrorCode::InvalidNumber, 7);
    }

    #[test]
    fn swar_lane_order_and_zero_lanes_are_exact() {
        for (bytes, expected) in [
            (*b"00000000", 0), (*b"00000001", 1), (*b"10000000", 10_000_000),
            (*b"12345678", 12_345_678), (*b"87654321", 87_654_321),
            (*b"90909090", 90_909_090), (*b"09090909", 9_090_909),
            (*b"99999999", 99_999_999),
        ] {
            assert_eq!(eight_digits(bytes), expected);
        }
        let weights = [10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];
        for (lane, weight) in weights.into_iter().enumerate() {
            for digit in 0_u8..=9 {
                let mut bytes = *b"00000000";
                bytes[lane] = b'0' + digit;
                assert_eq!(eight_digits(bytes), u64::from(digit) * weight);
                let mut bytes = *b"99999999";
                bytes[lane] = b'0' + digit;
                assert_eq!(eight_digits(bytes), 99_999_999 - u64::from(9 - digit) * weight);
            }
        }
    }

    #[test]
    fn every_digit_length_and_tail_length_matches_scalar_reference() {
        let mut digits = [b'0'; 24];
        let mut expected = 0_u128;
        for length in 1..=digits.len() {
            let digit = if length == 1 { 1 } else { (length % 10) as u8 };
            digits[length - 1] = b'0' + digit;
            expected = expected * 10 + u128::from(digit);
            let token = token(&digits[..length]);
            match u64::try_from(expected) {
                Ok(value) => assert_eq!(parse_u64(&token), Ok(value)),
                Err(_) => assert_error(parse_u64(&token), ErrorCode::NumberOverflow, 0),
            }
            match i64::try_from(expected) {
                Ok(value) => assert_eq!(parse_i64(&token), Ok(value)),
                Err(_) => assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 0),
            }
        }
    }

    #[test]
    fn deterministic_full_width_values_round_trip_through_lexer() {
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        for _ in 0..2048 {
            // Fixed-seed xorshift needs neither a dependency nor wrapping sums.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut text = NumberText::new();
            write!(&mut text, "{state}").unwrap();
            let token = token(text.as_bytes());
            assert_eq!(parse_u64(&token), Ok(state));
            assert_eq!(convert_integer(&token), Ok(ConvertedNumber::U64(state)));
            match i64::try_from(state) {
                Ok(value) => assert_eq!(parse_i64(&token), Ok(value)),
                Err(_) => assert_error(parse_i64(&token), ErrorCode::NumberOverflow, 0),
            }

            let signed = i64::from_le_bytes(state.to_le_bytes());
            let mut text = NumberText::new();
            write!(&mut text, "{signed}").unwrap();
            let signed_token = lex_number(text.as_bytes(), 0, &policy()).unwrap();
            assert_eq!(parse_i64(&signed_token), Ok(signed));
            if signed < 0 {
                assert_error(parse_u64(&signed_token), ErrorCode::NumberOverflow, 0);
                assert_eq!(convert_integer(&signed_token), Ok(ConvertedNumber::I64(signed)));
            } else {
                assert_eq!(parse_u64(&signed_token), Ok(signed as u64));
                assert_eq!(convert_integer(&signed_token), Ok(ConvertedNumber::U64(signed as u64)));
            }
        }
    }

    #[test]
    fn exact_slice_boundaries_and_all_start_alignments_are_supported() {
        for length in [7_usize, 8, 9, 15, 16, 17, 19, 20] {
            let spelling = &b"12345678901234567890"[..length];
            let expected = spelling.iter().fold(0_u64, |value, &byte| {
                value * 10 + u64::from(byte - b'0')
            });
            for offset in 0..16 {
                let mut input = [b'x'; 40];
                input[offset..offset + length].copy_from_slice(spelling);
                let end = offset + length;
                let token = lex_number(&input[..end], offset as u32, &policy()).unwrap();
                assert_eq!(parse_u64(&token), Ok(expected));
                assert_eq!(token.raw_bytes().as_ptr(), input[offset..].as_ptr());
                for delimiter in [b' ', b'\t', b'\r', b'\n', b',', b']', b'}'] {
                    input[end] = delimiter;
                    let token = lex_number(&input, offset as u32, &policy()).unwrap();
                    assert_eq!(parse_u64(&token), Ok(expected));
                    assert_eq!(token.span(), Span32::new(offset as u32, length as u32));
                }
            }
        }
    }

    #[test]
    fn arbitrarily_long_lexed_integers_fail_without_truncating() {
        let mut input = [b'9'; 8192];
        let positive = token(&input);
        assert_eq!(positive.integer_digits(), 8192);
        assert_error(parse_u64(&positive), ErrorCode::NumberOverflow, 0);
        assert_error(parse_i64(&positive), ErrorCode::NumberOverflow, 0);
        assert_error(convert_integer(&positive), ErrorCode::NumberOverflow, 0);

        input[0] = b'-';
        let negative = token(&input);
        assert_error(parse_u64(&negative), ErrorCode::NumberOverflow, 0);
        assert_error(parse_i64(&negative), ErrorCode::NumberOverflow, 0);
        assert_error(convert_integer(&negative), ErrorCode::NumberOverflow, 0);

        input[0] = b'"';
        input[8191] = b'"';
        let quoted = token(&input);
        assert!(quoted.is_quoted());
        assert_error(parse_u64(&quoted), ErrorCode::NumberOverflow, 0);
        assert_error(parse_i64(&quoted), ErrorCode::NumberOverflow, 0);
        assert_error(convert_integer(&quoted), ErrorCode::NumberOverflow, 0);
    }

    #[test]
    fn conversion_preserves_input_token_metadata_and_borrowed_spelling() {
        for input in [
            b"prefix:\"-9223372036854775808\"}".as_slice(),
            b"prefix:18446744073709551615,",
            b"prefix:\"18446744073709551616\"]",
            b"prefix:-0 ",
            b"prefix:-1e0]",
        ] {
            let mut saved_bytes = [0_u8; 64];
            saved_bytes[..input.len()].copy_from_slice(input);
            let token = lex_number(input, 7, &policy()).unwrap();
            let saved_token = token;
            let saved_pointer = token.raw_bytes().as_ptr();
            let _ = parse_u64(&token);
            let _ = parse_i64(&token);
            let _ = convert_integer(&token);
            assert_eq!(input, &saved_bytes[..input.len()]);
            assert_eq!(token, saved_token);
            assert_eq!(token.raw_bytes().as_ptr(), saved_pointer);
            assert_eq!(token.span().slice(input), saved_token.span().slice(input));
        }
    }
}
