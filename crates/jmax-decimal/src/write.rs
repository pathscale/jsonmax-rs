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

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &[(i128, u8, &[u8])] = &[
        (0, 0, b"\"0\""),
        (0, 1, b"\"0.0\""),
        (0, 2, b"\"0.00\""),
        (1, 0, b"\"1\""),
        (-1, 0, b"\"-1\""),
        (123, 2, b"\"1.23\""),
        (-123, 2, b"\"-1.23\""),
        (1, 4, b"\"0.0001\""),
        (-1, 4, b"\"-0.0001\""),
        (10, 2, b"\"0.10\""),
        (-10, 2, b"\"-0.10\""),
        (100, 2, b"\"1.00\""),
        (100, 3, b"\"0.100\""),
        (1000, 2, b"\"10.00\""),
        (-1000, 2, b"\"-10.00\""),
        (12300, 4, b"\"1.2300\""),
        (-12300, 4, b"\"-1.2300\""),
        (10001, 4, b"\"1.0001\""),
        (1234567890123456789, 9, b"\"1234567890.123456789\""),
        (-1234567890123456789, 10, b"\"-123456789.0123456789\""),
    ];

    // A bounded sink records the writer's call pattern independently of the
    // slice writer and can inject errors before or after partial progress.
    struct RecordingSink {
        bytes: [u8; MAX_DECIMAL_TEXT_LEN + 16],
        used: usize,
        reserve_calls: usize,
        write_calls: usize,
        reserved: usize,
        reservation: Option<usize>,
        reserve_error: Option<Error>,
        write_error: Option<Error>,
        partial_write: usize,
    }

    impl RecordingSink {
        fn new(prefix: &[u8]) -> Self {
            let mut bytes = [0xa5; MAX_DECIMAL_TEXT_LEN + 16];
            bytes[..prefix.len()].copy_from_slice(prefix);
            Self {
                bytes,
                used: prefix.len(),
                reserve_calls: 0,
                write_calls: 0,
                reserved: 0,
                reservation: None,
                reserve_error: None,
                write_error: None,
                partial_write: 0,
            }
        }

        fn append(&mut self, bytes: &[u8]) {
            let end = self.used + bytes.len();
            self.bytes[self.used..end].copy_from_slice(bytes);
            self.used = end;
        }

        fn as_slice(&self) -> &[u8] {
            &self.bytes[..self.used]
        }
    }

    impl ByteSink for RecordingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reserve_calls += 1;
            self.reserved = additional;
            self.reservation = None;
            if let Some(error) = &self.reserve_error {
                return Err(error.clone());
            }
            if additional > self.bytes.len() - self.used {
                return Err(Error::new(
                    ErrorCode::BudgetExhausted,
                    ErrorSource::Encode,
                    self.used as u32,
                    "recording sink capacity exceeded",
                ));
            }
            self.reservation = Some(additional);
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.write_calls += 1;
            assert_eq!(self.reservation.take(), Some(bytes.len()));
            if let Some(error) = self.write_error.clone() {
                let written = core::cmp::min(self.partial_write, bytes.len());
                self.append(&bytes[..written]);
                return Err(error);
            }
            self.append(bytes);
            Ok(())
        }

        fn write_byte(&mut self, _byte: u8) -> Result<()> {
            panic!("decimal writer must use one complete write_bytes call");
        }

        fn len(&self) -> usize {
            self.used
        }
    }

    fn policy(scale: u8) -> DecimalPolicy {
        DecimalPolicy {
            max_precision: MAX_PRECISION,
            target_scale: scale,
            allow_exponent: false,
            reject_inexact_rescale: true,
            tick_step: 1,
        }
    }

    fn assert_round_trip(value: Decimal128) {
        let mut output = [0xa5; MAX_DECIMAL_TEXT_LEN];
        let bytes = write_to_slice(value, &mut output).unwrap();
        assert_eq!(bytes.len(), encoded_len(value));
        assert!(bytes.len() <= MAX_DECIMAL_TEXT_LEN);
        // Exercise the last representable absolute end, not just offset zero.
        let offset = u32::MAX - bytes.len() as u32;
        let lexeme = lex::lex_quoted(bytes, offset, false).unwrap();
        assert_eq!(lexeme.raw(), bytes);
        assert_eq!(lexeme.span(), Span32::new(offset, bytes.len() as u32));
        assert_eq!(lexeme.span().end(), Some(u32::MAX));
        assert_eq!(lexeme.is_negative(), value.coefficient < 0);
        assert_eq!(lexeme.exponent(), 0);
        assert!(!lexeme.integer_digits().is_empty());
        assert_eq!(lexeme.fractional_digits().len(), usize::from(value.scale));
        assert_eq!(
            bytes.len(),
            2 + usize::from(value.coefficient < 0)
                + lexeme.integer_digits().len()
                + usize::from(value.scale != 0)
                + lexeme.fractional_digits().len(),
        );

        let accumulated = parse::accumulate(&lexeme).unwrap();
        assert_eq!(accumulated.coefficient, value.coefficient);
        assert_eq!(accumulated.fractional_digits, u32::from(value.scale));
        assert_eq!(accumulated.exponent, 0);
        assert_eq!(accumulated.precision, coefficient_precision(value.coefficient));
        let policy = policy(value.scale);
        let rescaled = bounds::rescale(accumulated, &policy, offset).unwrap();
        assert_eq!(rescaled, value);
        bounds::validate(
            rescaled,
            &policy,
            DecimalBounds { min: Some(value), max: Some(value) },
            offset,
        ).unwrap();
        assert_eq!(bounds::compare(rescaled, value), core::cmp::Ordering::Equal);
        assert_eq!(parse::parse_quoted(bytes, offset, &policy).unwrap(), value);
    }

    #[test]
    fn exact_vectors_match_length_and_destinations() {
        for &(coefficient, scale, expected) in VECTORS {
            let value = Decimal128::new(coefficient, scale);
            assert_eq!(encoded_len(value), expected.len());
            let mut output = [0xa5; MAX_DECIMAL_TEXT_LEN + 8];
            assert_eq!(write_to_slice(value, &mut output).unwrap(), expected);
            assert!(output[expected.len()..].iter().all(|&byte| byte == 0xa5));

            let mut sink = RecordingSink::new(b"prefix:");
            write_quoted(value, &mut sink).unwrap();
            assert_eq!(sink.reserve_calls, 1);
            assert_eq!(sink.write_calls, 1);
            assert_eq!(sink.reserved, expected.len());
            assert_eq!(sink.len(), 7 + expected.len());
            assert_eq!(&sink.as_slice()[..7], b"prefix:");
            assert_eq!(&sink.as_slice()[7..], expected);
            assert!(sink.bytes[sink.used..].iter().all(|&byte| byte == 0xa5));
            assert_round_trip(value);
        }
    }

    #[test]
    fn signed_endpoints_have_exact_fixed_point_spellings() {
        let vectors: &[(i128, u8, &[u8])] = &[
            (i128::MAX, 0, b"\"170141183460469231731687303715884105727\""),
            (i128::MIN, 0, b"\"-170141183460469231731687303715884105728\""),
            (i128::MAX, 1, b"\"17014118346046923173168730371588410572.7\""),
            (i128::MIN, 1, b"\"-17014118346046923173168730371588410572.8\""),
            (i128::MAX, 19, b"\"17014118346046923173.1687303715884105727\""),
            (i128::MIN, 19, b"\"-17014118346046923173.1687303715884105728\""),
            (i128::MAX, 38, b"\"1.70141183460469231731687303715884105727\""),
            (i128::MIN, 38, b"\"-1.70141183460469231731687303715884105728\""),
            (i128::MAX, 39, b"\"0.170141183460469231731687303715884105727\""),
            (i128::MIN, 39, b"\"-0.170141183460469231731687303715884105728\""),
            (i128::MAX, 40, b"\"0.0170141183460469231731687303715884105727\""),
            (i128::MIN, 40, b"\"-0.0170141183460469231731687303715884105728\""),
        ];
        for &(coefficient, scale, expected) in vectors {
            let value = Decimal128::new(coefficient, scale);
            let mut output = [0_u8; MAX_DECIMAL_TEXT_LEN];
            assert_eq!(encoded_len(value), expected.len());
            assert_eq!(write_to_slice(value, &mut output).unwrap(), expected);
            let mut sink = RecordingSink::new(b"");
            write_quoted(value, &mut sink).unwrap();
            assert_eq!(sink.as_slice(), expected);
            assert_eq!((sink.reserve_calls, sink.write_calls), (1, 1));
            assert_eq!(sink.reserved, expected.len());
            assert_round_trip(value);
        }
    }

    #[test]
    fn maximum_scale_has_exact_padding_and_fits_bound() {
        let vectors: &[(i128, &[u8])] = &[
            (0, b"0"),
            (1, b"1"),
            (-1, b"1"),
            (10, b"10"),
            (-10, b"10"),
            (i128::MAX, b"170141183460469231731687303715884105727"),
            (i128::MIN, b"170141183460469231731687303715884105728"),
        ];
        for &(coefficient, digits) in vectors {
            let value = Decimal128::new(coefficient, u8::MAX);
            let prefix: &[u8] = if coefficient < 0 { b"\"-0." } else { b"\"0." };
            let len = prefix.len() + 255 + 1;
            let mut expected = [b'0'; MAX_DECIMAL_TEXT_LEN];
            expected[..prefix.len()].copy_from_slice(prefix);
            expected[len - 1 - digits.len()..len - 1].copy_from_slice(digits);
            expected[len - 1] = b'"';
            assert_eq!(len, 259 + usize::from(coefficient < 0));
            assert_eq!(encoded_len(value), len);

            let mut output = [0xa5; MAX_DECIMAL_TEXT_LEN];
            assert_eq!(write_to_slice(value, &mut output).unwrap(), &expected[..len]);
            let mut sink = RecordingSink::new(b"prefix:");
            write_quoted(value, &mut sink).unwrap();
            assert_eq!(&sink.as_slice()[7..], &expected[..len]);
            assert_eq!((sink.reserve_calls, sink.write_calls), (1, 1));
            assert_eq!(sink.reserved, len);
            assert_round_trip(value);
        }
    }

    #[test]
    fn all_scales_round_trip_through_every_prior_wave() {
        let coefficients = [
            i128::MIN, i128::MIN + 1, -i128::MAX, -1000100, -100, -10, -1,
            0, 1, 10, 100, 1000100, i128::MAX - 1, i128::MAX,
        ];
        for scale in 0..=u8::MAX {
            for coefficient in coefficients {
                assert_round_trip(Decimal128::new(coefficient, scale));
            }
        }
    }

    #[test]
    fn powers_of_ten_and_neighbors_round_trip_at_digit_boundaries() {
        // 10^38 and its neighbors fit in i128. A checked multiplication is
        // used only when the next loop iteration needs a larger power.
        let mut power = 1_i128;
        for exponent in 0..=38_u8 {
            for magnitude in [power - 1, power, power + 1] {
                for coefficient in [magnitude, -magnitude] {
                    for scale in [0, 1, exponent, exponent + 1, exponent + 2, 255] {
                        assert_round_trip(Decimal128::new(coefficient, scale));
                    }
                }
            }
            if exponent < 38 {
                power = power.checked_mul(10).unwrap();
            }
        }
    }

    #[test]
    fn zero_keeps_every_scale_without_a_minus() {
        for scale in 0..=u8::MAX {
            let value = Decimal128::new(0, scale);
            let mut output = [0xa5; MAX_DECIMAL_TEXT_LEN];
            let bytes = write_to_slice(value, &mut output).unwrap();
            if scale == 0 {
                assert_eq!(bytes, b"\"0\"");
            } else {
                assert_eq!(bytes.len(), usize::from(scale) + 4);
                assert_eq!(&bytes[..3], b"\"0.");
                assert!(bytes[3..bytes.len() - 1].iter().all(|&byte| byte == b'0'));
                assert_eq!(bytes[bytes.len() - 1], b'"');
            }
            assert!(!bytes.contains(&b'-'));
            assert_eq!(parse::parse_quoted(bytes, 0, &policy(scale)).unwrap(), value);
        }
    }

    #[test]
    fn equal_values_keep_distinct_coefficient_and_scale() {
        for negative in [false, true] {
            let mut coefficient = if negative { -1_i128 } else { 1_i128 };
            let integer = Decimal128::new(coefficient, 0);
            for scale in 0..=38_u8 {
                let value = Decimal128::new(coefficient, scale);
                assert_eq!(bounds::compare(value, integer), core::cmp::Ordering::Equal);
                let mut output = [0_u8; MAX_DECIMAL_TEXT_LEN];
                let bytes = write_to_slice(value, &mut output).unwrap();
                let lexeme = lex::lex_quoted(bytes, 0, false).unwrap();
                assert_eq!(lexeme.integer_digits(), b"1");
                assert_eq!(lexeme.fractional_digits().len(), usize::from(scale));
                assert!(lexeme.fractional_digits().iter().all(|&byte| byte == b'0'));
                assert_eq!(parse::parse_quoted(bytes, 0, &policy(scale)).unwrap(), value);
                if scale < 38 {
                    coefficient = coefficient.checked_mul(10).unwrap();
                }
            }
        }
    }

    #[test]
    fn exact_capacity_returns_borrowed_prefix_and_preserves_guards() {
        for coefficient in [i128::MIN, -1, 0, 1, i128::MAX] {
            for scale in 0..=u8::MAX {
                let value = Decimal128::new(coefficient, scale);
                let len = encoded_len(value);
                let mut guarded = [0xa5; MAX_DECIMAL_TEXT_LEN + 8];
                let start = guarded[4..].as_ptr();
                let bytes = write_to_slice(value, &mut guarded[4..4 + len]).unwrap();
                assert_eq!(bytes.len(), len);
                assert_eq!(bytes.as_ptr(), start);
                assert_eq!(parse::parse_quoted(bytes, 0, &policy(scale)).unwrap(), value);
                assert_eq!(&guarded[..4], &[0xa5; 4]);
                assert!(guarded[4 + len..].iter().all(|&byte| byte == 0xa5));
            }
        }
    }

    #[test]
    fn every_short_capacity_fails_without_mutation() {
        for coefficient in [i128::MIN, -1, 0, 1, i128::MAX] {
            for scale in [0, 1, 2, 38, 39, 40, 254, 255] {
                let value = Decimal128::new(coefficient, scale);
                for capacity in 0..encoded_len(value) {
                    let mut output = [0xa5; MAX_DECIMAL_TEXT_LEN + 8];
                    let error = write_to_slice(value, &mut output[4..4 + capacity])
                        .unwrap_err();
                    assert_eq!(error.code, ErrorCode::BudgetExhausted);
                    assert_eq!(error.source, ErrorSource::Encode);
                    assert_eq!(error.byte_offset, 0);
                    assert_eq!((error.schema_id, error.field_id), (0, 0));
                    assert!(!error.detail.is_empty());
                    assert_eq!(output, [0xa5; MAX_DECIMAL_TEXT_LEN + 8]);
                }
            }
        }
    }

    #[test]
    fn reserve_failure_propagates_without_writing() {
        let expected = Error::new(
            ErrorCode::Cancelled, ErrorSource::Transport, u32::MAX, "reserve cancelled",
        ).with_schema(17, 29);
        for value in [Decimal128::new(0, 0), Decimal128::new(i128::MIN, 255)] {
            let mut sink = RecordingSink::new(b"prefix:");
            let original = sink.bytes;
            sink.reserve_error = Some(expected.clone());
            sink.write_error = Some(Error::new(
                ErrorCode::InternalError, ErrorSource::Encode, 11, "write must not run",
            ));
            sink.partial_write = 3;
            assert_eq!(write_quoted(value, &mut sink).unwrap_err(), expected);
            assert_eq!((sink.reserve_calls, sink.write_calls), (1, 0));
            assert_eq!(sink.reserved, encoded_len(value));
            assert_eq!(sink.len(), 7);
            assert_eq!(sink.bytes, original);
        }
    }

    #[test]
    fn write_failure_before_progress_propagates_unchanged() {
        let expected = Error::new(
            ErrorCode::AuthFailed, ErrorSource::Auth, 1234, "sink write denied",
        ).with_schema(u32::MAX, 91);
        for value in [Decimal128::new(1, 2), Decimal128::new(i128::MIN, 255)] {
            let mut sink = RecordingSink::new(b"prefix:");
            let original = sink.bytes;
            sink.write_error = Some(expected.clone());
            assert_eq!(write_quoted(value, &mut sink).unwrap_err(), expected);
            assert_eq!((sink.reserve_calls, sink.write_calls), (1, 1));
            assert_eq!(sink.reserved, encoded_len(value));
            assert_eq!(sink.len(), 7);
            assert_eq!(sink.bytes, original);
        }
    }

    #[test]
    fn partial_sink_failures_propagate_without_retry_or_rollback() {
        let value = Decimal128::new(i128::MIN, 255);
        let mut output = [0_u8; MAX_DECIMAL_TEXT_LEN];
        let encoded = write_to_slice(value, &mut output).unwrap();
        let expected = Error::new(
            ErrorCode::TransportIncomplete, ErrorSource::Wire, 47, "partial sink write",
        ).with_schema(19, 23);
        // Include an error after the sink accepted every byte: the writer must
        // still preserve the reported failure and must not attempt another write.
        for progress in 0..=encoded.len() {
            let mut sink = RecordingSink::new(b"prefix:");
            sink.write_error = Some(expected.clone());
            sink.partial_write = progress;
            assert_eq!(write_quoted(value, &mut sink).unwrap_err(), expected);
            assert_eq!((sink.reserve_calls, sink.write_calls), (1, 1));
            assert_eq!(sink.reserved, encoded.len());
            assert_eq!(sink.len(), 7 + progress);
            assert_eq!(&sink.as_slice()[..7], b"prefix:");
            assert_eq!(&sink.as_slice()[7..], &encoded[..progress]);
            assert!(sink.bytes[sink.used..].iter().all(|&byte| byte == 0xa5));
        }
    }

    #[test]
    fn repeated_writes_append_one_complete_token_each() {
        let mut sink = RecordingSink::new(b"prefix:");
        for (index, value) in [
            Decimal128::new(0, 0),
            Decimal128::new(123, 2),
            Decimal128::new(-100, 4),
            Decimal128::new(i128::MIN, 0),
        ].into_iter().enumerate() {
            let before = sink.len();
            write_quoted(value, &mut sink).unwrap();
            assert_eq!(sink.reserve_calls, index + 1);
            assert_eq!(sink.write_calls, index + 1);
            assert_eq!(sink.reserved, encoded_len(value));
            assert_eq!(sink.len(), before + encoded_len(value));
        }
        assert_eq!(
            sink.as_slice(),
            b"prefix:\"0\"\"1.23\"\"-0.0100\"\"-170141183460469231731687303715884105728\"",
        );
    }

    #[test]
    fn parsing_exponents_and_negative_zero_produces_exact_fixed_point_output() {
        let vectors: &[(&[u8], u8, &[u8])] = &[
            (b"\"-0\"", 2, b"\"0.00\""),
            (b"\"-0.000\"", 0, b"\"0\""),
            (b"\"-0E-32767\"", 4, b"\"0.0000\""),
            (b"\"12.3400\"", 2, b"\"12.34\""),
            (b"\"1.2300e2\"", 2, b"\"123.00\""),
            (b"\"-12.30E+2\"", 0, b"\"-1230\""),
            (b"\"100e-4\"", 4, b"\"0.0100\""),
            (b"\"0.000001e6\"", 8, b"\"1.00000000\""),
            (b"\"12e-2\"", 4, b"\"0.1200\""),
        ];
        for &(input, scale, expected) in vectors {
            let mut exponent_policy = policy(scale);
            exponent_policy.allow_exponent = true;
            let value = parse::parse_quoted(input, 83, &exponent_policy).unwrap();
            let mut output = [0_u8; MAX_DECIMAL_TEXT_LEN];
            let bytes = write_to_slice(value, &mut output).unwrap();
            assert_eq!(bytes, expected);
            assert_eq!(encoded_len(value), expected.len());
            assert_eq!(parse::parse_quoted(bytes, 83, &policy(scale)).unwrap(), value);
            let mut sink = RecordingSink::new(b"");
            write_quoted(value, &mut sink).unwrap();
            assert_eq!(sink.as_slice(), expected);
        }
    }

    #[test]
    fn written_values_retain_parser_precision_scale_and_tick_checks() {
        for coefficient in [-123, 123] {
            let value = Decimal128::new(coefficient, 2);
            let mut output = [0_u8; MAX_DECIMAL_TEXT_LEN];
            let bytes = write_to_slice(value, &mut output).unwrap();
            let mut precision_policy = policy(2);
            precision_policy.max_precision = 2;
            let mut tick_policy = policy(2);
            tick_policy.tick_step = 5;
            for (policy, code) in [
                (policy(1), ErrorCode::DecimalScaleMismatch),
                (precision_policy, ErrorCode::DecimalPrecisionOverflow),
                (tick_policy, ErrorCode::TickMultipleViolation),
            ] {
                let error = parse::parse_quoted(bytes, 97, &policy).unwrap_err();
                assert_eq!(error.code, code);
                assert_eq!(error.source, ErrorSource::Decode);
                assert_eq!(error.byte_offset, 97);
                assert_eq!((error.schema_id, error.field_id), (0, 0));
                assert!(!error.detail.is_empty());
            }
            assert_eq!(parse::parse_quoted(bytes, 97, &policy(2)).unwrap(), value);
        }
    }
}
