//! JSON string discovery, strict escape decoding, measurement, and output.
//!
//! Raw spans always refer to the caller's input. Decoded output is written to
//! an ABI byte sink; this crate never manufactures an owner-free string view.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod escape;
pub mod length;
pub mod masks;
pub mod scan;
pub mod unescape;
pub use masks::{
    EscapeMaskCursor, EscapeMasker, EscapeMasks, escape_mask_64, escape_masks_64,
    escape_or_control_mask_64, first_escape_or_control,
};
pub use unescape::{cook_into, decode_escaped_slice_into};

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::traits::ByteSink;

pub const QUOTE: u8 = b'"';
pub const BACKSLASH: u8 = b'\\';
pub const MAX_ESCAPE_BYTES: usize = 12;
pub const MAX_UTF8_SCALAR_BYTES: usize = 4;

/// Raw token boundaries. `raw` includes both quotes, `content` excludes them,
/// and `next_offset` is the exclusive end of `raw`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringScan {
    pub raw: Span32,
    pub content: Span32,
    pub next_offset: u32,
    pub first_escape: Option<u32>,
}

/// Length of fully decoded UTF-8, independent of JSON escape spelling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StringLengths {
    pub byte_len: u32,
    pub scalar_len: u32,
}

impl StringLengths {
    pub const fn new(byte_len: u32, scalar_len: u32) -> Self {
        Self {
            byte_len,
            scalar_len,
        }
    }

    /// Combine measurements without wrapping either ABI counter.
    pub fn checked_add(self, other: Self) -> Result<Self> {
        let byte_len = self.byte_len.checked_add(other.byte_len);
        let scalar_len = self.scalar_len.checked_add(other.scalar_len);
        match (byte_len, scalar_len) {
            (Some(byte_len), Some(scalar_len)) => Ok(Self {
                byte_len,
                scalar_len,
            }),
            _ => Err(Error::new(
                ErrorCode::DecodedArenaOverflow,
                ErrorSource::Decode,
                0,
                "decoded string length exceeds the 32-bit ABI limit",
            )),
        }
    }
}

/// One complete JSON escape, including a surrogate pair when required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedEscape {
    pub scalar: char,
    /// Source bytes consumed, including the initial backslash: 2, 6, or 12.
    pub consumed: u8,
}

/// Output spelling choices. Mandatory JSON escaping is always enforced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscapePolicy {
    pub escape_solidus: bool,
    pub ascii_only: bool,
}

/// Reject oversized inputs before narrowing indices to ABI offsets.
pub(crate) fn checked_input_len(input: &[u8]) -> Result<u32> {
    u32::try_from(input.len()).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            u32::MAX,
            "string input exceeds the 32-bit ABI length limit",
        )
    })
}

/// Checked source slicing shared by measurement and unescaping.
pub(crate) fn content_bytes(input: &[u8], content: Span32) -> Result<&[u8]> {
    checked_input_len(input)?;
    if content.end().is_none() {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            content.offset,
            "string content span overflows its ABI offset",
        ));
    }
    content.slice(input).ok_or_else(|| {
        Error::new(
            ErrorCode::UnexpectedEof,
            ErrorSource::Decode,
            content.offset,
            "string content span is outside the input",
        )
    })
}
