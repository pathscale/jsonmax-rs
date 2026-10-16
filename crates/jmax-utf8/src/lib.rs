//! Safe UTF-8 validation, incremental state, and borrowed validation proofs.
//!
//! A proof borrows the exact immutable bytes it validates. It is distinct from
//! the ABI schema-validation proof and cannot authorize schema operations.
#![no_std]
#![forbid(unsafe_code)]

pub mod count;
pub mod validate;
pub use count::{count_continuation_bytes, count_scalars_trusted_utf8, validate_and_count_scalars};
pub mod fused;
pub mod proof;
pub mod stream;

#[cfg(feature = "alloc")]
pub use jmax_abi::buffer::Payload;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BlockMasks, ScanCarry};
pub use jmax_abi::span::Span32;

pub const MAX_SEQUENCE_BYTES: usize = 4;
pub const MAX_PENDING_BYTES: usize = MAX_SEQUENCE_BYTES - 1;
pub const BLOCK_BYTES: usize = 64;

/// Counts for a completely validated byte sequence. Scalars are Unicode scalar
/// values, with neither grapheme clustering nor normalization implied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utf8Summary {
    pub byte_len: u32,
    pub scalar_len: u32,
    pub ascii_only: bool,
}

impl Default for Utf8Summary {
    fn default() -> Self {
        Self {
            byte_len: 0,
            scalar_len: 0,
            ascii_only: true,
        }
    }
}

/// Incremental validator state. Only the UTF-8 fields of `carry` are used.
/// Failed and finished states reject further input until explicitly reset.
#[derive(Clone, Debug)]
pub struct Utf8Stream {
    carry: ScanCarry,
    total_bytes: u32,
    total_scalars: u32,
    ascii_only: bool,
    finished: bool,
    failure: Option<Error>,
}

impl Utf8Stream {
    pub fn new() -> Self {
        Self {
            carry: ScanCarry::default(),
            total_bytes: 0,
            total_scalars: 0,
            ascii_only: true,
            finished: false,
            failure: None,
        }
    }

    /// Reset counters, pending bytes, terminal status, and any saved error.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Bytes from successful chunks, including an incomplete terminal sequence.
    pub const fn bytes_seen(&self) -> u32 {
        self.total_bytes
    }

    /// Complete scalars from successful chunks, excluding the pending prefix.
    pub const fn scalars_seen(&self) -> u32 {
        self.total_scalars
    }

    pub fn pending_bytes(&self) -> &[u8] {
        &self.carry.utf8_tail[..usize::from(self.carry.utf8_tail_len)]
    }

    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn failure(&self) -> Option<&Error> {
        self.failure.as_ref()
    }
}

impl Default for Utf8Stream {
    fn default() -> Self {
        Self::new()
    }
}

/// Owner-bound UTF-8 evidence. Fields are private and there is no unchecked
/// public constructor. The borrowed `str` itself enforces UTF-8 validity.
#[derive(Clone, Copy, Debug)]
pub struct Utf8Proof<'a> {
    text: &'a str,
    summary: Utf8Summary,
}

impl<'a> Utf8Proof<'a> {
    pub const fn as_str(&self) -> &'a str {
        self.text
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        self.text.as_bytes()
    }

    pub const fn summary(&self) -> Utf8Summary {
        self.summary
    }
}

/// Shared bound for ABI-sized validation and absolute error positions.
pub(crate) fn checked_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Utf8,
            u32::MAX,
            "UTF-8 input exceeds the 32-bit ABI length limit",
        )
    })
}
