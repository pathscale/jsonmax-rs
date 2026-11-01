//! Bounded, fallibly reserved storage for decoded bytes.
//!
//! The limit applies to logical length, not allocation capacity. Byte appends
//! accept arbitrary data; only `append_str` supplies UTF-8 scalar metadata.
//! Truncating or clearing the arena invalidates references to removed bytes.

use super::{
    checked_span, Box, DecodedArena, Error, ErrorCode, ErrorSource, Result, Span32,
    SpanSource, StrRef, Vec, DEFAULT_MAX_DECODED_BYTES,
};

impl DecodedArena {
    /// Creates an empty arena without allocating.
    pub fn new(max_decoded_bytes: u32) -> Self {
        Self { bytes: Vec::new(), limit: max_decoded_bytes }
    }

    /// Reserves initial storage without adding logical bytes.
    ///
    /// The requested capacity must fit the logical limit. Allocator rounding
    /// may leave more capacity than requested, including more than the limit.
    pub fn with_capacity(capacity: usize, max_decoded_bytes: u32) -> Result<Self> {
        let mut arena = Self::new(max_decoded_bytes);
        arena.reserve(capacity)?;
        Ok(arena)
    }

    /// Returns the number of initialized bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the arena has no logical bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the allocation capacity, which may exceed the logical limit.
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Returns the maximum permitted logical byte length.
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Borrows all logical bytes, excluding spare capacity.
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Reserves space for at least `additional` bytes beyond the current length.
    ///
    /// Checks the ABI range and logical limit even when spare capacity already
    /// suffices. Errors preserve logical bytes and length.
    pub fn reserve(&mut self, additional: usize) -> Result<()> {
        self.reserve_region(additional).map(|_| ())
    }

    /// Appends bytes and returns their coordinates in this arena.
    ///
    /// No UTF-8 or JSON validation is performed. An empty append returns a
    /// zero-length span at the current end, including at the logical limit.
    pub fn append(&mut self, bytes: &[u8]) -> Result<Span32> {
        let span = self.reserve_region(bytes.len())?;
        // Reservation already guarantees capacity for the entire copy, so this
        // extension cannot introduce a second allocation after error handling.
        self.bytes.extend_from_slice(bytes);
        Ok(span)
    }

    /// Appends UTF-8 text with its checked Unicode scalar count.
    ///
    /// Scalar counting completes before any mutation. Other arena regions may
    /// contain arbitrary bytes, so the whole arena need not be valid UTF-8.
    pub fn append_str(&mut self, text: &str) -> Result<StrRef> {
        let scalar_len = u32::try_from(text.chars().count())
            .map_err(|_| length_error(self.len()))?;
        let span = self.append(text.as_bytes())?;
        Ok(StrRef { span, source: SpanSource::DecodedArena, scalar_len })
    }

    /// Removes bytes after `len`, retaining the allocation.
    ///
    /// Refuses to grow the arena. References into removed bytes are invalidated;
    /// a subsequent append may reuse their former coordinates.
    pub fn truncate(&mut self, len: u32) -> Result<()> {
        let invalid = || Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Decode,
            len,
            "decoded arena truncation exceeds current length",
        );
        let requested = usize::try_from(len).map_err(|_| invalid())?;
        if requested > self.bytes.len() {
            return Err(invalid());
        }
        self.bytes.truncate(requested);
        Ok(())
    }

    /// Removes all logical bytes while retaining allocation and limit.
    ///
    /// All references to removed bytes are invalidated.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// Transfers exactly the logical bytes into an owned slice.
    ///
    /// Spare capacity is discarded. This conversion follows `alloc`'s normal
    /// allocation-failure behavior rather than returning an ABI error.
    pub fn into_boxed_slice(self) -> Box<[u8]> {
        self.bytes.into_boxed_slice()
    }

    // Keep validation and reservation shared by byte appends and reservations.
    // checked_span checks both conversions and offset + length before any
    // allocation. Remap its generic location to the current arena offset.
    fn reserve_region(&mut self, additional: usize) -> Result<Span32> {
        let offset = self.bytes.len();
        let span = checked_span(offset, additional, SpanSource::DecodedArena)
            .map_err(|_| length_error(offset))?;
        let end = span.end().ok_or_else(|| length_error(offset))?;
        if end > self.limit {
            return Err(length_error(offset));
        }
        reserve_bytes(&mut self.bytes, additional, span.offset)?;
        Ok(span)
    }
}

impl Default for DecodedArena {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_DECODED_BYTES)
    }
}

fn length_error(offset: usize) -> Error {
    Error::new(
        ErrorCode::DecodedArenaOverflow,
        ErrorSource::Decode,
        // Public operations preserve a representable length. Retain a bounded
        // diagnostic even if internal crate code supplies an invalid arena.
        u32::try_from(offset).unwrap_or(u32::MAX),
        "decoded arena length exceeds configured or ABI limit",
    )
}

fn reserve_bytes(bytes: &mut Vec<u8>, additional: usize, offset: u32) -> Result<()> {
    bytes.try_reserve(additional).map_err(|_| Error::new(
        ErrorCode::BudgetExhausted,
        ErrorSource::Decode,
        offset,
        "decoded arena allocation failed",
    ))
}
