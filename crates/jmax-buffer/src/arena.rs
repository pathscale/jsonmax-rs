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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_BUFFER_BYTES;

    fn assert_error(error: Error, code: ErrorCode, offset: u32, detail: &'static str) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert_eq!(error.detail, detail);
    }

    fn assert_overflow(error: Error, offset: u32) {
        assert_error(
            error,
            ErrorCode::DecodedArenaOverflow,
            offset,
            "decoded arena length exceeds configured or ABI limit",
        );
    }

    #[test]
    fn new_and_default_are_empty_without_allocation() {
        for limit in [0, 1, DEFAULT_MAX_DECODED_BYTES, u32::MAX] {
            let arena = DecodedArena::new(limit);
            assert!(arena.is_empty());
            assert_eq!(arena.len(), 0);
            assert_eq!(arena.capacity(), 0);
            assert_eq!(arena.limit(), limit);
            assert_eq!(arena.as_slice(), &[]);
        }
        let arena = DecodedArena::default();
        assert_eq!(arena.limit(), DEFAULT_MAX_DECODED_BYTES);
        assert!(arena.is_empty());
        assert_eq!(arena.capacity(), 0);
    }

    #[test]
    fn initial_capacity_is_reserved_but_not_initialized() {
        for capacity in [0, 1, 7, 32] {
            let limit = u32::try_from(capacity).unwrap();
            let mut arena = DecodedArena::with_capacity(capacity, limit).unwrap();
            assert!(arena.is_empty());
            assert_eq!(arena.len(), 0);
            assert_eq!(arena.limit(), limit);
            assert!(arena.capacity() >= capacity);
            let allocated = arena.capacity();
            let pointer = arena.as_slice().as_ptr();
            let bytes = alloc::vec![0x7f; capacity];
            assert_eq!(arena.append(&bytes).unwrap(), Span32::new(0, limit));
            assert_eq!(arena.as_slice(), bytes.as_slice());
            assert_eq!(arena.capacity(), allocated);
            assert_eq!(arena.as_slice().as_ptr(), pointer);
        }
        assert_eq!(DecodedArena::with_capacity(0, 0).unwrap().capacity(), 0);
    }

    #[test]
    fn initial_capacity_rejects_limit_and_abi_overflow_before_allocation() {
        for (capacity, limit) in [(1, 0), (4, 3), (usize::MAX, 0)] {
            assert_overflow(DecodedArena::with_capacity(capacity, limit).unwrap_err(), 0);
        }
        if let Some(over_abi) = MAX_BUFFER_BYTES.checked_add(1) {
            assert_overflow(
                DecodedArena::with_capacity(over_abi, u32::MAX).unwrap_err(), 0,
            );
        }
    }

    #[test]
    fn zero_limit_accepts_only_empty_operations() {
        let mut arena = DecodedArena::new(0);
        assert_eq!(arena.reserve(0), Ok(()));
        assert_eq!(arena.append(&[]), Ok(Span32::new(0, 0)));
        assert_eq!(arena.append_str(""), Ok(StrRef {
            span: Span32::new(0, 0),
            source: SpanSource::DecodedArena,
            scalar_len: 0,
        }));
        assert_eq!(arena.truncate(0), Ok(()));
        assert_overflow(arena.reserve(1).unwrap_err(), 0);
        assert_overflow(arena.append(b"x").unwrap_err(), 0);
        assert_overflow(arena.append_str("x").unwrap_err(), 0);
        arena.clear();
        assert!(arena.is_empty());
        assert_eq!(arena.capacity(), 0);
    }

    #[test]
    fn reserve_counts_additional_bytes_from_length_without_mutating_bytes() {
        let mut arena = DecodedArena::new(16);
        arena.append(b"abc").unwrap();
        arena.reserve(13).unwrap();
        assert_eq!(arena.as_slice(), b"abc");
        assert_eq!(arena.len(), 3);
        assert!(arena.capacity() >= 16);
        let capacity = arena.capacity();
        let pointer = arena.as_slice().as_ptr();
        arena.reserve(0).unwrap();
        assert_eq!(arena.as_slice().as_ptr(), pointer);
        assert_eq!(arena.capacity(), capacity);
        assert_overflow(arena.reserve(14).unwrap_err(), 3);
        assert_eq!(arena.as_slice(), b"abc");
        assert_eq!(arena.len(), 3);
    }

    #[test]
    fn spare_capacity_above_limit_never_permits_excess_logical_bytes() {
        // This is a valid arena state, also permitted after allocator rounding.
        // Construct it explicitly to avoid assuming an allocator growth policy.
        let mut arena = DecodedArena { bytes: Vec::with_capacity(64), limit: 3 };
        let capacity = arena.capacity();
        assert!(arena.capacity() > usize::try_from(arena.limit()).unwrap());
        arena.append(b"abc").unwrap();
        assert_overflow(arena.reserve(1).unwrap_err(), 3);
        assert_overflow(arena.append(b"d").unwrap_err(), 3);
        assert_eq!(arena.as_slice(), b"abc");
        assert_eq!(arena.limit(), 3);
        assert_eq!(arena.capacity(), capacity);
        assert_eq!(arena.reserve(0), Ok(()));
        assert_eq!(arena.append(&[]), Ok(Span32::new(3, 0)));
    }

    #[test]
    fn overflowing_reservations_report_current_offset_and_preserve_content() {
        let mut arena = DecodedArena::new(u32::MAX);
        arena.append(b"x").unwrap();
        // On 32-bit targets this overflows the span end; on wider targets it
        // also exceeds the ABI length. Neither case attempts an allocation.
        assert_overflow(arena.reserve(usize::MAX).unwrap_err(), 1);
        assert_overflow(arena.reserve(MAX_BUFFER_BYTES).unwrap_err(), 1);
        if let Some(over_abi) = MAX_BUFFER_BYTES.checked_add(1) {
            assert_overflow(arena.reserve(over_abi).unwrap_err(), 1);
        }
        assert_eq!(arena.as_slice(), b"x");
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn allocation_errors_are_mapped_without_allocator_fault_injection() {
        // usize::MAX cannot be reserved by Vec<u8>: with an existing byte the
        // requested length overflows, and an empty Vec exceeds isize::MAX.
        // Exercise the same reservation helper used by public operations;
        // public ABI/limit checks correctly reject this request even earlier.
        for prefix in [&b""[..], &b"abc"[..]] {
            let mut bytes = prefix.to_vec();
            let offset = u32::try_from(bytes.len()).unwrap();
            let error = reserve_bytes(&mut bytes, usize::MAX, offset).unwrap_err();
            assert_error(
                error, ErrorCode::BudgetExhausted, offset,
                "decoded arena allocation failed",
            );
            assert_eq!(bytes.as_slice(), prefix);
            assert_eq!(bytes.len(), prefix.len());
        }
    }

    #[test]
    fn binary_appends_return_contiguous_source_independent_spans() {
        let mut arena = DecodedArena::new(8);
        let vectors: &[(&[u8], Span32)] = &[
            (b"", Span32::new(0, 0)),
            (b"a", Span32::new(0, 1)),
            (b"\0\xff\x80", Span32::new(1, 3)),
            (b"", Span32::new(4, 0)),
            (b"{]x?", Span32::new(4, 4)),
            (b"", Span32::new(8, 0)),
        ];
        for &(bytes, expected) in vectors {
            let span = arena.append(bytes).unwrap();
            assert_eq!(span, expected);
            assert_eq!(span.slice(arena.as_slice()), Some(bytes));
        }
        assert_eq!(arena.as_slice(), b"a\0\xff\x80{]x?");
        assert_eq!(arena.len(), 8);
        assert!(core::str::from_utf8(arena.as_slice()).is_err());
    }

    #[test]
    fn many_small_appends_preserve_earlier_coordinates_and_content() {
        let mut arena = DecodedArena::new(256);
        let mut spans = Vec::new();
        for value in 0u8..=u8::MAX {
            let span = arena.append(&[value]).unwrap();
            assert_eq!(span, Span32::new(u32::from(value), 1));
            spans.push(span);
        }
        for (value, span) in (0u8..=u8::MAX).zip(spans) {
            assert_eq!(span.slice(arena.as_slice()), Some(&[value][..]));
        }
        assert_eq!(arena.len(), 256);
        assert_overflow(arena.append(&[0]).unwrap_err(), 256);
        assert_eq!(arena.len(), 256);
    }

    #[test]
    fn failed_byte_and_string_appends_preserve_existing_bytes() {
        let mut arena = DecodedArena::with_capacity(5, 5).unwrap();
        arena.append(b"abc").unwrap();
        assert_overflow(arena.append(b"def").unwrap_err(), 3);
        assert_overflow(arena.append_str("\u{20ac}").unwrap_err(), 3);
        assert_eq!(arena.as_slice(), b"abc");
        let reference = arena.append_str("\u{e9}").unwrap();
        assert_eq!(reference.span, Span32::new(3, 2));
        assert_eq!(reference.scalar_len, 1);
        assert_eq!(arena.len(), 5);
        assert_overflow(arena.append(b"x").unwrap_err(), 5);
        assert_overflow(arena.append_str("x").unwrap_err(), 5);
        assert_eq!(arena.as_slice(), b"abc\xc3\xa9");
        assert_eq!(arena.append_str("").unwrap().span, Span32::new(5, 0));
    }

    #[test]
    fn string_metadata_counts_unicode_scalars_and_utf8_bytes_separately() {
        let vectors: &[(&str, u32, u32)] = &[
            ("", 0, 0),
            ("ascii", 5, 5),
            ("\0", 1, 1),
            ("\u{7f}", 1, 1),
            ("\u{80}", 2, 1),
            ("\u{7ff}", 2, 1),
            ("\u{800}", 3, 1),
            ("\u{d7ff}", 3, 1),
            ("\u{e000}", 3, 1),
            ("\u{ffff}", 3, 1),
            ("\u{10000}", 4, 1),
            ("\u{10ffff}", 4, 1),
            ("e\u{301}", 3, 2),
            ("\u{1f469}\u{200d}\u{1f4bb}", 11, 3),
            ("a\u{e9}\u{20ac}\u{1f600}", 10, 4),
        ];
        let mut arena = DecodedArena::new(128);
        arena.append(b"\xff\x80").unwrap();
        let mut offset = 2u32;
        for &(text, byte_len, scalar_len) in vectors {
            let reference = arena.append_str(text).unwrap();
            assert_eq!(reference, StrRef {
                span: Span32::new(offset, byte_len),
                source: SpanSource::DecodedArena,
                scalar_len,
            });
            assert_eq!(reference.span.slice(arena.as_slice()), Some(text.as_bytes()));
            offset = offset.checked_add(byte_len).unwrap();
        }
        assert_eq!(arena.len(), usize::try_from(offset).unwrap());
        assert!(core::str::from_utf8(arena.as_slice()).is_err());
    }

    #[test]
    fn truncate_retains_allocation_and_reuses_removed_coordinates() {
        let mut arena = DecodedArena::with_capacity(32, 32).unwrap();
        let prefix = arena.append(b"abc").unwrap();
        let removed = arena.append(b"def").unwrap();
        let capacity = arena.capacity();
        let pointer = arena.as_slice().as_ptr();
        arena.truncate(6).unwrap();
        assert_eq!(arena.as_slice(), b"abcdef");
        arena.truncate(3).unwrap();
        assert_eq!(arena.as_slice(), b"abc");
        assert_eq!(prefix.slice(arena.as_slice()), Some(&b"abc"[..]));
        assert_eq!(removed.slice(arena.as_slice()), None);
        assert_eq!(arena.capacity(), capacity);
        assert_eq!(arena.as_slice().as_ptr(), pointer);
        assert_eq!(arena.append(b"XY").unwrap(), Span32::new(3, 2));
        assert_eq!(arena.as_slice(), b"abcXY");
        arena.truncate(0).unwrap();
        assert!(arena.is_empty());
        assert_eq!(arena.capacity(), capacity);
        assert_eq!(arena.as_slice().as_ptr(), pointer);
        assert_eq!(arena.limit(), 32);
    }

    #[test]
    fn truncate_operates_on_bytes_without_requiring_a_utf8_boundary() {
        let mut arena = DecodedArena::new(4);
        arena.append_str("\u{1f600}").unwrap();
        arena.truncate(2).unwrap();
        assert_eq!(arena.as_slice(), &[0xf0, 0x9f]);
        assert!(core::str::from_utf8(arena.as_slice()).is_err());
        assert_eq!(arena.append(&[0x98, 0x80]).unwrap(), Span32::new(2, 2));
        assert_eq!(core::str::from_utf8(arena.as_slice()), Ok("\u{1f600}"));
    }

    #[test]
    fn invalid_truncation_reports_requested_length_and_preserves_bytes() {
        let mut arena = DecodedArena::new(8);
        arena.append(b"abc").unwrap();
        for requested in [4, 8, u32::MAX] {
            let error = arena.truncate(requested).unwrap_err();
            assert_error(
                error, ErrorCode::InvalidToken, requested,
                "decoded arena truncation exceeds current length",
            );
            assert_eq!(arena.as_slice(), b"abc");
            assert_eq!(arena.len(), 3);
        }
        let mut empty = DecodedArena::new(0);
        assert_error(
            empty.truncate(1).unwrap_err(), ErrorCode::InvalidToken, 1,
            "decoded arena truncation exceeds current length",
        );
        assert!(empty.is_empty());
    }

    #[test]
    fn clear_retains_allocation_and_restarts_coordinates() {
        let mut arena = DecodedArena::with_capacity(16, 16).unwrap();
        let old = arena.append(b"old").unwrap();
        let capacity = arena.capacity();
        let pointer = arena.as_slice().as_ptr();
        for _ in 0..2 {
            arena.clear();
            assert!(arena.is_empty());
            assert_eq!(arena.len(), 0);
            assert_eq!(arena.capacity(), capacity);
            assert_eq!(arena.as_slice().as_ptr(), pointer);
            assert_eq!(arena.limit(), 16);
            assert_eq!(old.slice(arena.as_slice()), None);
        }
        assert_eq!(arena.append(b"new").unwrap(), Span32::new(0, 3));
        assert_eq!(arena.as_slice(), b"new");
        assert_eq!(arena.as_slice().as_ptr(), pointer);
    }

    #[test]
    fn boxed_slice_contains_only_logical_bytes_including_empty_cases() {
        let mut arena = DecodedArena::with_capacity(64, 64).unwrap();
        arena.append(b"\0\xffkept:removed").unwrap();
        arena.truncate(6).unwrap();
        let bytes: Box<[u8]> = arena.into_boxed_slice();
        assert_eq!(bytes.as_ref(), b"\0\xffkept");
        assert_eq!(bytes.len(), 6);
        let mut cleared = DecodedArena::with_capacity(64, 64).unwrap();
        cleared.append(b"discarded").unwrap();
        cleared.clear();
        assert!(cleared.into_boxed_slice().is_empty());
        assert!(DecodedArena::with_capacity(8, 8).unwrap().into_boxed_slice().is_empty());
        assert!(DecodedArena::new(0).into_boxed_slice().is_empty());
    }

    #[test]
    fn clone_preserves_bytes_and_limit_with_independent_mutations() {
        let mut original = DecodedArena::new(8);
        let span = original.append(b"abc").unwrap();
        let mut cloned = original.clone();
        cloned.truncate(1).unwrap();
        cloned.append(b"XY").unwrap();
        assert_eq!(original.as_slice(), b"abc");
        assert_eq!(cloned.as_slice(), b"aXY");
        assert_eq!(cloned.limit(), original.limit());
        original.clear();
        assert_eq!(span.slice(cloned.as_slice()), Some(&b"aXY"[..]));
        assert_eq!(cloned.as_slice(), b"aXY");
    }
}
