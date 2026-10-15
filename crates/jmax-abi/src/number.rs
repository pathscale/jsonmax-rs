//! Shared numeric representations and conversion policies.

/// Policy applied when converting JSON number lexemes.
#[allow(non_snake_case)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumericPolicy {
    /// Whether an integer may be read from a JSON string.
    pub allow_Quoted_int: bool,
    pub reject_non_finite_float: bool,
    pub reject_negative_zero: bool,
    /// Inclusive limit on the absolute value of an explicit exponent.
    pub max_exponent_abs: i16,
}

impl NumericPolicy {
    /// Reject quoted integers, non-finite floats, and negative zero.
    ///
    /// Explicit exponents are limited to an absolute value of 308. This
    /// syntactic limit does not replace overflow checks during conversion.
    pub const fn strict_default() -> Self {
        Self {
            allow_Quoted_int: false,
            reject_non_finite_float: true,
            reject_negative_zero: true,
            max_exponent_abs: 308,
        }
    }
}

/// An exact decimal value equal to `coefficient * 10^(-scale)`.
///
/// Equality and hashing compare the representation, including its scale.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Decimal128 {
    pub coefficient: i128,
    pub scale: u8,
}

impl Decimal128 {
    /// Construct a representation without rounding or normalization.
    pub const fn new(coefficient: i128, scale: u8) -> Self {
        Self { coefficient, scale }
    }

    pub const fn is_zero(&self) -> bool {
        self.coefficient == 0
    }
}

/// Constraints for converting and rescaling an exact decimal.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecimalPolicy {
    pub max_precision: u8,
    pub target_scale: u8,
    pub allow_exponent: bool,
    pub reject_inexact_rescale: bool,
    /// Tick size in coefficient units at `target_scale`.
    pub tick_step: i128,
}

impl DecimalPolicy {
    /// A 38-digit, eight-place policy with exact rescaling and unit ticks.
    ///
    /// Exponent notation is disabled. One tick represents `0.00000001`.
    pub const fn spot_8() -> Self {
        Self {
            max_precision: 38,
            target_scale: 8,
            allow_exponent: false,
            reject_inexact_rescale: true,
            tick_step: 1,
        }
    }
}

/// A converted numeric value; floating-point values retain their exact bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvertedNumber {
    U64(u64),
    I64(i64),
    F64Bits(u64),
    Decimal(Decimal128),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_policy_is_const_and_explicit() {
        const POLICY: NumericPolicy = NumericPolicy::strict_default();
        assert_eq!(POLICY, NumericPolicy {
            allow_Quoted_int: false,
            reject_non_finite_float: true,
            reject_negative_zero: true,
            max_exponent_abs: 308,
        });
    }

    #[test]
    fn spot_policy_is_const_and_exact() {
        const POLICY: DecimalPolicy = DecimalPolicy::spot_8();
        assert_eq!(POLICY, DecimalPolicy {
            max_precision: 38,
            target_scale: 8,
            allow_exponent: false,
            reject_inexact_rescale: true,
            tick_step: 1,
        });
    }

    #[test]
    fn decimal_preserves_full_coefficient_and_scale_ranges() {
        const ZERO: Decimal128 = Decimal128::new(0, u8::MAX);
        const IS_ZERO: bool = ZERO.is_zero();
        assert!(IS_ZERO);
        assert_eq!(Decimal128::default(), Decimal128::new(0, 0));
        for coefficient in [i128::MIN, -1, 0, 1, i128::MAX] {
            for scale in [0, 8, u8::MAX] {
                let value = Decimal128::new(coefficient, scale);
                assert_eq!(value.coefficient, coefficient);
                assert_eq!(value.scale, scale);
                assert_eq!(value.is_zero(), coefficient == 0);
            }
        }
        assert_ne!(Decimal128::new(10, 1), Decimal128::new(100, 2));
        assert_ne!(Decimal128::new(0, 0), ZERO);
    }

    #[test]
    fn converted_numbers_preserve_variants_and_float_bits() {
        assert_ne!(ConvertedNumber::U64(1), ConvertedNumber::I64(1));
        assert_eq!(ConvertedNumber::U64(u64::MAX), ConvertedNumber::U64(u64::MAX));
        assert_eq!(ConvertedNumber::I64(i64::MIN), ConvertedNumber::I64(i64::MIN));
        let decimal = Decimal128::new(i128::MIN, 8);
        assert_eq!(ConvertedNumber::Decimal(decimal), ConvertedNumber::Decimal(decimal));
        for bits in [0, 1 << 63, 0x7ff0_0000_0000_0000, 0x7ff8_0000_0000_0042] {
            let value = ConvertedNumber::F64Bits(bits);
            assert_eq!(value, value);
            if let ConvertedNumber::F64Bits(stored) = value {
                assert_eq!(f64::from_bits(stored).to_bits(), bits);
            } else {
                panic!("floating-point variant changed");
            }
        }
        assert_ne!(ConvertedNumber::F64Bits(0), ConvertedNumber::F64Bits(1 << 63));
    }
}
