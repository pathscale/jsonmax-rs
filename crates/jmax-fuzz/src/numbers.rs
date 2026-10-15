//! Deterministic adversarial corpus and differential harness for numeric conversion.

use alloc::{string::String, vec::Vec};

use crate::{
    harness_error, ConvertedNumber, ErrorCode, Failure, FailureKind, FuzzConfig,
    FuzzReport, NumberCase, NumberConfig, NumberMode, NumberOracle, NumberOutcome,
    RejectionComparison, Result,
};

/// Builds a deterministic mix of boundary, grammar, IEEE-754, and decimal cases.
/// Cases that exceed the byte budget are omitted intact.
pub fn adversarial_corpus(config: NumberConfig, fuzz: FuzzConfig) -> Result<Vec<NumberCase>> {
    validate_number_config(config)?;
    fuzz.validate()?;

    let mut corpus = Vec::new();
    let candidates: &[(&str, &[u8], NumberMode, Option<NumberOutcome>)] = &[
        // Integer endpoints and their adjacent overflow values.
        ("u64-zero", b"0", NumberMode::Unsigned, Some(NumberOutcome::Value(ConvertedNumber::U64(0)))),
        ("u64-maximum", b"18446744073709551615", NumberMode::Unsigned, Some(NumberOutcome::Value(ConvertedNumber::U64(u64::MAX)))),
        ("u64-overflow-neighbor", b"18446744073709551616", NumberMode::Unsigned, Some(NumberOutcome::Rejected(ErrorCode::NumberOverflow))),
        ("u64-negative", b"-1", NumberMode::Unsigned, None),
        ("i64-maximum", b"9223372036854775807", NumberMode::Signed, Some(NumberOutcome::Value(ConvertedNumber::I64(i64::MAX)))),
        ("i64-overflow-neighbor", b"9223372036854775808", NumberMode::Signed, Some(NumberOutcome::Rejected(ErrorCode::NumberOverflow))),
        ("i64-minimum", b"-9223372036854775808", NumberMode::Signed, Some(NumberOutcome::Value(ConvertedNumber::I64(i64::MIN)))),
        ("i64-underflow-neighbor", b"-9223372036854775809", NumberMode::Signed, Some(NumberOutcome::Rejected(ErrorCode::NumberOverflow))),
        ("unsigned-leading-zero", b"01", NumberMode::Unsigned, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),
        ("signed-leading-plus", b"+1", NumberMode::Signed, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),
        ("signed-negative-zero", b"-0", NumberMode::Signed, None),
        ("fraction-to-integer", b"1.0", NumberMode::Signed, None),
        ("exponent-to-integer", b"1e0", NumberMode::Signed, None),
        ("empty-fraction", b"1.", NumberMode::Float, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),
        ("empty-exponent", b"1e", NumberMode::Float, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),
        ("leading-decimal-point", b".1", NumberMode::Float, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),
        ("bare-plus-invalid-number", b"+", NumberMode::Float, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),
        ("repeated-sign", b"--1", NumberMode::Float, Some(NumberOutcome::Rejected(ErrorCode::InvalidNumber))),

        // Stable IEEE-754 rounding anchors. Values are stored as bits so signed
        // zero and subnormal distinctions remain visible to comparisons.
        ("float-zero", b"0", NumberMode::Float, Some(NumberOutcome::Value(ConvertedNumber::F64Bits(0x0000_0000_0000_0000)))),
        ("float-one", b"1", NumberMode::Float, Some(NumberOutcome::Value(ConvertedNumber::F64Bits(0x3ff0_0000_0000_0000)))),
        ("float-halfway-even-one", b"1.00000000000000011102230246251565404236316680908203125", NumberMode::Float, Some(NumberOutcome::Value(ConvertedNumber::F64Bits(0x3ff0_0000_0000_0000)))),
        ("float-halfway-even-two", b"1.00000000000000033306690738754696212708950042724609375", NumberMode::Float, Some(NumberOutcome::Value(ConvertedNumber::F64Bits(0x3ff0_0000_0000_0002)))),
        ("float-2pow53", b"9007199254740992", NumberMode::Float, Some(NumberOutcome::Value(ConvertedNumber::F64Bits(0x4340_0000_0000_0000)))),
        ("float-2pow53-plus-one-rounds-even", b"9007199254740993", NumberMode::Float, Some(NumberOutcome::Value(ConvertedNumber::F64Bits(0x4340_0000_0000_0000)))),
        ("float-smallest-subnormal", b"5e-324", NumberMode::Float, None),
        ("float-half-smallest-subnormal", b"2.4703282292062327e-324", NumberMode::Float, None),
        ("float-subnormal-normal-boundary", b"2.2250738585072014e-308", NumberMode::Float, None),
        ("float-max-finite", b"1.7976931348623157e308", NumberMode::Float, None),
        ("float-overflow", b"1e309", NumberMode::Float, None),
        ("float-negative-zero", b"-0.0", NumberMode::Float, None),
        ("float-exponent-at-policy-limit", b"1e308", NumberMode::Float, None),
        ("float-exponent-over-policy-limit", b"1e309", NumberMode::Float, None),

        // Quoted decimal lexemes contain the JSON quotes expected by the oracle.
        // Their result depends on the caller's precision, scale, exponent, and
        // tick policy, so they are intentionally differential-only.
        ("decimal-zero", b"\"0\"", NumberMode::Decimal, None),
        ("decimal-negative-zero", b"\"-0\"", NumberMode::Decimal, None),
        ("decimal-negative-coefficient", b"\"-12.3400\"", NumberMode::Decimal, None),
        ("decimal-i128-near-maximum", b"\"170141183460469231731687303715884105727\"", NumberMode::Decimal, None),
        ("decimal-i128-overflow", b"\"170141183460469231731687303715884105728\"", NumberMode::Decimal, None),
        ("decimal-precision-overflow", b"\"123456789012345678901234567890123456789\"", NumberMode::Decimal, None),
        ("decimal-scale-extension", b"\"1.2\"", NumberMode::Decimal, None),
        ("decimal-scale-reduction-exact", b"\"1.2300000000\"", NumberMode::Decimal, None),
        ("decimal-scale-reduction-inexact", b"\"1.2300000001\"", NumberMode::Decimal, None),
        ("decimal-exponent-allowed-or-rejected", b"\"1.25e+2\"", NumberMode::Decimal, None),
        ("decimal-exponent-leading-zeroes", b"\"1e0002\"", NumberMode::Decimal, None),
        ("decimal-quoted-leading-zero", b"\"01.2\"", NumberMode::Decimal, None),
        ("decimal-quoted-leading-plus", b"\"+1\"", NumberMode::Decimal, None),
        ("decimal-quoted-empty-fraction", b"\"1.\"", NumberMode::Decimal, None),
        ("decimal-overlong-digit-run", b"\"12345678901234567890123456789012345678901234567890\"", NumberMode::Decimal, None),
        ("decimal-tick-multiple", b"\"1.25\"", NumberMode::Decimal, None),
        ("decimal-tick-nonmultiple", b"\"1.26\"", NumberMode::Decimal, None),
    ];

    let mut known_accepted = false;
    let mut known_rejected = false;
    for (name, lexeme, mode, expected) in candidates {
        if lexeme.len() > fuzz.max_input_bytes || corpus.len() >= fuzz.max_cases as usize {
            continue;
        }
        let mut name_owned = String::new();
        name_owned.try_reserve_exact(name.len()).map_err(|_| {
            harness_error(ErrorCode::BudgetExhausted, "number corpus allocation failed")
        })?;
        name_owned.push_str(name);
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(lexeme.len()).map_err(|_| {
            harness_error(ErrorCode::BudgetExhausted, "number corpus allocation failed")
        })?;
        bytes.extend_from_slice(lexeme);
        corpus.try_reserve(1).map_err(|_| {
            harness_error(ErrorCode::BudgetExhausted, "number corpus allocation failed")
        })?;
        if let Some(answer) = expected {
            known_accepted |= matches!(answer, NumberOutcome::Value(_));
            known_rejected |= matches!(answer, NumberOutcome::Rejected(_));
        }
        corpus.push(NumberCase { name: name_owned, lexeme: bytes, mode: *mode, expected: expected.clone() });
    }

    // Derive tick vectors from the actual policy so a non-unit tick exercises
    // both divisibility outcomes whenever the corresponding coefficients fit.
    let tick = config.decimal.tick_step;
    if corpus.len() < fuzz.max_cases as usize {
        let bytes = quoted_scaled_coefficient(tick, config.decimal.target_scale)?;
        if bytes.len() <= fuzz.max_input_bytes {
            corpus.try_reserve(1).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "number corpus allocation failed")
            })?;
            corpus.push(NumberCase {
                name: String::from("decimal-configured-tick-multiple"),
                lexeme: bytes,
                mode: NumberMode::Decimal,
                expected: None,
            });
        }
    }
    if tick > 1 && corpus.len() < fuzz.max_cases as usize {
        let bytes = quoted_scaled_coefficient(tick - 1, config.decimal.target_scale)?;
        if bytes.len() <= fuzz.max_input_bytes {
            corpus.try_reserve(1).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "number corpus allocation failed")
            })?;
            corpus.push(NumberCase {
                name: String::from("decimal-configured-tick-nonmultiple"),
                lexeme: bytes,
                mode: NumberMode::Decimal,
                expected: None,
            });
        }
    }
    if !known_accepted || !known_rejected {
        return Err(harness_error(ErrorCode::ConstraintViolation, "number corpus budget cannot retain known answers"));
    }
    Ok(corpus)
}

/// Runs two numeric adapters and checks differential and known-answer results.
pub fn run_numbers(
    cases: &[NumberCase],
    left: &mut dyn NumberOracle,
    right: &mut dyn NumberOracle,
    number: NumberConfig,
    fuzz: FuzzConfig,
) -> Result<FuzzReport> {
    validate_number_config(number)?;
    fuzz.validate()?;
    if cases.is_empty() {
        return Err(harness_error(ErrorCode::ConstraintViolation, "number run requires cases"));
    }
    for case in cases {
        if case.lexeme.len() > fuzz.max_input_bytes {
            return Err(harness_error(ErrorCode::PayloadTooLarge, "number case exceeds byte cap"));
        }
    }

    let target = core::cmp::min(cases.len(), fuzz.max_cases as usize);
    let mut report = FuzzReport::default();
    for (case_index, case) in cases.iter().take(target).enumerate() {
        if report.failures.len() >= fuzz.max_failures as usize {
            report.truncated = true;
            break;
        }
        let left_outcome = left.observe(&case.lexeme, case.mode, number)?;
        let right_outcome = right.observe(&case.lexeme, case.mode, number)?;
        report.attempted += 1;

        let mut mismatch = left_outcome != right_outcome;
        if let (NumberOutcome::Rejected(left_code), NumberOutcome::Rejected(right_code)) = (&left_outcome, &right_outcome) {
            if fuzz.rejection_comparison == RejectionComparison::AcceptanceOnly {
                mismatch = false;
            } else {
                mismatch = left_code != right_code;
            }
        }
        mismatch |= invalid_number_output(&left_outcome, case.mode, number);
        mismatch |= invalid_number_output(&right_outcome, case.mode, number);
        if let Some(expected) = &case.expected {
            mismatch |= !matches_expected(&left_outcome, expected, true);
            mismatch |= !matches_expected(&right_outcome, expected, true);
        }

        if mismatch {
            let mut input = Vec::new();
            input.try_reserve_exact(case.lexeme.len()).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "number failure input allocation failed")
            })?;
            input.extend_from_slice(&case.lexeme);
            let mut detail = String::new();
            detail.try_reserve_exact(320).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "number failure detail allocation failed")
            })?;
            detail.push_str("number oracle mismatch: ");
            for character in left.name().chars().take(32) {
                detail.push(character);
            }
            detail.push_str(" vs ");
            for character in right.name().chars().take(32) {
                detail.push(character);
            }
            report.failures.try_reserve(1).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "number report allocation failed")
            })?;
            report.record_failure(Failure {
                case_index: case_index as u64,
                seed: fuzz.seed,
                kind: FailureKind::Numeric,
                input,
                detail,
            }, fuzz.max_failures);
        } else {
            report.agreed += 1;
        }
    }
    if report.attempted == 0 {
        return Err(harness_error(ErrorCode::ConstraintViolation, "number run attempted no cases"));
    }
    if cases.len() > target || (report.attempted < target as u64 && report.failures.len() >= fuzz.max_failures as usize) {
        report.truncated = true;
    }
    Ok(report)
}

fn validate_number_config(config: NumberConfig) -> Result<()> {
    if config.numeric.max_exponent_abs < 0
        || config.decimal.max_precision == 0
        || config.decimal.max_precision > 38
        || config.decimal.target_scale > 38
        || config.decimal.target_scale > config.decimal.max_precision
        || config.decimal.tick_step <= 0
    {
        return Err(harness_error(ErrorCode::ConstraintViolation, "invalid number policy"));
    }
    Ok(())
}

fn quoted_scaled_coefficient(coefficient: i128, scale: u8) -> Result<Vec<u8>> {
    let mut digit_storage = [0u8; 39];
    let mut magnitude = coefficient as u128;
    let mut first_digit = digit_storage.len();
    loop {
        first_digit -= 1;
        digit_storage[first_digit] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    let digits = &digit_storage[first_digit..];
    let scale = scale as usize;
    let integer_digits = digits.len().saturating_sub(scale);
    let leading_fractional_zeroes = scale.saturating_sub(digits.len());
    let capacity = digits.len().checked_add(scale).and_then(|v| v.checked_add(4)).ok_or_else(|| {
        harness_error(ErrorCode::BudgetExhausted, "decimal tick vector length overflow")
    })?;
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|_| {
        harness_error(ErrorCode::BudgetExhausted, "decimal tick vector allocation failed")
    })?;
    output.push(b'"');
    if scale == 0 {
        output.extend_from_slice(digits);
    } else {
        if integer_digits == 0 {
            output.push(b'0');
        } else {
            output.extend_from_slice(&digits[..integer_digits]);
        }
        output.push(b'.');
        output.extend(core::iter::repeat(b'0').take(leading_fractional_zeroes));
        output.extend_from_slice(&digits[integer_digits..]);
    }
    output.push(b'"');
    Ok(output)
}

fn invalid_number_output(outcome: &NumberOutcome, mode: NumberMode, config: NumberConfig) -> bool {
    match outcome {
        NumberOutcome::Rejected(_) => false,
        NumberOutcome::Value(value) => match (mode, value) {
            (NumberMode::Unsigned, ConvertedNumber::U64(_)) => false,
            (NumberMode::Signed, ConvertedNumber::I64(_)) => false,
            (NumberMode::Float, ConvertedNumber::F64Bits(bits)) => {
                config.numeric.reject_non_finite_float && (bits & 0x7ff0_0000_0000_0000) == 0x7ff0_0000_0000_0000
            }
            (NumberMode::Decimal, ConvertedNumber::Decimal(value)) => value.scale != config.decimal.target_scale,
            _ => true,
        },
    }
}

fn matches_expected(actual: &NumberOutcome, expected: &NumberOutcome, exact_rejection: bool) -> bool {
    match (actual, expected) {
        (NumberOutcome::Value(a), NumberOutcome::Value(e)) => a == e,
        (NumberOutcome::Rejected(a), NumberOutcome::Rejected(e)) => !exact_rejection || a == e,
        _ => false,
    }
}
