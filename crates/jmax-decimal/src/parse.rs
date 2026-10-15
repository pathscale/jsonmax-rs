//! Checked, allocation-free accumulation and quoted-decimal conversion.

use super::*;

/// Concatenate the integer and fractional digits into a bounded coefficient.
///
/// The lexer's checked span proves that digit lengths and absolute offsets fit
/// in `u32`. Leading zeros do not contribute to precision, but every fractional
/// digit contributes to the source scale. The exponent is preserved unchanged.
/// Coefficient overflow is reported at the first unrepresentable digit, before
/// any rescaling can discard digits.
pub fn accumulate(lexeme: &DecimalLexeme<'_>) -> Result<AccumulatedDecimal> {
    // Quotes and an optional minus precede the integer. The point, when
    // present, occupies exactly one byte between the two digit slices.
    let integer_offset = lexeme.span().offset + 1 + u32::from(lexeme.is_negative());
    let fraction_offset = integer_offset + lexeme.integer_digits().len() as u32 + 1;
    let coefficient = accumulate_digits(
        0,
        lexeme.integer_digits(),
        lexeme.is_negative(),
        integer_offset,
    )?;
    let coefficient = accumulate_digits(
        coefficient,
        lexeme.fractional_digits(),
        lexeme.is_negative(),
        fraction_offset,
    )?;

    Ok(AccumulatedDecimal {
        coefficient,
        fractional_digits: lexeme.fractional_digits().len() as u32,
        exponent: lexeme.exponent(),
        precision: coefficient_precision(coefficient),
    })
}

/// Parse one complete quoted token under the supplied decimal policy.
///
/// Configuration is validated before inspecting the input. Lexing,
/// accumulation, rescaling, and final validation preserve their original error
/// metadata. The returned representation has exactly `policy.target_scale`.
pub fn parse_quoted(
    input: &[u8],
    offset: u32,
    policy: &DecimalPolicy,
) -> Result<Decimal128> {
    validate_policy(policy)?;
    let lexeme = lex::lex_quoted(input, offset, policy.allow_exponent)?;
    let accumulated = accumulate(&lexeme)?;
    let value = bounds::rescale(accumulated, policy, offset)?;
    bounds::validate(value, policy, DecimalBounds::default(), offset)?;
    Ok(value)
}

// Each slice contains only ASCII digits, as established by the lexer. Applying
// the lexical sign at every step avoids ever constructing the positive signed
// magnitude of i128::MIN. Chunks restart at the point; the accumulated
// coefficient does not.
fn accumulate_digits(
    mut coefficient: i128,
    digits: &[u8],
    negative: bool,
    mut offset: u32,
) -> Result<i128> {
    for chunk in digits.chunks(CHUNK_DIGITS as usize) {
        let mut magnitude = 0_u32;
        let mut base = 1_u32;
        for &digit in chunk {
            // At most nine digits: magnitude <= CHUNK_BASE - 1 and
            // base <= CHUNK_BASE. Both intermediates fit in u32.
            magnitude = magnitude * 10 + u32::from(digit - b'0');
            base *= 10;
        }
        let signed_chunk = if negative {
            -i128::from(magnitude)
        } else {
            i128::from(magnitude)
        };
        if let Some(next) = coefficient
            .checked_mul(i128::from(base))
            .and_then(|value| value.checked_add(signed_chunk))
        {
            coefficient = next;
        } else {
            // Prefix magnitudes never decrease. Replay only a failing chunk
            // to report the earliest digit that exceeds the signed range.
            for (index, &digit) in chunk.iter().enumerate() {
                let digit = i128::from(digit - b'0');
                let signed_digit = if negative { -digit } else { digit };
                coefficient = coefficient
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(signed_digit))
                    .ok_or_else(|| {
                        decimal_error(
                            ErrorCode::NumberOverflow,
                            offset + index as u32,
                            "decimal coefficient exceeds i128 range",
                        )
                    })?;
            }
        }
        // The lexer's validated absolute end bounds this addition and every
        // narrowing conversion above, including the end of the final chunk.
        offset += chunk.len() as u32;
    }
    Ok(coefficient)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(scale: u8) -> DecimalPolicy {
        DecimalPolicy {
            max_precision: MAX_PRECISION,
            target_scale: scale,
            allow_exponent: true,
            reject_inexact_rescale: true,
            tick_step: 0,
        }
    }

    fn accumulated(input: &[u8]) -> AccumulatedDecimal {
        accumulate(&lex::lex_quoted(input, 73, true).unwrap()).unwrap()
    }

    fn assert_error(error: &Error, code: ErrorCode, source: ErrorSource, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn accumulation_preserves_fraction_count_exponent_and_sign() {
        let vectors: &[(&[u8], i128, u32, i32, u8)] = &[
            (b"\"0\"", 0, 0, 0, 1),
            (b"\"-0\"", 0, 0, 0, 1),
            (b"\"0.0000000000\"", 0, 10, 0, 1),
            (b"\"-0.000E-32767\"", 0, 3, -32767, 1),
            (b"\"1e32767\"", 1, 0, 32767, 1),
            (b"\"0.000001\"", 1, 6, 0, 1),
            (b"\"-0.00100\"", -100, 5, 0, 3),
            (b"\"12.003400E+0002\"", 12003400, 6, 2, 8),
            (b"\"-123456789.0123456789e-4\"", -1234567890123456789, 10, -4, 19),
            (b"\"999999999\"", 999999999, 0, 0, 9),
            (b"\"1000000000\"", 1000000000, 0, 0, 10),
            (b"\"1000000000000000000\"", 1000000000000000000, 0, 0, 19),
        ];
        for &(input, coefficient, fractional_digits, exponent, precision) in vectors {
            assert_eq!(
                accumulated(input),
                AccumulatedDecimal {
                    coefficient,
                    fractional_digits,
                    exponent,
                    precision,
                },
                "input: {:?}",
                input,
            );
        }
    }

    #[test]
    fn signed_endpoints_and_their_neighbors_are_exact() {
        let vectors: &[(&[u8], i128)] = &[
            (b"\"170141183460469231731687303715884105726\"", i128::MAX - 1),
            (b"\"170141183460469231731687303715884105727\"", i128::MAX),
            (b"\"-170141183460469231731687303715884105727\"", -i128::MAX),
            (b"\"-170141183460469231731687303715884105728\"", i128::MIN),
            (b"\"17014118346046923173.1687303715884105727\"", i128::MAX),
            (b"\"-17014118346046923173.1687303715884105728\"", i128::MIN),
            (b"\"0.170141183460469231731687303715884105727\"", i128::MAX),
            (b"\"-0.170141183460469231731687303715884105728\"", i128::MIN),
        ];
        for &(input, coefficient) in vectors {
            let value = accumulated(input);
            assert_eq!(value.coefficient, coefficient, "input: {:?}", input);
            assert_eq!(value.precision, 39);
        }
    }

    #[test]
    fn every_partial_chunk_length_and_point_position_preserves_value() {
        let digits = b"123456789012345678901234567890123456789";
        let mut expected = 0_i128;
        for length in 1..=digits.len() {
            expected = expected * 10 + i128::from(digits[length - 1] - b'0');
            for negative in [false, true] {
                for point in 1..=length {
                    let mut input = [0_u8; 44];
                    input[0] = b'"';
                    let mut cursor = 1;
                    if negative {
                        input[cursor] = b'-';
                        cursor += 1;
                    }
                    input[cursor..cursor + point].copy_from_slice(&digits[..point]);
                    cursor += point;
                    if point < length {
                        input[cursor] = b'.';
                        cursor += 1;
                        input[cursor..cursor + length - point]
                            .copy_from_slice(&digits[point..length]);
                        cursor += length - point;
                    }
                    input[cursor] = b'"';
                    let value = accumulated(&input[..=cursor]);
                    assert_eq!(value.coefficient, if negative { -expected } else { expected });
                    assert_eq!(value.fractional_digits, (length - point) as u32);
                    assert_eq!(value.precision, length as u8);
                }
            }
        }
    }

    #[test]
    fn long_zero_prefixes_do_not_consume_precision_or_limit_fraction_length() {
        let mut input = [b'0'; 4096];
        input[0] = b'"';
        input[1] = b'-';
        input[2] = b'0';
        input[3] = b'.';
        input[4095] = b'"';
        let zero = accumulated(&input);
        assert_eq!(zero.coefficient, 0);
        assert_eq!(zero.precision, 1);
        assert_eq!(zero.fractional_digits, 4091);
        input[4092..4095].copy_from_slice(b"100");
        let value = accumulated(&input);
        assert_eq!(value.coefficient, -100);
        assert_eq!(value.precision, 3);
        assert_eq!(value.fractional_digits, 4091);
    }

    #[test]
    fn successful_accumulation_accepts_the_last_representable_span_end() {
        let inputs: &[&[u8]] = &[
            b"\"0\"",
            b"\"-0\"",
            b"\"123.004500\"",
            b"\"-0.000001e-32767\"",
            b"\"170141183460469231731687303715884105727\"",
            b"\"-170141183460469231731687303715884105728\"",
        ];
        for &input in inputs {
            let offset = u32::MAX - input.len() as u32;
            let lexeme = lex::lex_quoted(input, offset, true).unwrap();
            assert_eq!(lexeme.span().end(), Some(u32::MAX));
            assert_eq!(accumulate(&lexeme).unwrap(), accumulated(input));
        }
    }

    #[test]
    fn overflow_reports_the_first_digit_including_sign_and_point_offsets() {
        let vectors: &[(&[u8], u32)] = &[
            (b"\"170141183460469231731687303715884105728\"", 39),
            (b"\"-170141183460469231731687303715884105729\"", 40),
            (b"\"999999999999999999999999999999999999999999999\"", 39),
            (b"\"-999999999999999999999999999999999999999999999\"", 40),
            (b"\"1701411834604692317316873037158841057270\"", 40),
            (b"\"-1701411834604692317316873037158841057280\"", 41),
            (b"\"17014118346046923173168730371588410572.8\"", 40),
            (b"\"-17014118346046923173168730371588410572.9\"", 41),
            (b"\"170141183460469231731687303715884105727.0e-32767\"", 41),
            (b"\"-170141183460469231731687303715884105728.0e-32767\"", 42),
        ];
        for &(input, relative_offset) in vectors {
            for offset in [0, 193, u32::MAX - input.len() as u32] {
                let lexeme = lex::lex_quoted(input, offset, true).unwrap();
                let error = accumulate(&lexeme).unwrap_err();
                assert_error(
                    &error,
                    ErrorCode::NumberOverflow,
                    ErrorSource::Decode,
                    offset + relative_offset,
                );
            }
        }
    }

    #[test]
    fn failing_chunks_locate_overflow_at_every_digit_position() {
        for negative in [false, true] {
            let magnitude: &[u8] = if negative {
                b"170141183460469231731687303715884105729"
            } else {
                b"170141183460469231731687303715884105728"
            };
            for leading_zeros in 0..CHUNK_DIGITS as usize {
                let mut input = [b'0'; 64];
                input[0] = b'"';
                let mut cursor = 1;
                if negative {
                    input[cursor] = b'-';
                    cursor += 1;
                }
                input[cursor] = b'0';
                input[cursor + 1] = b'.';
                cursor += 2 + leading_zeros;
                input[cursor..cursor + magnitude.len()].copy_from_slice(magnitude);
                let offending = cursor + magnitude.len() - 1;
                cursor += magnitude.len() + CHUNK_DIGITS as usize;
                input[cursor] = b'"';
                let lexeme = lex::lex_quoted(&input[..=cursor], 211, false).unwrap();
                assert_error(
                    &accumulate(&lexeme).unwrap_err(),
                    ErrorCode::NumberOverflow,
                    ErrorSource::Decode,
                    211 + offending as u32,
                );
            }
        }
    }

    #[test]
    fn parse_returns_the_requested_scale_and_applies_exponents() {
        let vectors: &[(&[u8], u8, i128)] = &[
            (b"\"0\"", 0, 0),
            (b"\"-0.000\"", 8, 0),
            (b"\"1\"", 8, 100000000),
            (b"\"-12.34\"", 4, -123400),
            (b"\"12.3400\"", 2, 1234),
            (b"\"0.000001e6\"", 0, 1),
            (b"\"-12.30e2\"", 0, -1230),
            (b"\"12e-2\"", 4, 1200),
            (b"\"100e-2\"", 0, 1),
            (b"\"0e32767\"", 255, 0),
            (b"\"-0e-32767\"", 255, 0),
            (b"\"170141183460469231731687303715884105727\"", 0, i128::MAX),
            (b"\"-170141183460469231731687303715884105728\"", 0, i128::MIN),
        ];
        for &(input, scale, coefficient) in vectors {
            assert_eq!(
                parse_quoted(input, 31, &policy(scale)).unwrap(),
                Decimal128::new(coefficient, scale),
                "input: {:?}",
                input,
            );
        }
    }

    #[test]
    fn policy_errors_precede_lexing_and_report_configuration_offset() {
        for (precision, tick) in [(0, 0), (40, 0), (255, 0), (39, -1), (39, i128::MIN)] {
            let mut policy = policy(0);
            policy.max_precision = precision;
            policy.tick_step = tick;
            let expected = validate_policy(&policy).unwrap_err();
            for input in [&b""[..], &b"not a decimal"[..], &b"\"1\""[..]] {
                let error = parse_quoted(input, u32::MAX, &policy).unwrap_err();
                assert_eq!(error, expected);
                assert_error(&error, ErrorCode::ConstraintViolation, ErrorSource::Decode, 0);
            }
        }
    }

    #[test]
    fn lexical_errors_are_propagated_without_rewriting_metadata() {
        let mut policy = policy(0);
        for allow_exponent in [false, true] {
            policy.allow_exponent = allow_exponent;
            for input in [
                &b"\""[..],
                &b"\"01\""[..],
                &b"\"1e32768\""[..],
                &b"\"1\"extra"[..],
                &b"\"999999999999999999999999999999999999999x\""[..],
            ] {
                let expected = lex::lex_quoted(input, 107, allow_exponent).unwrap_err();
                assert_eq!(parse_quoted(input, 107, &policy).unwrap_err(), expected);
            }
        }
        policy.allow_exponent = false;
        let error = parse_quoted(b"\"1e0\"", 107, &policy).unwrap_err();
        assert_error(&error, ErrorCode::InvalidNumber, ErrorSource::Lexer, 109);
        let error = parse_quoted(b"\"0\"", u32::MAX - 2, &policy).unwrap_err();
        assert_error(&error, ErrorCode::PayloadTooLarge, ErrorSource::Lexer, u32::MAX - 2);
    }

    #[test]
    fn raw_coefficient_overflow_precedes_rescale_precision_and_tick_checks() {
        let input = b"\"170141183460469231731687303715884105728e-32767\"";
        let mut policy = policy(0);
        policy.max_precision = 1;
        policy.reject_inexact_rescale = false;
        policy.tick_step = 7;
        let lexeme = lex::lex_quoted(input, 61, true).unwrap();
        let expected = accumulate(&lexeme).unwrap_err();
        let error = parse_quoted(input, 61, &policy).unwrap_err();
        assert_eq!(error, expected);
        assert_error(&error, ErrorCode::NumberOverflow, ErrorSource::Decode, 100);
    }

    #[test]
    fn final_precision_is_checked_after_rescaling_and_ignores_leading_zeros() {
        let mut policy = policy(2);
        policy.max_precision = 1;
        for input in [&b"\"0.0000000100e6\""[..], &b"\"0.010000\""[..]] {
            assert_eq!(
                parse_quoted(input, 0, &policy).unwrap(),
                Decimal128::new(1, 2),
            );
        }
        assert_eq!(
            parse_quoted(b"\"-0.00000\"", 0, &policy).unwrap(),
            Decimal128::new(0, 2),
        );
        assert_error(
            &parse_quoted(b"\"1\"", 97, &policy).unwrap_err(),
            ErrorCode::DecimalPrecisionOverflow,
            ErrorSource::Decode,
            97,
        );
        policy.max_precision = 3;
        assert_eq!(
            parse_quoted(b"\"1\"", 0, &policy).unwrap(),
            Decimal128::new(100, 2),
        );
    }

    #[test]
    fn downstream_errors_propagate_unchanged_and_respect_validation_order() {
        let mut precision_policy = policy(2);
        precision_policy.max_precision = 2;
        precision_policy.tick_step = 7;
        let mut tick_policy = policy(2);
        tick_policy.tick_step = 5;
        let cases: &[(&[u8], DecimalPolicy, ErrorCode)] = &[
            (b"\"1.001\"", policy(2), ErrorCode::DecimalScaleMismatch),
            (b"\"1\"", policy(255), ErrorCode::NumberOverflow),
            (b"\"1e32767\"", policy(0), ErrorCode::NumberOverflow),
            (b"\"1e-32767\"", policy(0), ErrorCode::DecimalScaleMismatch),
            (b"\"1\"", precision_policy, ErrorCode::DecimalPrecisionOverflow),
            (b"\"1.01\"", tick_policy, ErrorCode::TickMultipleViolation),
            (b"\"-1.01\"", tick_policy, ErrorCode::TickMultipleViolation),
        ];
        for &(input, policy, code) in cases {
            let value = accumulated(input);
            let expected = match bounds::rescale(value, &policy, 149) {
                Err(error) => error,
                Ok(value) => bounds::validate(value, &policy, DecimalBounds::default(), 149)
                    .unwrap_err(),
            };
            let error = parse_quoted(input, 149, &policy).unwrap_err();
            assert_eq!(error, expected);
            assert_error(&error, code, ErrorSource::Decode, 149);
        }
        assert_eq!(
            parse_quoted(b"\"-1.05\"", 0, &tick_policy).unwrap(),
            Decimal128::new(-105, 2),
        );
    }

    #[test]
    fn optional_truncation_is_toward_zero_and_precedes_tick_validation() {
        let mut policy = policy(2);
        policy.reject_inexact_rescale = false;
        policy.tick_step = 5;
        for (input, coefficient) in [(&b"\"1.059\""[..], 105), (&b"\"-1.059\""[..], -105)] {
            assert_eq!(
                parse_quoted(input, 0, &policy).unwrap(),
                Decimal128::new(coefficient, 2),
            );
        }
        policy.target_scale = 0;
        policy.max_precision = 1;
        policy.tick_step = 0;
        for input in [&b"\"1e-32767\""[..], &b"\"-1e-32767\""[..]] {
            assert_eq!(parse_quoted(input, 0, &policy).unwrap(), Decimal128::new(0, 0));
        }
    }
}
