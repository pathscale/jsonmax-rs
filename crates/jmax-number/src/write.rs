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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::float::{convert_float, parse_f64};
    use crate::integer::{convert_integer, parse_i64, parse_u64};
    use crate::lex::lex_number;
    use core::fmt::Write;

    const SIGN: u64 = 1_u64 << 63;
    const PREFIX: &[u8] = b"prefix:";
    const CAPACITY: usize = MAX_F64_TEXT_BYTES + 32;

    struct RecordingSink {
        bytes: [u8; CAPACITY],
        len: usize,
        limit: usize,
        reserve_calls: usize,
        write_calls: usize,
        reserved: Option<usize>,
        last_reserved: usize,
        last_written: usize,
        reserve_error: Option<Error>,
        write_error: Option<(usize, Error)>,
    }

    impl RecordingSink {
        fn new() -> Self {
            let mut bytes = [0xa5; CAPACITY];
            bytes[..PREFIX.len()].copy_from_slice(PREFIX);
            Self {
                bytes,
                len: PREFIX.len(),
                limit: CAPACITY,
                reserve_calls: 0,
                write_calls: 0,
                reserved: None,
                last_reserved: 0,
                last_written: 0,
                reserve_error: None,
                write_error: None,
            }
        }

        fn output(&self) -> &[u8] {
            &self.bytes[PREFIX.len()..self.len]
        }

        fn assert_one_write(&self) {
            assert_eq!(self.reserve_calls, 1);
            assert_eq!(self.write_calls, 1);
            assert_eq!(self.last_reserved, self.output().len());
            assert_eq!(self.last_written, self.output().len());
            assert_eq!(self.reserved, None);
            assert_eq!(&self.bytes[..PREFIX.len()], PREFIX);
            assert!(self.bytes[self.len..].iter().all(|&byte| byte == 0xa5));
        }

        fn assert_untouched(&self) {
            assert_eq!(self.reserve_calls, 0);
            assert_eq!(self.write_calls, 0);
            assert_eq!(self.len, PREFIX.len());
            assert_eq!(self.reserved, None);
            assert_eq!(&self.bytes[..PREFIX.len()], PREFIX);
            assert!(self.bytes[self.len..].iter().all(|&byte| byte == 0xa5));
        }
    }

    impl ByteSink for RecordingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            assert_eq!(self.reserved, None, "reservation must precede one write");
            self.reserve_calls += 1;
            self.last_reserved = additional;
            if let Some(error) = &self.reserve_error {
                return Err(error.clone());
            }
            if additional > self.limit.saturating_sub(self.len) {
                return Err(sink_error());
            }
            self.reserved = Some(additional);
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.write_calls += 1;
            self.last_written = bytes.len();
            assert_eq!(self.reserved.take(), Some(bytes.len()));
            assert!(bytes.len() <= self.limit - self.len);
            let accepted = match &self.write_error {
                Some((prefix, _)) => (*prefix).min(bytes.len()),
                None => bytes.len(),
            };
            let end = self.len + accepted;
            self.bytes[self.len..end].copy_from_slice(&bytes[..accepted]);
            self.len = end;
            match &self.write_error {
                Some((_, error)) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn len(&self) -> usize {
            self.len
        }
    }

    fn sink_error() -> Error {
        Error::new(ErrorCode::BudgetExhausted, ErrorSource::Transport, 123, "sink failure")
            .with_schema(456, 789)
    }

    fn policy() -> NumericPolicy {
        NumericPolicy {
            allow_Quoted_int: false,
            reject_non_finite_float: true,
            reject_negative_zero: false,
            max_exponent_abs: 0,
        }
    }

    fn assert_encode_error<T: fmt::Debug>(result: Result<T>, code: ErrorCode) {
        let error = result.expect_err("encoding must fail");
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn assert_unsigned(value: u64, expected: Option<&[u8]>) {
        let mut sink = RecordingSink::new();
        write_u64(value, &mut sink).unwrap();
        sink.assert_one_write();
        assert!(sink.output().len() <= MAX_U64_TEXT_BYTES);
        if let Some(expected) = expected {
            assert_eq!(sink.output(), expected);
        }
        let token = lex_number(&sink.bytes[..sink.len], PREFIX.len() as u32, &policy()).unwrap();
        assert_eq!(token.raw_bytes(), sink.output());
        assert_eq!(token.kind(), NumberLexKind::UnsignedInt);
        assert_eq!(token.span(), Span32::new(PREFIX.len() as u32, sink.output().len() as u32));
        assert_eq!(parse_u64(&token), Ok(value));
        assert_eq!(convert_integer(&token), Ok(ConvertedNumber::U64(value)));
    }

    fn assert_signed(value: i64, expected: Option<&[u8]>) {
        let mut sink = RecordingSink::new();
        write_i64(value, &mut sink).unwrap();
        sink.assert_one_write();
        assert!(sink.output().len() <= MAX_I64_TEXT_BYTES);
        if let Some(expected) = expected {
            assert_eq!(sink.output(), expected);
        }
        let token = lex_number(sink.output(), 0, &policy()).unwrap();
        assert_eq!(parse_i64(&token), Ok(value));
        let expected = if value < 0 {
            ConvertedNumber::I64(value)
        } else {
            ConvertedNumber::U64(value as u64)
        };
        assert_eq!(convert_integer(&token), Ok(expected));
    }

    fn assert_float(bits: u64, policy: &NumericPolicy) {
        let value = f64::from_bits(bits);
        assert!(value.is_finite());
        let saved_policy = *policy;
        let mut sink = RecordingSink::new();
        write_f64(value, policy, &mut sink).unwrap();
        sink.assert_one_write();
        assert!(!sink.output().is_empty());
        assert!(sink.output().len() <= MAX_F64_TEXT_BYTES);
        assert!(!sink.output().iter().any(|&byte| matches!(byte, b'e' | b'E')));
        let token = lex_number(&sink.bytes[..sink.len], PREFIX.len() as u32, policy).unwrap();
        assert_eq!(token.raw_bytes(), sink.output());
        assert_eq!(token.exponent(), 0);
        assert!(!token.is_quoted());
        assert_eq!(token.is_negative(), value.is_sign_negative());
        assert_eq!(token.span(), Span32::new(PREFIX.len() as u32, sink.output().len() as u32));
        assert_eq!(parse_f64(&token, policy).unwrap().to_bits(), bits);
        assert_eq!(convert_float(&token, policy), Ok(ConvertedNumber::F64Bits(bits)));
        assert_eq!(*policy, saved_policy);
    }

    #[test]
    fn unsigned_exact_vectors_cover_full_range_and_digit_groups() {
        for (value, expected) in [
            (0, b"0".as_slice()),
            (1, b"1"),
            (9, b"9"),
            (10, b"10"),
            (99, b"99"),
            (100, b"100"),
            (12_345_678, b"12345678"),
            (99_999_999, b"99999999"),
            (100_000_000, b"100000000"),
            (1_234_567_890_123_456, b"1234567890123456"),
            (9_223_372_036_854_775_807, b"9223372036854775807"),
            (9_223_372_036_854_775_808, b"9223372036854775808"),
            (10_000_000_000_000_000_000, b"10000000000000000000"),
            (u64::MAX - 1, b"18446744073709551614"),
            (u64::MAX, b"18446744073709551615"),
        ] {
            assert_unsigned(value, Some(expected));
        }
    }

    #[test]
    fn signed_exact_vectors_include_minimum_without_negation_overflow() {
        for (value, expected) in [
            (0, b"0".as_slice()),
            (1, b"1"),
            (-1, b"-1"),
            (-9, b"-9"),
            (-10, b"-10"),
            (-100, b"-100"),
            (-12_345_678, b"-12345678"),
            (-1_234_567_890_123_456, b"-1234567890123456"),
            (i64::MAX - 1, b"9223372036854775806"),
            (i64::MAX, b"9223372036854775807"),
            (i64::MIN + 1, b"-9223372036854775807"),
            (i64::MIN, b"-9223372036854775808"),
        ] {
            assert_signed(value, Some(expected));
        }
    }

    #[test]
    fn every_integer_decimal_length_and_adjacent_boundary_round_trips() {
        let mut power = 1_u64;
        let mut expected = [b'0'; MAX_U64_TEXT_BYTES];
        expected[0] = b'1';
        for digits in 1..=MAX_U64_TEXT_BYTES {
            assert_unsigned(power, Some(&expected[..digits]));
            for value in [power - 1, power, power + 1] {
                assert_unsigned(value, None);
                if let Ok(signed) = i64::try_from(value) {
                    assert_signed(signed, None);
                    assert_signed(-signed, None);
                }
            }
            if digits != MAX_U64_TEXT_BYTES {
                power *= 10;
            }
        }
    }

    #[test]
    fn floating_exact_vectors_use_shortest_significant_digits() {
        for (value, expected) in [
            (0.0, b"0".as_slice()),
            (-0.0, b"-0"),
            (1.0, b"1"),
            (-1.0, b"-1"),
            (0.1, b"0.1"),
            (-0.1, b"-0.1"),
            (1.25, b"1.25"),
            (1000.0, b"1000"),
            (0.000001, b"0.000001"),
            (1.2345678901234567, b"1.2345678901234567"),
            (f64::from_bits(0x3fef_ffff_ffff_ffff), b"0.9999999999999999"),
            (f64::from_bits(0x3ff0_0000_0000_0001), b"1.0000000000000002"),
            (9_007_199_254_740_991.0, b"9007199254740991"),
            (9_007_199_254_740_992.0, b"9007199254740992"),
        ] {
            let mut sink = RecordingSink::new();
            write_f64(value, &policy(), &mut sink).unwrap();
            assert_eq!(sink.output(), expected);
            sink.assert_one_write();
            assert_float(value.to_bits(), &policy());
        }
    }

    #[test]
    fn extreme_ordinary_spellings_have_exact_digits_and_lengths() {
        let mut tiny = [b'0'; 326];
        tiny[1] = b'.';
        tiny[325] = b'5';
        let mut huge = [b'0'; 309];
        huge[..17].copy_from_slice(b"17976931348623157");
        let mut normal = [b'0'; 326];
        normal[1] = b'.';
        normal[309..].copy_from_slice(b"22250738585072014");
        for (value, expected) in [
            (f64::from_bits(1), tiny.as_slice()),
            (f64::MIN_POSITIVE, normal.as_slice()),
            (f64::MAX, huge.as_slice()),
        ] {
            for negative in [false, true] {
                let value = if negative { -value } else { value };
                let mut sink = RecordingSink::new();
                write_f64(value, &policy(), &mut sink).unwrap();
                let output = sink.output();
                if negative {
                    assert_eq!(output[0], b'-');
                    assert_eq!(&output[1..], expected);
                } else {
                    assert_eq!(output, expected);
                }
                sink.assert_one_write();
                assert_float(value.to_bits(), &policy());
            }
        }
    }

    #[test]
    fn binary_boundaries_round_trip_under_all_relevant_exponent_limits() {
        let patterns = [
            0, 1, 2, 3, 0x000f_ffff_ffff_fffe, 0x000f_ffff_ffff_ffff,
            0x0010_0000_0000_0000, 0x0010_0000_0000_0001,
            0x3fb9_9999_9999_999a, 0x3fef_ffff_ffff_ffff,
            0x3ff0_0000_0000_0000, 0x3ff0_0000_0000_0001,
            0x433f_ffff_ffff_ffff, 0x4340_0000_0000_0000,
            0x4340_0000_0000_0001, 0x7fef_ffff_ffff_fffe,
            0x7fef_ffff_ffff_ffff,
        ];
        for limit in [0, 1, 307, 308, 309, 323, 324, i16::MAX] {
            let policy = NumericPolicy { max_exponent_abs: limit, ..policy() };
            for bits in patterns {
                assert_float(bits, &policy);
                assert_float(bits | SIGN, &policy);
            }
        }
    }

    #[test]
    fn every_finite_binary_exponent_covers_significand_endpoints_and_signs() {
        for exponent in 0_u64..0x7ff {
            for fraction in [0, 1, 0x000f_ffff_ffff_ffff] {
                let bits = (exponent << 52) | fraction;
                assert_float(bits, &policy());
                assert_float(bits | SIGN, &policy());
            }
        }
    }

    #[test]
    fn deterministic_integer_and_floating_bit_patterns_round_trip() {
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        for _ in 0..2048 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            assert_unsigned(state, None);
            assert_signed(i64::from_ne_bytes(state.to_ne_bytes()), None);
            if f64::from_bits(state).is_finite() {
                for limit in [0, 308, i16::MAX] {
                    assert_float(state, &NumericPolicy { max_exponent_abs: limit, ..policy() });
                }
            }
        }
    }

    #[test]
    fn quoted_and_nonfinite_permissions_do_not_change_finite_output() {
        for value in [0.0, -0.0, 1.25, -42.0, f64::from_bits(1), f64::MAX] {
            let mut baseline = RecordingSink::new();
            write_f64(value, &policy(), &mut baseline).unwrap();
            for quoted in [false, true] {
                for reject_nonfinite in [false, true] {
                    let policy = NumericPolicy {
                        allow_Quoted_int: quoted,
                        reject_non_finite_float: reject_nonfinite,
                        ..policy()
                    };
                    let mut sink = RecordingSink::new();
                    write_f64(value, &policy, &mut sink).unwrap();
                    assert_eq!(sink.output(), baseline.output());
                    sink.assert_one_write();
                }
            }
        }
    }

    #[test]
    fn all_nonfinite_classes_are_rejected_without_sink_calls() {
        for bits in [
            0x7ff0_0000_0000_0000, 0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_0000, 0x7ff8_1234_5678_9abc,
            0x7fff_ffff_ffff_ffff,
        ] {
            for sign in [0, SIGN] {
                for reject in [false, true] {
                    let policy = NumericPolicy { reject_non_finite_float: reject, ..policy() };
                    let mut sink = RecordingSink::new();
                    sink.reserve_error = Some(sink_error());
                    assert_encode_error(
                        write_f64(f64::from_bits(bits | sign), &policy, &mut sink),
                        ErrorCode::NumberOverflow,
                    );
                    sink.assert_untouched();
                }
            }
        }
    }

    #[test]
    fn invalid_exponent_policy_precedes_nonfinite_and_negative_zero_checks() {
        for limit in [-1, i16::MIN] {
            let policy = NumericPolicy { max_exponent_abs: limit, ..NumericPolicy::strict_default() };
            for value in [0.0, -0.0, 1.0, f64::MAX, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut sink = RecordingSink::new();
                let error = write_f64(value, &policy, &mut sink).unwrap_err();
                assert_eq!(error.detail, "maximum absolute exponent must be nonnegative");
                assert_encode_error::<()>(Err(error), ErrorCode::ConstraintViolation);
                sink.assert_untouched();
            }
        }
    }

    #[test]
    fn negative_zero_policy_preserves_positive_zero_and_negative_nonzero() {
        let strict = NumericPolicy::strict_default();
        let mut sink = RecordingSink::new();
        assert_encode_error(write_f64(-0.0, &strict, &mut sink), ErrorCode::ConstraintViolation);
        sink.assert_untouched();
        for value in [0.0, -1.0, -f64::from_bits(1), -f64::MIN_POSITIVE, -f64::MAX] {
            assert_float(value.to_bits(), &strict);
        }
        assert_float(SIGN, &policy());
    }

    fn emit_case(case: usize, sink: &mut dyn ByteSink) -> Result<()> {
        match case {
            0 => write_u64(u64::MAX, sink),
            1 => write_i64(i64::MIN, sink),
            2 => write_f64(-1.25, &policy(), sink),
            _ => write_f64(-f64::from_bits(1), &policy(), sink),
        }
    }

    #[test]
    fn exact_capacity_succeeds_and_one_byte_short_fails_before_writing() {
        for case in 0..4 {
            let mut baseline = RecordingSink::new();
            emit_case(case, &mut baseline).unwrap();
            let length = baseline.output().len();
            let mut exact = RecordingSink::new();
            exact.limit = PREFIX.len() + length;
            emit_case(case, &mut exact).unwrap();
            exact.assert_one_write();
            assert_eq!(exact.output(), baseline.output());

            let mut short = RecordingSink::new();
            short.limit = PREFIX.len() + length - 1;
            assert_eq!(emit_case(case, &mut short), Err(sink_error()));
            assert_eq!(short.reserve_calls, 1);
            assert_eq!(short.last_reserved, length);
            assert_eq!(short.write_calls, 0);
            assert_eq!(short.len, PREFIX.len());
            assert_eq!(short.bytes, RecordingSink::new().bytes);
        }
    }

    #[test]
    fn reservation_errors_preserve_all_metadata_and_never_call_write() {
        for case in 0..4 {
            let mut sink = RecordingSink::new();
            sink.reserve_error = Some(sink_error());
            assert_eq!(emit_case(case, &mut sink), Err(sink_error()));
            assert_eq!(sink.reserve_calls, 1);
            assert_eq!(sink.write_calls, 0);
            assert_eq!(sink.len, PREFIX.len());
            assert_eq!(sink.bytes, RecordingSink::new().bytes);
        }
    }

    #[test]
    fn write_errors_preserve_metadata_and_allow_exactly_the_sink_accepted_prefix() {
        for case in 0..4 {
            let mut baseline = RecordingSink::new();
            emit_case(case, &mut baseline).unwrap();
            for accepted in 0..=baseline.output().len() {
                let mut sink = RecordingSink::new();
                sink.write_error = Some((accepted, sink_error()));
                assert_eq!(emit_case(case, &mut sink), Err(sink_error()));
                assert_eq!(sink.reserve_calls, 1);
                assert_eq!(sink.write_calls, 1);
                assert_eq!(sink.last_reserved, baseline.output().len());
                assert_eq!(sink.last_written, baseline.output().len());
                assert_eq!(sink.output(), &baseline.output()[..accepted]);
                assert_eq!(&sink.bytes[..PREFIX.len()], PREFIX);
                assert!(sink.bytes[sink.len..].iter().all(|&byte| byte == 0xa5));
            }
        }
    }

    #[test]
    fn consecutive_calls_append_without_resetting_the_sink() {
        let mut sink = RecordingSink::new();
        write_u64(12, &mut sink).unwrap();
        write_i64(-34, &mut sink).unwrap();
        write_f64(0.5, &policy(), &mut sink).unwrap();
        assert_eq!(sink.output(), b"12-340.5");
        assert_eq!(sink.reserve_calls, 3);
        assert_eq!(sink.write_calls, 3);
        assert_eq!(sink.reserved, None);
        assert_eq!(&sink.bytes[..PREFIX.len()], PREFIX);
    }

    #[test]
    fn emitted_numbers_stop_at_each_json_delimiter() {
        for case in 0..4 {
            let mut sink = RecordingSink::new();
            emit_case(case, &mut sink).unwrap();
            for delimiter in [b' ', b'\t', b'\r', b'\n', b',', b']', b'}'] {
                sink.bytes[sink.len] = delimiter;
                let token = lex_number(&sink.bytes[..sink.len + 1], PREFIX.len() as u32, &policy())
                    .unwrap();
                assert_eq!(token.raw_bytes(), sink.output());
                assert_eq!(token.span().len as usize, sink.output().len());
            }
        }
    }

    #[test]
    fn ordinary_validation_rejects_invalid_prefixes_suffixes_and_exponents() {
        for bytes in [
            b"".as_slice(), b"-", b"+1", b"00", b"-01", b".1", b"1.",
            b"1..0", b"1.2.3", b"--1", b"1e0", b"1E+2", b"NaN", b"inf",
            b"Infinity", b"\"1\"", b" 1", b"1 ", b"1,", b"1\n", b"1\0", b"\xff",
        ] {
            assert_encode_error(validate_ordinary(bytes), ErrorCode::InternalError);
        }
        for bytes in [b"0".as_slice(), b"-0", b"1", b"-42", b"0.1", b"-1.25", b"1000"] {
            assert_eq!(validate_ordinary(bytes), Ok(()));
        }
    }

    struct FailingFormatter;

    impl fmt::Display for FailingFormatter {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("123")?;
            Err(fmt::Error)
        }
    }

    #[test]
    fn scratch_boundaries_and_formatter_failures_return_internal_errors() {
        let bytes = [b'7'; MAX_F64_TEXT_BYTES + 1];
        let exact = core::str::from_utf8(&bytes[..MAX_F64_TEXT_BYTES]).unwrap();
        let excessive = core::str::from_utf8(&bytes).unwrap();
        let text = format_ordinary(format_args!("{exact}")).unwrap();
        assert_eq!(text.as_bytes(), exact.as_bytes());
        assert_encode_error(format_ordinary(format_args!("{excessive}")), ErrorCode::InternalError);
        assert_encode_error(format_ordinary(format_args!("1{excessive}")), ErrorCode::InternalError);
        assert_encode_error(format_ordinary(format_args!("{FailingFormatter}")), ErrorCode::InternalError);
        assert_encode_error(format_ordinary(format_args!("1x")), ErrorCode::InternalError);

        let mut scratch = NumberText::new();
        scratch.write_str(exact).unwrap();
        assert_eq!(scratch.write_str("0"), Err(fmt::Error));
        assert_eq!(scratch.as_bytes(), exact.as_bytes());
        assert_eq!(scratch.len(), MAX_F64_TEXT_BYTES);
        scratch.clear();
        assert!(scratch.is_empty());
        scratch.write_str("-0").unwrap();
        assert_eq!(scratch.as_bytes(), b"-0");
    }
}
