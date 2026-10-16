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
