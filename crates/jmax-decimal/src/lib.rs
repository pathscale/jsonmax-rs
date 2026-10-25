//! Exact fixed-point decimal contracts shared by parsing and writing.
#![no_std]
#![forbid(unsafe_code)]

pub mod lex;
pub mod parse;
pub mod bounds;
pub mod write;

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{Decimal128, DecimalPolicy};
pub use jmax_abi::span::Span32;
pub use jmax_abi::traits::ByteSink;

/// Maximum number of significant base-ten digits in an i128 coefficient.
pub const MAX_PRECISION: u8 = 39;
/// Absolute exponent limit accepted by the quoted grammar.
pub const MAX_EXPONENT_ABS: i32 = 32_767;
/// Space for any quoted Decimal128, including sign and scale 255.
pub const MAX_DECIMAL_TEXT_LEN: usize = 260;
pub const CHUNK_DIGITS: u32 = 9;
pub const CHUNK_BASE: u32 = 1_000_000_000;

/// A borrowed, fully checked lexical decomposition. Spans include both quotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecimalLexeme<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) span: Span32,
    pub(crate) negative: bool,
    pub(crate) integer: &'a [u8],
    pub(crate) fraction: &'a [u8],
    pub(crate) exponent: i32,
}

impl<'a> DecimalLexeme<'a> {
    pub const fn raw(&self) -> &'a [u8] { self.raw }
    pub const fn span(&self) -> Span32 { self.span }
    pub const fn is_negative(&self) -> bool { self.negative }
    pub const fn integer_digits(&self) -> &'a [u8] { self.integer }
    pub const fn fractional_digits(&self) -> &'a [u8] { self.fraction }
    pub const fn exponent(&self) -> i32 { self.exponent }
}

/// Inclusive bounds, interpreted numerically even when their scales differ.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecimalBounds {
    pub min: Option<Decimal128>,
    pub max: Option<Decimal128>,
}

/// Unscaled coefficient plus its lexical fractional scale and exponent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccumulatedDecimal {
    pub coefficient: i128,
    pub fractional_digits: u32,
    pub exponent: i32,
    /// Significant coefficient digits, with zero counted as one digit.
    pub precision: u8,
}

/// Reject malformed configuration before looking at input or mutating a sink.
pub fn validate_policy(policy: &DecimalPolicy) -> Result<()> {
    if policy.max_precision == 0 || policy.max_precision > MAX_PRECISION {
        return Err(decimal_error(ErrorCode::ConstraintViolation, 0,
            "decimal precision must be in 1..=39"));
    }
    if policy.tick_step < 0 {
        return Err(decimal_error(ErrorCode::ConstraintViolation, 0,
            "decimal tick step must be nonnegative"));
    }
    Ok(())
}

/// Decimal digit count without overflowing on i128::MIN.
pub fn coefficient_precision(coefficient: i128) -> u8 {
    let mut magnitude = coefficient.unsigned_abs();
    let mut digits = 1;
    while magnitude >= 10 {
        magnitude /= 10;
        digits += 1;
    }
    digits
}

pub(crate) const fn decimal_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, offset, detail)
}
