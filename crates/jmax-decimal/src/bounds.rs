//! Exact, allocation-free decimal rescaling and constraint validation.

use super::*;
use core::cmp::Ordering;

/// Convert an accumulated coefficient to exactly `policy.target_scale`.
///
/// Explicit exponents require policy permission, including for zero. Increasing
/// coefficient magnitude uses checked arithmetic. Decreasing magnitude either
/// requires exact division or truncates toward zero, according to the policy.
/// Precision is recomputed after rescaling; `value.precision` is not trusted.
/// Range and tick constraints are left to [`validate`].
///
/// Configuration errors have offset zero. All other errors have the supplied
/// `offset`, with source [`ErrorSource::Decode`].
pub fn rescale(
    value: AccumulatedDecimal,
    policy: &DecimalPolicy,
    offset: u32,
) -> Result<Decimal128> {
    validate_policy(policy)?;
    if value.exponent != 0 && !policy.allow_exponent {
        return Err(decimal_error(
            ErrorCode::ConstraintViolation,
            offset,
            "decimal exponent is disabled",
        ));
    }

    // Every combination of u32 fractional digits, i32 exponent and u8 target
    // scale fits in i64, including manually constructed accumulators.
    let source_scale = i64::from(value.fractional_digits) - i64::from(value.exponent);
    let mut shift = i64::from(policy.target_scale) - source_scale;
    let mut coefficient = value.coefficient;
    while shift > 0 && coefficient != 0 {
        // A nonzero i128 can grow by fewer than 39 decimal places before
        // overflowing, so even extreme exponents cause only bounded work.
        coefficient = coefficient.checked_mul(10).ok_or_else(|| {
            decimal_error(
                ErrorCode::NumberOverflow,
                offset,
                "rescaled decimal coefficient exceeds i128 range",
            )
        })?;
        shift -= 1;
    }
    while shift < 0 && coefficient != 0 {
        if policy.reject_inexact_rescale && coefficient % 10 != 0 {
            return Err(decimal_error(
                ErrorCode::DecimalScaleMismatch,
                offset,
                "decimal rescaling would discard nonzero digits",
            ));
        }
        // Signed division by positive ten truncates toward zero and is safe
        // even for i128::MIN. At most 39 divisions reach zero; no power of ten
        // needs to be representable for an arbitrarily large downward shift.
        coefficient /= 10;
        shift += 1;
    }

    validate_precision(coefficient, policy.max_precision, offset)?;
    Ok(Decimal128::new(coefficient, policy.target_scale))
}

/// Validate target scale, coefficient precision, inclusive bounds and tick.
///
/// Bounds are compared numerically even when their representations have
/// different scales. Reversed bounds fail even if the value would otherwise
/// pass. After policy validation, errors are selected in scale, precision,
/// range, then tick order. A zero tick disables divisibility validation.
pub fn validate(
    value: Decimal128,
    policy: &DecimalPolicy,
    bounds: DecimalBounds,
    offset: u32,
) -> Result<()> {
    validate_policy(policy)?;
    if value.scale != policy.target_scale {
        return Err(decimal_error(
            ErrorCode::DecimalScaleMismatch,
            offset,
            "decimal scale differs from target scale",
        ));
    }
    validate_precision(value.coefficient, policy.max_precision, offset)?;

    if let (Some(minimum), Some(maximum)) = (bounds.min, bounds.max) {
        if compare(minimum, maximum) == Ordering::Greater {
            return Err(decimal_error(
                ErrorCode::ConstraintViolation,
                offset,
                "decimal minimum exceeds maximum",
            ));
        }
    }
    if let Some(minimum) = bounds.min {
        if compare(value, minimum) == Ordering::Less {
            return Err(decimal_error(
                ErrorCode::ConstraintViolation,
                offset,
                "decimal value is below minimum",
            ));
        }
    }
    if let Some(maximum) = bounds.max {
        if compare(value, maximum) == Ordering::Greater {
            return Err(decimal_error(
                ErrorCode::ConstraintViolation,
                offset,
                "decimal value exceeds maximum",
            ));
        }
    }

    // Policy validation guarantees a strictly positive divisor here, excluding
    // both division by zero and the signed MIN % -1 overflow case.
    if policy.tick_step > 0 && value.coefficient % policy.tick_step != 0 {
        return Err(decimal_error(
            ErrorCode::TickMultipleViolation,
            offset,
            "decimal coefficient is not a tick multiple",
        ));
    }
    Ok(())
}

/// Compare mathematical decimal values across the complete coefficient/scale domain.
///
/// Signs are compared first. Unsigned magnitudes include `i128::MIN` exactly.
/// When scales differ, division aligns the finer-scale magnitude with the
/// coarser one. A nonzero remainder breaks a quotient tie, preserving exactness
/// without multiplication, rounding, allocation or an overflowing power of ten.
pub fn compare(left: Decimal128, right: Decimal128) -> Ordering {
    let sign_order = left.coefficient.signum().cmp(&right.coefficient.signum());
    if sign_order != Ordering::Equal {
        return sign_order;
    }
    if left.coefficient == 0 {
        return Ordering::Equal;
    }

    let left_magnitude = left.coefficient.unsigned_abs();
    let right_magnitude = right.coefficient.unsigned_abs();
    let magnitude_order = match left.scale.cmp(&right.scale) {
        Ordering::Equal => left_magnitude.cmp(&right_magnitude),
        Ordering::Greater => compare_finer_magnitude(
            left_magnitude,
            right_magnitude,
            left.scale - right.scale,
        ),
        Ordering::Less => compare_finer_magnitude(
            right_magnitude,
            left_magnitude,
            right.scale - left.scale,
        )
        .reverse(),
    };
    if left.coefficient < 0 {
        magnitude_order.reverse()
    } else {
        magnitude_order
    }
}

fn validate_precision(coefficient: i128, maximum: u8, offset: u32) -> Result<()> {
    if coefficient_precision(coefficient) > maximum {
        return Err(decimal_error(
            ErrorCode::DecimalPrecisionOverflow,
            offset,
            "decimal coefficient exceeds precision limit",
        ));
    }
    Ok(())
}

// Compare finer / 10^places with coarser. Dropping digits individually bounds
// the loop by the magnitude's (at most 39) digits, even for a scale gap of 255.
// The sticky remainder records every discarded digit, including those dropped
// before the quotient becomes zero.
fn compare_finer_magnitude(mut finer: u128, coarser: u128, mut places: u8) -> Ordering {
    let mut remainder_nonzero = false;
    while places != 0 && finer != 0 {
        remainder_nonzero |= finer % 10 != 0;
        finer /= 10;
        places -= 1;
    }
    match finer.cmp(&coarser) {
        Ordering::Equal => remainder_nonzero.cmp(&false),
        order => order,
    }
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

    fn accumulated(coefficient: i128, fractional_digits: u32, exponent: i32) -> AccumulatedDecimal {
        AccumulatedDecimal {
            coefficient,
            fractional_digits,
            exponent,
            precision: coefficient_precision(coefficient),
        }
    }

    fn assert_error(error: Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn assert_order(left: Decimal128, right: Decimal128, expected: Ordering) {
        assert_eq!(compare(left, right), expected, "left: {:?}, right: {:?}", left, right);
        assert_eq!(compare(right, left), expected.reverse());
    }

    #[test]
    fn exact_rescale_vectors_preserve_value_and_target_scale() {
        let vectors = [
            (1234, 2, 0, 2, 1234),
            (1234, 2, 0, 4, 123400),
            (-1234, 2, 0, 4, -123400),
            (123400, 4, 0, 2, 1234),
            (-123400, 4, 0, 2, -1234),
            (1230, 2, 2, 0, 1230),
            (1230, 2, -1, 4, 12300),
            (1, 6, 6, 0, 1),
            (100, 0, -2, 0, 1),
            (1, 0, 2, 0, 100),
            (1, 255, 0, 255, 1),
            (-100, 257, 0, 255, -1),
        ];
        for (coefficient, fraction, exponent, scale, expected) in vectors {
            assert_eq!(
                rescale(accumulated(coefficient, fraction, exponent), &policy(scale), 73).unwrap(),
                Decimal128::new(expected, scale),
            );
        }
    }

    #[test]
    fn extreme_accumulator_scales_are_computed_without_wrapping() {
        for coefficient in [i128::MIN, -1, 1, i128::MAX] {
            for exponent in [i32::MIN, -32767, 0, 32767, i32::MAX] {
                let value = accumulated(coefficient, u32::MAX, exponent);
                assert_error(
                    rescale(value, &policy(255), u32::MAX).unwrap_err(),
                    ErrorCode::DecimalScaleMismatch,
                    u32::MAX,
                );
                let mut truncating = policy(255);
                truncating.reject_inexact_rescale = false;
                assert_eq!(rescale(value, &truncating, 19).unwrap(), Decimal128::new(0, 255));
            }
            assert_error(
                rescale(accumulated(coefficient, 0, i32::MAX), &policy(0), 91).unwrap_err(),
                ErrorCode::NumberOverflow,
                91,
            );
        }
        // i32::MIN cannot be negated in i32, but is valid in the wide scale computation.
        let mut truncating = policy(0);
        truncating.reject_inexact_rescale = false;
        assert_eq!(
            rescale(accumulated(1, 0, i32::MIN), &truncating, 0).unwrap(),
            Decimal128::new(0, 0),
        );
    }

    #[test]
    fn zero_rescales_at_every_target_scale_with_extreme_source_scales() {
        for scale in 0..=u8::MAX {
            let mut policy = policy(scale);
            policy.max_precision = 1;
            for fraction in [0, 255, u32::MAX] {
                for exponent in [i32::MIN, 0, i32::MAX] {
                    let mut value = accumulated(0, fraction, exponent);
                    value.precision = u8::MAX;
                    assert_eq!(rescale(value, &policy, 21).unwrap(), Decimal128::new(0, scale));
                }
            }
        }
    }

    #[test]
    fn disabled_exponents_reject_manual_accumulators_including_zero() {
        let mut policy = policy(2);
        policy.allow_exponent = false;
        for coefficient in [0, 1, -1, i128::MIN, i128::MAX] {
            for exponent in [i32::MIN, -1, 1, i32::MAX] {
                assert_error(
                    rescale(accumulated(coefficient, 2, exponent), &policy, 47).unwrap_err(),
                    ErrorCode::ConstraintViolation,
                    47,
                );
            }
            assert_eq!(
                rescale(accumulated(coefficient, 2, 0), &policy, 47).unwrap(),
                Decimal128::new(coefficient, 2),
            );
        }
    }

    #[test]
    fn multiplication_accepts_the_last_safe_step_and_rejects_the_next() {
        for coefficient in [i128::MIN / 10, i128::MAX / 10] {
            assert_eq!(
                rescale(accumulated(coefficient, 0, 0), &policy(1), 89).unwrap(),
                Decimal128::new(coefficient * 10, 1),
            );
        }
        for coefficient in [i128::MIN / 10 - 1, i128::MAX / 10 + 1, i128::MIN, i128::MAX] {
            assert_error(
                rescale(accumulated(coefficient, 0, 0), &policy(1), 89).unwrap_err(),
                ErrorCode::NumberOverflow,
                89,
            );
        }
        let ten_to_38 = 10_i128.pow(38);
        for sign in [-1, 1] {
            assert_eq!(
                rescale(accumulated(sign, 0, 0), &policy(38), 0).unwrap(),
                Decimal128::new(sign * ten_to_38, 38),
            );
            for scale in [39, 255] {
                assert_error(
                    rescale(accumulated(sign, 0, 0), &policy(scale), 89).unwrap_err(),
                    ErrorCode::NumberOverflow,
                    89,
                );
            }
        }
    }

    #[test]
    fn endpoint_coefficients_survive_identity_and_truncate_toward_zero() {
        for coefficient in [i128::MIN, i128::MIN + 1, i128::MAX - 1, i128::MAX] {
            for scale in [0, 38, 255] {
                assert_eq!(
                    rescale(accumulated(coefficient, u32::from(scale), 0), &policy(scale), 0).unwrap(),
                    Decimal128::new(coefficient, scale),
                );
            }
            let mut truncating = policy(0);
            truncating.reject_inexact_rescale = false;
            assert_eq!(
                rescale(accumulated(coefficient, 1, 0), &truncating, 0).unwrap(),
                Decimal128::new(coefficient / 10, 0),
            );
        }
    }

    #[test]
    fn inexact_rescale_detects_nonzero_digits_at_every_discard_position() {
        for places in 1..=38_u32 {
            let power = 10_i128.pow(places);
            for sign in [-1, 1] {
                assert_eq!(
                    rescale(accumulated(sign * power, places, 0), &policy(0), 0).unwrap(),
                    Decimal128::new(sign, 0),
                );
                for coefficient in [sign * (power - 1), sign * (power + 1)] {
                    assert_error(
                        rescale(accumulated(coefficient, places, 0), &policy(0), 113).unwrap_err(),
                        ErrorCode::DecimalScaleMismatch,
                        113,
                    );
                    let mut truncating = policy(0);
                    truncating.reject_inexact_rescale = false;
                    assert_eq!(
                        rescale(accumulated(coefficient, places, 0), &truncating, 113).unwrap(),
                        Decimal128::new(coefficient / power, 0),
                    );
                }
            }
        }
        for coefficient in [-1000, 1000] {
            assert_error(
                rescale(accumulated(coefficient, 4, 0), &policy(0), 113).unwrap_err(),
                ErrorCode::DecimalScaleMismatch,
                113,
            );
        }
    }

    #[test]
    fn huge_downward_shifts_reject_inexact_or_truncate_to_zero() {
        for places in [39, 255, 32767, u32::MAX] {
            for coefficient in [i128::MIN, -1, 1, i128::MAX] {
                let value = accumulated(coefficient, places, 0);
                assert_error(
                    rescale(value, &policy(0), 131).unwrap_err(),
                    ErrorCode::DecimalScaleMismatch,
                    131,
                );
                let mut truncating = policy(0);
                truncating.max_precision = 1;
                truncating.reject_inexact_rescale = false;
                assert_eq!(rescale(value, &truncating, 131).unwrap(), Decimal128::new(0, 0));
            }
        }
    }

    #[test]
    fn rescale_precision_uses_final_coefficient_and_ignores_descriptive_field() {
        let mut policy = policy(0);
        policy.max_precision = 1;
        let mut value = accumulated(1000, 3, 0);
        value.precision = u8::MAX;
        assert_eq!(rescale(value, &policy, 0).unwrap(), Decimal128::new(1, 0));
        value.coefficient = 1200;
        assert_error(
            rescale(value, &policy, 151).unwrap_err(),
            ErrorCode::DecimalScaleMismatch,
            151,
        );
        policy.reject_inexact_rescale = false;
        assert_eq!(rescale(value, &policy, 0).unwrap(), Decimal128::new(1, 0));
        value = accumulated(10, 0, 0);
        value.precision = 0;
        assert_error(
            rescale(value, &policy, 151).unwrap_err(),
            ErrorCode::DecimalPrecisionOverflow,
            151,
        );
        policy.target_scale = 2;
        policy.max_precision = 2;
        assert_error(
            rescale(accumulated(1, 0, 0), &policy, 151).unwrap_err(),
            ErrorCode::DecimalPrecisionOverflow,
            151,
        );
        policy.max_precision = 3;
        assert_eq!(
            rescale(accumulated(1, 0, 0), &policy, 0).unwrap(),
            Decimal128::new(100, 2),
        );
    }

    #[test]
    fn rescale_leaves_tick_validation_to_validate() {
        let mut policy = policy(2);
        policy.tick_step = 5;
        let value = rescale(accumulated(101, 2, 0), &policy, 0).unwrap();
        assert_eq!(value, Decimal128::new(101, 2));
        assert_error(
            validate(value, &policy, DecimalBounds::default(), 163).unwrap_err(),
            ErrorCode::TickMultipleViolation,
            163,
        );
    }

    #[test]
    fn invalid_policies_precede_data_errors_and_preserve_configuration_metadata() {
        for (precision, tick) in [(0, 0), (40, 0), (255, 0), (39, -1), (39, i128::MIN)] {
            let mut policy = policy(255);
            policy.max_precision = precision;
            policy.tick_step = tick;
            policy.allow_exponent = false;
            let expected = validate_policy(&policy).unwrap_err();
            let error = rescale(accumulated(i128::MAX, 0, i32::MAX), &policy, u32::MAX)
                .unwrap_err();
            assert_eq!(error, expected);
            assert_error(error, ErrorCode::ConstraintViolation, 0);
            let error = validate(
                Decimal128::new(i128::MIN, 0),
                &policy,
                DecimalBounds { min: Some(Decimal128::new(1, 0)), max: Some(Decimal128::new(-1, 0)) },
                u32::MAX,
            ).unwrap_err();
            assert_eq!(error, expected);
            assert_error(error, ErrorCode::ConstraintViolation, 0);
        }
    }

    #[test]
    fn validation_enforces_scale_then_precision_then_range_then_tick() {
        let mut policy = policy(2);
        policy.max_precision = 2;
        policy.tick_step = 7;
        let reversed = DecimalBounds {
            min: Some(Decimal128::new(2, 0)),
            max: Some(Decimal128::new(1, 0)),
        };
        for (value, bounds, code) in [
            (Decimal128::new(123, 1), reversed, ErrorCode::DecimalScaleMismatch),
            (Decimal128::new(123, 2), reversed, ErrorCode::DecimalPrecisionOverflow),
            (Decimal128::new(13, 2), reversed, ErrorCode::ConstraintViolation),
            (Decimal128::new(13, 2), DecimalBounds::default(), ErrorCode::TickMultipleViolation),
        ] {
            assert_error(validate(value, &policy, bounds, 179).unwrap_err(), code, 179);
        }
        let below = DecimalBounds { min: Some(Decimal128::new(1, 0)), max: None };
        let above = DecimalBounds { min: None, max: Some(Decimal128::new(1, 3)) };
        for bounds in [below, above] {
            assert_error(
                validate(Decimal128::new(13, 2), &policy, bounds, 179).unwrap_err(),
                ErrorCode::ConstraintViolation,
                179,
            );
        }
    }

    #[test]
    fn precision_accepts_each_boundary_and_rejects_the_next_digit() {
        for precision in 1..=38_u8 {
            let mut policy = policy(255);
            policy.max_precision = precision;
            let power = 10_i128.pow(u32::from(precision));
            for coefficient in [0, power - 1, -(power - 1)] {
                assert_eq!(
                    validate(Decimal128::new(coefficient, 255), &policy, DecimalBounds::default(), 0),
                    Ok(()),
                );
            }
            for coefficient in [power, -power] {
                assert_error(
                    validate(Decimal128::new(coefficient, 255), &policy, DecimalBounds::default(), 191)
                        .unwrap_err(),
                    ErrorCode::DecimalPrecisionOverflow,
                    191,
                );
            }
        }
        for coefficient in [i128::MIN, i128::MAX] {
            assert_eq!(
                validate(Decimal128::new(coefficient, 255), &policy(255), DecimalBounds::default(), 0),
                Ok(()),
            );
        }
    }

    #[test]
    fn inclusive_bounds_accept_endpoints_and_reject_adjacent_values() {
        let bounds = DecimalBounds {
            min: Some(Decimal128::new(-125, 2)),
            max: Some(Decimal128::new(25000, 4)),
        };
        for coefficient in [-1250, -1249, 0, 2499, 2500] {
            assert_eq!(validate(Decimal128::new(coefficient, 3), &policy(3), bounds, 0), Ok(()));
        }
        for coefficient in [-1251, 2501] {
            assert_error(
                validate(Decimal128::new(coefficient, 3), &policy(3), bounds, 211).unwrap_err(),
                ErrorCode::ConstraintViolation,
                211,
            );
        }
        for coefficient in [i128::MIN, 0, i128::MAX] {
            let value = Decimal128::new(coefficient, 255);
            assert_eq!(
                validate(value, &policy(255), DecimalBounds { min: Some(value), max: Some(value) }, 0),
                Ok(()),
            );
        }
    }

    #[test]
    fn equal_numeric_bounds_with_different_scales_form_a_singleton() {
        let bounds = DecimalBounds {
            min: Some(Decimal128::new(10, 1)),
            max: Some(Decimal128::new(1000, 3)),
        };
        assert_eq!(validate(Decimal128::new(100, 2), &policy(2), bounds, 0), Ok(()));
        for coefficient in [99, 101] {
            assert_error(
                validate(Decimal128::new(coefficient, 2), &policy(2), bounds, 223).unwrap_err(),
                ErrorCode::ConstraintViolation,
                223,
            );
        }
        let reversed = DecimalBounds {
            min: Some(Decimal128::new(1, 254)),
            max: Some(Decimal128::new(9, 255)),
        };
        assert_error(
            validate(Decimal128::new(0, 255), &policy(255), reversed, 223).unwrap_err(),
            ErrorCode::ConstraintViolation,
            223,
        );
    }

    #[test]
    fn one_sided_bounds_and_extreme_scale_gaps_are_exact() {
        for (bounds, accepted, rejected) in [
            (DecimalBounds { min: Some(Decimal128::new(1, 255)), max: None }, 1, 0),
            (DecimalBounds { min: None, max: Some(Decimal128::new(-1, 255)) }, -1, 0),
            (DecimalBounds { min: Some(Decimal128::new(i128::MIN, 255)), max: None }, 0, -1),
            (DecimalBounds { min: None, max: Some(Decimal128::new(i128::MAX, 255)) }, 0, 1),
        ] {
            assert_eq!(validate(Decimal128::new(accepted, 0), &policy(0), bounds, 0), Ok(()));
            assert_error(
                validate(Decimal128::new(rejected, 0), &policy(0), bounds, 227).unwrap_err(),
                ErrorCode::ConstraintViolation,
                227,
            );
        }
    }

    #[test]
    fn tick_checks_cover_both_signs_zero_and_largest_positive_divisor() {
        let mut policy = policy(2);
        policy.tick_step = 5;
        for coefficient in [-105, -5, 0, 5, 105] {
            assert_eq!(validate(Decimal128::new(coefficient, 2), &policy, DecimalBounds::default(), 0), Ok(()));
        }
        for coefficient in [-106, -104, -1, 1, 104, 106] {
            assert_error(
                validate(Decimal128::new(coefficient, 2), &policy, DecimalBounds::default(), 229)
                    .unwrap_err(),
                ErrorCode::TickMultipleViolation,
                229,
            );
        }
        policy.tick_step = i128::MAX;
        for coefficient in [-i128::MAX, 0, i128::MAX] {
            assert_eq!(validate(Decimal128::new(coefficient, 2), &policy, DecimalBounds::default(), 0), Ok(()));
        }
        assert_error(
            validate(Decimal128::new(i128::MIN, 2), &policy, DecimalBounds::default(), 229).unwrap_err(),
            ErrorCode::TickMultipleViolation,
            229,
        );
        for tick in [0, 1, 2] {
            policy.tick_step = tick;
            assert_eq!(
                validate(Decimal128::new(i128::MIN, 2), &policy, DecimalBounds::default(), 0),
                Ok(()),
            );
        }
    }

    #[test]
    fn compare_vectors_cover_sign_scale_endpoints_and_sticky_remainders() {
        let vectors = [
            (0, 0, 0, 255, Ordering::Equal),
            (-1, 255, 0, 0, Ordering::Less),
            (1, 255, 0, 0, Ordering::Greater),
            (-1, 0, 1, 255, Ordering::Less),
            (10, 1, 1, 0, Ordering::Equal),
            (-1000, 3, -10, 1, Ordering::Equal),
            (1001, 3, 1, 0, Ordering::Greater),
            (1010, 3, 1, 0, Ordering::Greater),
            (999, 3, 1, 0, Ordering::Less),
            (-1001, 3, -1, 0, Ordering::Less),
            (-999, 3, -1, 0, Ordering::Greater),
            (1, 255, 1, 0, Ordering::Less),
            (i128::MAX, 255, 1, 0, Ordering::Less),
            (i128::MIN, 255, -1, 0, Ordering::Greater),
            (i128::MIN, 0, -i128::MAX, 0, Ordering::Less),
            (i128::MAX, 1, i128::MAX / 10, 0, Ordering::Greater),
            (i128::MIN, 1, i128::MIN / 10, 0, Ordering::Less),
            (i128::MAX, 38, 1, 0, Ordering::Greater),
            (i128::MAX, 39, 1, 0, Ordering::Less),
            (i128::MIN, 38, -1, 0, Ordering::Less),
            (i128::MIN, 39, -1, 0, Ordering::Greater),
        ];
        for (left, left_scale, right, right_scale, expected) in vectors {
            assert_order(Decimal128::new(left, left_scale), Decimal128::new(right, right_scale), expected);
        }
    }

    #[test]
    fn compare_agrees_with_independent_bounded_cross_products() {
        let coefficients = [-999_i128, -101, -100, -10, -1, 0, 1, 10, 100, 101, 999];
        for left in coefficients {
            for right in coefficients {
                for left_scale in 0..=6_u8 {
                    for right_scale in 0..=6_u8 {
                        let expected = (left * 10_i128.pow(u32::from(right_scale)))
                            .cmp(&(right * 10_i128.pow(u32::from(left_scale))));
                        assert_order(
                            Decimal128::new(left, left_scale),
                            Decimal128::new(right, right_scale),
                            expected,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn compare_preserves_identity_and_scaled_equalities_at_every_scale() {
        for scale in 0..=u8::MAX {
            for coefficient in [i128::MIN, -100, -1, 0, 1, 100, i128::MAX] {
                let value = Decimal128::new(coefficient, scale);
                assert_order(value, value, Ordering::Equal);
                assert_order(value, Decimal128::new(0, 255 - scale), coefficient.cmp(&0));
            }
            if scale < u8::MAX {
                for coefficient in [i128::MIN / 10, -1, 0, 1, i128::MAX / 10] {
                    assert_order(
                        Decimal128::new(coefficient, scale),
                        Decimal128::new(coefficient * 10, scale + 1),
                        Ordering::Equal,
                    );
                }
            }
        }
    }

    #[test]
    fn compare_handles_every_scale_pair_for_endpoint_magnitudes() {
        for left_scale in 0..=u8::MAX {
            for right_scale in 0..=u8::MAX {
                assert_order(
                    Decimal128::new(i128::MAX, left_scale),
                    Decimal128::new(i128::MAX, right_scale),
                    right_scale.cmp(&left_scale),
                );
                assert_order(
                    Decimal128::new(i128::MIN, left_scale),
                    Decimal128::new(i128::MIN, right_scale),
                    left_scale.cmp(&right_scale),
                );
                assert_order(
                    Decimal128::new(0, left_scale),
                    Decimal128::new(0, right_scale),
                    Ordering::Equal,
                );
            }
        }
    }
}
