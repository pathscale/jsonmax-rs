//! Ownership, decoded storage, checked views, and retention of ABI buffers.
//!
//! The public contract is frozen in `contracts/jmax-buffer.md`. This crate never
//! issues validation proofs: checked storage access is not schema validation.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ops::Range;

pub use jmax_abi::buffer::{ParsedMessage, Payload, UnvalidatedSpan, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};

pub mod payload;
pub mod arena;
pub mod views;
pub mod retention;

/// Largest length or offset representable by the frozen ABI.
pub const MAX_BUFFER_BYTES: usize = u32::MAX as usize;
pub const DEFAULT_MAX_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_DECODED_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferLimits {
    pub max_payload_bytes: u32,
    pub max_decoded_bytes: u32,
}

impl Default for BufferLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_decoded_bytes: DEFAULT_MAX_DECODED_BYTES,
        }
    }
}

/// An append-only byte arena. Truncation explicitly invalidates removed spans.
#[derive(Clone, Debug)]
pub struct DecodedArena {
    pub(crate) bytes: Vec<u8>,
    pub(crate) limit: u32,
}

/// Borrowed storage from a single owner; references cannot outlive that owner.
#[derive(Clone, Copy, Debug)]
pub struct OwnerBytes<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) decoded: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteView<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) span: Span32,
    pub(crate) source: SpanSource,
}

impl<'a> ByteView<'a> {
    pub const fn as_bytes(&self) -> &'a [u8] { self.bytes }
    pub const fn span(&self) -> Span32 { self.span }
    pub const fn source(&self) -> SpanSource { self.source }
    pub const fn len(&self) -> usize { self.bytes.len() }
    pub const fn is_empty(&self) -> bool { self.bytes.is_empty() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrView<'a> {
    pub(crate) text: &'a str,
    pub(crate) reference: StrRef,
}

impl<'a> StrView<'a> {
    pub const fn as_str(&self) -> &'a str { self.text }
    pub const fn reference(&self) -> StrRef { self.reference }
    pub const fn len(&self) -> usize { self.text.len() }
    pub const fn is_empty(&self) -> bool { self.text.is_empty() }
}

/// `AtLeast` is necessary because a Bytes slice hides its backing allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackingSize {
    Exact(u64),
    AtLeast(u64),
}

impl BackingSize {
    pub const fn minimum(self) -> u64 {
        match self { Self::Exact(n) | Self::AtLeast(n) => n }
    }

    pub const fn exact(self) -> Option<u64> {
        match self { Self::Exact(n) => Some(n), Self::AtLeast(_) => None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionStats {
    pub payload_visible_bytes: u64,
    pub payload_backing_bytes: BackingSize,
    pub decoded_len: u64,
    pub decoded_capacity: u64,
}

impl RetentionStats {
    /// Saturation also makes manually constructed statistics safe to total.
    pub const fn minimum_retained_bytes(&self) -> u64 {
        self.payload_backing_bytes.minimum().saturating_add(self.decoded_capacity)
    }

    pub const fn exact_retained_bytes(&self) -> Option<u64> {
        match self.payload_backing_bytes.exact() {
            Some(n) => n.checked_add(self.decoded_capacity),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Budget for visible retained payload bytes, excluding allocation overhead.
    pub max_visible_bytes: u32,
    pub min_savings_bytes: u32,
    pub copy_unknown_backing: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_visible_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            min_savings_bytes: 4096,
            copy_unknown_backing: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RetainedRange {
    pub(crate) payload: Payload,
    pub(crate) original: Span32,
    pub(crate) retained: Span32,
    pub(crate) compacted: bool,
}

/// Constructs an ABI span without truncation or wrapping.
pub fn checked_span(offset: usize, len: usize, source: SpanSource) -> Result<Span32> {
    let code = match source {
        SpanSource::RawPayload => ErrorCode::PayloadTooLarge,
        SpanSource::DecodedArena => ErrorCode::DecodedArenaOverflow,
    };
    let failure = || Error::new(code, ErrorSource::Decode, u32::MAX, "buffer span exceeds ABI range");
    let offset = u32::try_from(offset).map_err(|_| failure())?;
    let len = u32::try_from(len).map_err(|_| failure())?;
    offset.checked_add(len).ok_or_else(failure)?;
    Ok(Span32::new(offset, len))
}

/// Checks arithmetic and bounds, including empty spans at the buffer end.
pub fn checked_range(span: Span32, buffer_len: usize) -> Result<Range<usize>> {
    let invalid = || Error::new(
        ErrorCode::InvalidToken, ErrorSource::Decode, span.offset,
        "buffer span is out of bounds",
    );
    let end = span.end().ok_or_else(invalid)? as usize;
    if end > buffer_len { return Err(invalid()); }
    Ok(span.offset as usize..end)
}
