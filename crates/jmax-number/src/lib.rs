//! Strict JSON numeric tokens, checked conversion, and bounded formatting.
//!
//! Lexed tokens borrow the original input and can only be constructed inside
//! this crate. All cross-crate values and errors use the canonical ABI types.
#![no_std]
#![forbid(unsafe_code)]

pub mod lex;
pub mod integer;
pub mod float;
pub mod write;

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{ConvertedNumber, NumericPolicy};
pub use jmax_abi::span::{NumberLexKind, Span32};
pub use jmax_abi::traits::ByteSink;

pub const MAX_U64_TEXT_BYTES: usize = 20;
pub const MAX_I64_TEXT_BYTES: usize = 20;
/// Enough for finite binary64 fixed or scientific decimal output, including
/// sign, decimal point, and exponent, with no allocation or truncation.
pub const MAX_F64_TEXT_BYTES: usize = 768;

/// A complete, grammar-checked token. `bytes` excludes optional wrapper quotes;
/// `span` includes them. No public constructor can bypass lexical validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumberToken<'a> {
    bytes: &'a [u8],
    span: Span32,
    kind: NumberLexKind,
    negative: bool,
    integer_digits: u32,
    fraction_digits: u32,
    exponent: i32,
}

impl<'a> NumberToken<'a> {
    pub const fn raw_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub const fn span(&self) -> Span32 {
        self.span
    }

    pub const fn kind(&self) -> NumberLexKind {
        self.kind
    }

    pub const fn is_negative(&self) -> bool {
        self.negative
    }

    pub const fn integer_digits(&self) -> u32 {
        self.integer_digits
    }

    pub const fn fraction_digits(&self) -> u32 {
        self.fraction_digits
    }

    /// Signed explicit exponent, or zero when there is no exponent part.
    pub const fn exponent(&self) -> i32 {
        self.exponent
    }

    pub const fn is_quoted(&self) -> bool {
        matches!(self.kind, NumberLexKind::QuotedDecimal)
    }
}

/// Stack storage for a complete numeric spelling before sink publication.
/// It implements `core::fmt::Write` without allocation. Capacity exhaustion
/// returns `fmt::Error` and leaves the previously stored bytes unchanged.
#[derive(Clone, Debug)]
pub struct NumberText {
    bytes: [u8; MAX_F64_TEXT_BYTES],
    len: usize,
}

impl NumberText {
    pub const fn new() -> Self {
        Self { bytes: [0; MAX_F64_TEXT_BYTES], len: 0 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for NumberText {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Write for NumberText {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(core::fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        destination.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Strict JSON value terminators. A colon or opening container is not a
/// delimiter for a number; token boundaries cannot hide an invalid suffix.
pub const fn is_number_delimiter(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}')
}

pub(crate) fn checked_input_len(input: &[u8]) -> Result<u32> {
    u32::try_from(input.len()).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Lexer,
            u32::MAX,
            "number input exceeds the 32-bit ABI length limit",
        )
    })
}
