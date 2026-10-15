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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex_number;
    use core::fmt::Write;

    const SIGN: u64 = 1_u64 << 63;
    const INFINITY: u64 = 0x7ff0_0000_0000_0000;

    fn permissive_policy() -> NumericPolicy {
        NumericPolicy {
            allow_Quoted_int: true,
            reject_non_finite_float: false,
            reject_negative_zero: false,
            max_exponent_abs: i16::MAX,
        }
    }

    fn assert_bits(input: &[u8], expected: u64) {
        let policy = permissive_policy();
        let token = lex_number(input, 0, &policy).expect("test spelling must lex");
        let value = parse_f64(&token, &policy).expect("conversion must succeed");
        assert_eq!(value.to_bits(), expected, "input: {input:?}");
        assert!(!value.is_nan());
        assert_eq!(convert_float(&token, &policy), Ok(ConvertedNumber::F64Bits(expected)));
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

    fn assert_conversion_error(
        token: &NumberToken<'_>,
        policy: &NumericPolicy,
        code: ErrorCode,
        offset: u32,
    ) {
        assert_error(parse_f64(token, policy), code, offset);
        assert_error(convert_float(token, policy), code, offset);
    }

    #[test]
    fn ordinary_spellings_and_integer_limits_have_known_bits() {
        let cases: &[(&[u8], u64)] = &[
            (b"0", 0),
            (b"1", 0x3ff0_0000_0000_0000),
            (b"-1", 0xbff0_0000_0000_0000),
            (b"1.5", 0x3ff8_0000_0000_0000),
            (b"0.1", 0x3fb9_9999_9999_999a),
            (b"-0.1", 0xbfb9_9999_9999_999a),
            (b"1.25e+2", 0x405f_4000_0000_0000),
            (b"125E-2", 0x3ff4_0000_0000_0000),
            (b"9007199254740991", 0x433f_ffff_ffff_ffff),
            (b"9007199254740992", 0x4340_0000_0000_0000),
            (b"9007199254740993", 0x4340_0000_0000_0000),
            (b"9007199254740995", 0x4340_0000_0000_0002),
            (b"9223372036854775807", 0x43e0_0000_0000_0000),
            (b"-9223372036854775808", 0xc3e0_0000_0000_0000),
            (b"18446744073709551615", 0x43f0_0000_0000_0000),
            (b"18446744073709551616", 0x43f0_0000_0000_0000),
            (b"\"9007199254740993\"", 0x4340_0000_0000_0000),
            (b"\"-9223372036854775809\"", 0xc3e0_0000_0000_0000),
        ];
        for &(input, bits) in cases {
            assert_bits(input, bits);
        }
    }

    #[test]
    fn exact_normal_midpoints_round_to_the_even_significand() {
        let cases: &[(&[u8], u64)] = &[
            (b"1.000000000000000111022302462515654042363166809082031249", 0x3ff0_0000_0000_0000),
            (b"1.00000000000000011102230246251565404236316680908203125", 0x3ff0_0000_0000_0000),
            (b"1.000000000000000111022302462515654042363166809082031251", 0x3ff0_0000_0000_0001),
            (b"1.00000000000000033306690738754696212708950042724609375", 0x3ff0_0000_0000_0002),
            (b"-1.00000000000000011102230246251565404236316680908203125", 0xbff0_0000_0000_0000),
            (b"-1.000000000000000111022302462515654042363166809082031251", 0xbff0_0000_0000_0001),
            (b"0.999999999999999944488848768742172978818416595458984375", 0x3ff0_0000_0000_0000),
        ];
        for &(input, bits) in cases {
            assert_bits(input, bits);
        }
    }

    #[test]
    fn subnormal_and_normal_boundaries_have_known_bits() {
        let cases: &[(&[u8], u64)] = &[
            (b"1e-324", 0),
            (b"2.4703282292062327e-324", 0),
            (b"2.4703282292062328e-324", 1),
            (b"4.9406564584124654e-324", 1),
            (b"5e-324", 1),
            (b"-5e-324", SIGN | 1),
            (b"1e-323", 2),
            (b"2.225073858507201e-308", 0x000f_ffff_ffff_ffff),
            (b"2.2250738585072012e-308", 0x0010_0000_0000_0000),
            (b"2.2250738585072014e-308", 0x0010_0000_0000_0000),
            (b"2.2250738585072019e-308", 0x0010_0000_0000_0001),
        ];
        for &(input, bits) in cases {
            assert_bits(input, bits);
        }
    }

    // Construct (odd * 5^1075 + adjustment) * 10^-1075 using only integer
    // decimal arithmetic. With adjustment zero this is an exact midpoint on
    // the subnormal lattice, independent of any floating-point formatter.
    fn subnormal_midpoint(odd: u64, adjustment: i8) -> ([u8; 800], usize) {
        assert!(odd > 0 && odd % 2 == 1);
        assert!((-1..=1).contains(&adjustment));
        let mut digits = [0_u8; 800];
        let mut length = 0;
        let mut remaining = odd;
        while remaining != 0 {
            digits[length] = (remaining % 10) as u8;
            length += 1;
            remaining /= 10;
        }
        for _ in 0..1075 {
            let mut carry = 0_u16;
            for digit in &mut digits[..length] {
                let product = u16::from(*digit) * 5 + carry;
                *digit = (product % 10) as u8;
                carry = product / 10;
            }
            if carry != 0 {
                digits[length] = carry as u8;
                length += 1;
            }
        }
        // An odd multiple of 5^1075 ends in 5; perturbation needs no carry.
        assert_eq!(digits[0], 5);
        digits[0] = (5_i8 + adjustment) as u8;
        let mut text = [0_u8; 800];
        for (index, &digit) in digits[..length].iter().rev().enumerate() {
            text[index] = b'0' + digit;
        }
        text[length..length + 6].copy_from_slice(b"e-1075");
        (text, length + 6)
    }

    #[test]
    fn exact_subnormal_midpoints_and_adjacent_decimals_round_correctly() {
        for (odd, below, tie, above) in [
            (1_u64, 0_u64, 0_u64, 1_u64),
            (3, 1, 2, 2),
            (5, 2, 2, 3),
            (9_007_199_254_740_991, 0x000f_ffff_ffff_ffff,
                0x0010_0000_0000_0000, 0x0010_0000_0000_0000),
        ] {
            for (adjustment, expected) in [(-1, below), (0, tie), (1, above)] {
                let (text, length) = subnormal_midpoint(odd, adjustment);
                assert_bits(&text[..length], expected);
                let mut negative = [0_u8; 801];
                negative[0] = b'-';
                negative[1..length + 1].copy_from_slice(&text[..length]);
                assert_bits(&negative[..length + 1], SIGN | expected);
            }
        }
    }

    #[test]
    fn distant_significand_digits_are_not_discarded_at_a_midpoint() {
        let midpoint = b"1.00000000000000011102230246251565404236316680908203125";
        let mut text = [b'0'; 8192];
        text[..midpoint.len()].copy_from_slice(midpoint);
        assert_bits(&text, 0x3ff0_0000_0000_0000);
        text[8191] = b'1';
        assert_bits(&text, 0x3ff0_0000_0000_0001);
        text[midpoint.len() - 1] = b'4';
        assert_bits(&text, 0x3ff0_0000_0000_0000);
    }

    #[test]
    fn long_significands_can_be_cancelled_by_explicit_exponents() {
        let mut text = [b'0'; 4103];
        text[0] = b'1';
        text[4097..].copy_from_slice(b"e-4096");
        assert_bits(&text, 0x3ff0_0000_0000_0000);
        text[4096] = b'1';
        assert_bits(&text, 0x3ff0_0000_0000_0000);

        let mut fraction = [b'0'; 4105];
        fraction[1] = b'.';
        fraction[4098] = b'1';
        fraction[4099..].copy_from_slice(b"e+4097");
        assert_bits(&fraction, 0x3ff0_0000_0000_0000);
    }

    #[test]
    fn overflow_boundary_is_rounded_before_nonfinite_policy_is_applied() {
        for input in [b"1.7976931348623157e308".as_slice(), b"1.7976931348623158e308"] {
            assert_bits(input, 0x7fef_ffff_ffff_ffff);
            let strict = NumericPolicy::strict_default();
            let token = lex_number(input, 0, &strict).unwrap();
            assert_eq!(parse_f64(&token, &strict).unwrap().to_bits(), f64::MAX.to_bits());
        }
        for (input, bits) in [
            (b"1.7976931348623159e308".as_slice(), INFINITY),
            (b"-1.7976931348623159e308", SIGN | INFINITY),
            (b"1e309", INFINITY),
            (b"-1e309", SIGN | INFINITY),
            (b"1e32767", INFINITY),
            (b"-1e32767", SIGN | INFINITY),
        ] {
            assert_bits(input, bits);
            let token = lex_number(input, 0, &permissive_policy()).unwrap();
            let finite = NumericPolicy { reject_non_finite_float: true, ..permissive_policy() };
            assert_conversion_error(&token, &finite, ErrorCode::NumberOverflow, 0);
        }
    }

    #[test]
    fn exact_overflow_midpoint_rounds_to_infinity() {
        // The overflow midpoint is (2^54 - 1) * 2^970. Build its exact
        // 309-digit integer spelling without floating-point arithmetic.
        let mut digits = [0_u8; 309];
        let mut length = 0;
        let mut coefficient = (1_u64 << 54) - 1;
        while coefficient != 0 {
            digits[length] = (coefficient % 10) as u8;
            coefficient /= 10;
            length += 1;
        }
        for _ in 0..970 {
            let mut carry = 0_u8;
            for digit in &mut digits[..length] {
                let product = *digit * 2 + carry;
                *digit = product % 10;
                carry = product / 10;
            }
            if carry != 0 {
                digits[length] = carry;
                length += 1;
            }
        }
        assert_eq!(length, 309);
        assert_eq!(digits[0], 2);
        for (last_digit, expected) in [
            (1, 0x7fef_ffff_ffff_ffff), (2, INFINITY), (3, INFINITY),
        ] {
            digits[0] = last_digit;
            let mut text = [0_u8; 309];
            for (index, &digit) in digits.iter().rev().enumerate() {
                text[index] = b'0' + digit;
            }
            assert_bits(&text, expected);
            let mut negative = [0_u8; 310];
            negative[0] = b'-';
            negative[1..].copy_from_slice(&text);
            assert_bits(&negative, SIGN | expected);
        }
    }

    #[test]
    fn huge_integers_overflow_at_the_full_token_start() {
        let finite = NumericPolicy { reject_non_finite_float: true, ..permissive_policy() };
        let mut input = [b'9'; 8192];
        input[..7].copy_from_slice(b"prefix:");
        for negative in [false, true] {
            input[7] = if negative { b'-' } else { b'9' };
            let token = lex_number(&input, 7, &permissive_policy()).unwrap();
            assert_conversion_error(&token, &finite, ErrorCode::NumberOverflow, 7);
            assert_eq!(parse_f64(&token, &permissive_policy()).unwrap().to_bits(),
                INFINITY | if negative { SIGN } else { 0 });
        }
        input[7] = b'"';
        input[8] = b'-';
        input[8191] = b'"';
        let token = lex_number(&input, 7, &permissive_policy()).unwrap();
        assert_conversion_error(&token, &finite, ErrorCode::NumberOverflow, 7);
        assert_eq!(convert_float(&token, &permissive_policy()),
            Ok(ConvertedNumber::F64Bits(SIGN | INFINITY)));
    }

    #[test]
    fn lexical_zero_and_underflow_preserve_the_sign() {
        for input in [
            b"0".as_slice(), b"0.000", b"0e32767", b"0e-32767", b"\"0\"",
            b"1e-32767", b"1e-325", b"2e-324",
        ] {
            assert_bits(input, 0);
        }
        for input in [
            b"-0".as_slice(), b"-0.000", b"-0e32767", b"-0e-32767", b"\"-0\"",
            b"-1e-32767", b"-1e-325", b"-2e-324",
        ] {
            assert_bits(input, SIGN);
        }
    }

    #[test]
    fn stricter_negative_zero_policy_covers_lexical_and_rounded_zero() {
        let reject = NumericPolicy { reject_negative_zero: true, ..permissive_policy() };
        for (input, error_offset) in [
            (b"prefix:-0,".as_slice(), 7),
            (b"prefix:-0.000e+123]", 7),
            (b"prefix:\"-0\"}", 8),
            (b"prefix:-1e-32767 ", 7),
            (b"prefix:-2e-324\n", 7),
        ] {
            let token = lex_number(input, 7, &permissive_policy()).unwrap();
            assert_conversion_error(&token, &reject, ErrorCode::ConstraintViolation, error_offset);
        }
        for input in [b"0".as_slice(), b"1e-32767", b"5e-324", b"-5e-324", b"-0.1"] {
            let token = lex_number(input, 0, &permissive_policy()).unwrap();
            assert!(parse_f64(&token, &reject).is_ok());
        }
    }

    #[test]
    fn quoted_permission_is_rechecked_before_value_policies() {
        let reject = NumericPolicy { allow_Quoted_int: false, reject_negative_zero: true,
            ..permissive_policy() };
        for input in [b"prefix:\"0\"".as_slice(), b"prefix:\"-0\"", b"prefix:\"123\""] {
            let token = lex_number(input, 7, &permissive_policy()).unwrap();
            assert_conversion_error(&token, &reject, ErrorCode::InvalidNumber, 7);
        }
    }

    #[test]
    fn invalid_exponent_policy_precedes_other_conversion_errors() {
        for limit in [-1, i16::MIN] {
            let invalid = NumericPolicy { max_exponent_abs: limit,
                ..NumericPolicy::strict_default() };
            for input in [b"prefix:1".as_slice(), b"prefix:\"-0\"", b"prefix:1e32767"] {
                let token = lex_number(input, 7, &permissive_policy()).unwrap();
                assert_conversion_error(&token, &invalid, ErrorCode::ConstraintViolation, 7);
            }
        }
    }

    #[test]
    fn tighter_exponent_bounds_report_the_first_exponent_digit() {
        let strict = NumericPolicy::strict_default();
        for (input, offset) in [
            (b"prefix:1e309".as_slice(), 9),
            (b"prefix:1e+309", 10),
            (b"prefix:1E-309", 10),
            (b"prefix:1e000309", 9),
            (b"prefix:1E-000309", 10),
            (b"prefix:-0.00E+00309", 14),
            (b"prefix:1e32767", 9),
            (b"prefix:1e-32767", 10),
        ] {
            let token = lex_number(input, 7, &permissive_policy()).unwrap();
            assert_conversion_error(&token, &strict, ErrorCode::ConstraintViolation, offset);
            let lexical = lex_number(input, 7, &strict).unwrap_err();
            let converted = parse_f64(&token, &strict).unwrap_err();
            assert_eq!(converted, lexical);
        }
        for input in [b"1e308".as_slice(), b"1e-308", b"1E+000308", b"1E-000308"] {
            let token = lex_number(input, 0, &permissive_policy()).unwrap();
            assert!(parse_f64(&token, &strict).unwrap().is_finite());
        }
    }

    #[test]
    fn exponent_zero_limit_is_explicit_and_allows_arbitrary_leading_zeros() {
        let zero = NumericPolicy { max_exponent_abs: 0, ..permissive_policy() };
        for input in [b"1".as_slice(), b"1.0", b"1e0", b"1E+0000", b"1e-0000", b"\"1\""] {
            let token = lex_number(input, 0, &permissive_policy()).unwrap();
            assert_eq!(parse_f64(&token, &zero).unwrap().to_bits(), 1.0_f64.to_bits());
        }
        let mut text = [b'0'; 8192];
        text[..3].copy_from_slice(b"1e-");
        let token = lex_number(&text, 0, &permissive_policy()).unwrap();
        assert_eq!(parse_f64(&token, &zero).unwrap().to_bits(), 1.0_f64.to_bits());
        text[8191] = b'1';
        let token = lex_number(&text, 0, &permissive_policy()).unwrap();
        assert_conversion_error(&token, &zero, ErrorCode::ConstraintViolation, 3);
        assert_eq!(parse_f64(&token, &permissive_policy()).unwrap().to_bits(), 0.1_f64.to_bits());

        // A fractional scale is not an explicit exponent and has no bound here.
        let mut fraction = [b'0'; 4096];
        fraction[1] = b'.';
        fraction[4095] = b'1';
        let token = lex_number(&fraction, 0, &permissive_policy()).unwrap();
        assert_eq!(parse_f64(&token, &zero).unwrap().to_bits(), 0);
    }

    #[test]
    fn safe_utf8_and_core_parser_failures_map_to_canonical_errors() {
        // Exercise the private failure adapter without forging NumberToken.
        for raw in [b"".as_slice(), b"-", b"1e+", b"1.2x", b"\xff", b"1\xc0\xaf"] {
            for offset in [0, 17, u32::MAX] {
                assert_error(parse_decimal(raw, offset), ErrorCode::InvalidNumber, offset);
            }
        }
    }

    #[test]
    fn exact_slice_boundaries_and_delimiters_do_not_enter_the_parser() {
        for spelling in [b"1.25".as_slice(), b"125e-2", b"\"1\"", b"-0"] {
            let expected = match spelling {
                b"\"1\"" => 1.0_f64.to_bits(),
                b"-0" => SIGN,
                _ => 1.25_f64.to_bits(),
            };
            for start in 0..16 {
                let mut input = [b'x'; 48];
                let end = start + spelling.len();
                input[start..end].copy_from_slice(spelling);
                let token = lex_number(&input[..end], start as u32, &permissive_policy()).unwrap();
                assert_eq!(parse_f64(&token, &permissive_policy()).unwrap().to_bits(), expected);
                for delimiter in [b' ', b'\t', b'\r', b'\n', b',', b']', b'}'] {
                    input[end] = delimiter;
                    let token = lex_number(&input, start as u32, &permissive_policy()).unwrap();
                    assert_eq!(token.span(), Span32::new(start as u32, spelling.len() as u32));
                    assert_eq!(parse_f64(&token, &permissive_policy()).unwrap().to_bits(), expected);
                }
            }
        }
    }

    #[test]
    fn deterministic_finite_bit_patterns_round_trip() {
        let policy = permissive_policy();
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        for _ in 0..2048 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let value = f64::from_bits(state);
            if !value.is_finite() {
                continue;
            }
            let mut text = NumberText::new();
            write!(&mut text, "{value:e}").unwrap();
            let token = lex_number(text.as_bytes(), 0, &policy).unwrap();
            assert_eq!(parse_f64(&token, &policy).unwrap().to_bits(), state);
            assert_eq!(convert_float(&token, &policy), Ok(ConvertedNumber::F64Bits(state)));
        }
    }

    #[test]
    fn success_and_errors_preserve_input_policy_and_token_metadata() {
        for input in [
            b"prefix:\"-0\"}".as_slice(), b"prefix:1.25e0,", b"prefix:1e309]",
            b"prefix:-1e-32767 ", b"prefix:18446744073709551616,",
        ] {
            let policy = permissive_policy();
            let saved_policy = policy;
            let mut saved_bytes = [0_u8; 64];
            saved_bytes[..input.len()].copy_from_slice(input);
            let token = lex_number(input, 7, &policy).unwrap();
            let saved_token = token;
            let pointer = token.raw_bytes().as_ptr();
            let _ = parse_f64(&token, &policy);
            let _ = convert_float(&token, &policy);
            let _ = parse_f64(&token, &NumericPolicy::strict_default());
            let _ = convert_float(&token, &NumericPolicy::strict_default());
            assert_eq!(input, &saved_bytes[..input.len()]);
            assert_eq!(policy, saved_policy);
            assert_eq!(token, saved_token);
            assert_eq!(token.raw_bytes().as_ptr(), pointer);
        }
    }
}
