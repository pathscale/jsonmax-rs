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
