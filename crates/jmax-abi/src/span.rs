//! Compact references into raw payloads and decoded string arenas.

/// A byte range whose exclusive end must fit in the 32-bit address space.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Span32 {
    pub offset: u32,
    pub len: u32,
}

impl Span32 {
    pub const fn new(offset: u32, len: u32) -> Self {
        Self { offset, len }
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn end(&self) -> Option<u32> {
        self.offset.checked_add(self.len)
    }

    /// Returns the range only if its end and both buffer indices are valid.
    /// Empty ranges at the end of the buffer are permitted.
    pub fn slice<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        let end = usize::try_from(self.end()?).ok()?;
        let start = usize::try_from(self.offset).ok()?;
        buf.get(start..end)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpanSource {
    RawPayload,
    DecodedArena,
}

/// A UTF-8 string range with its Unicode scalar count, excluding JSON quotes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StrRef {
    pub span: Span32,
    pub source: SpanSource,
    pub scalar_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NumberRef {
    pub raw: Span32,
    pub kind: NumberLexKind,
    pub slot_index: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumberLexKind {
    UnsignedInt,
    SignedInt,
    FloatDecimal,
    QuotedDecimal,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct NodeId(pub u32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct SpanId(pub u32);

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn construction_and_arithmetic_are_const() {
        const SPAN: Span32 = Span32::new(3, 7);
        const END: Option<u32> = SPAN.end();
        const EMPTY: bool = Span32::new(7, 0).is_empty();
        assert_eq!(END, Some(10));
        assert!(EMPTY);
        assert!(!SPAN.is_empty());
        assert_eq!(Span32::default(), Span32::new(0, 0));
    }

    #[test]
    fn end_handles_u32_boundary_without_wrapping() {
        assert_eq!(Span32::new(u32::MAX, 0).end(), Some(u32::MAX));
        assert_eq!(Span32::new(u32::MAX - 1, 1).end(), Some(u32::MAX));
        assert_eq!(Span32::new(u32::MAX, 1).end(), None);
        assert_eq!(Span32::new(1, u32::MAX).end(), None);
    }

    #[test]
    fn slicing_handles_empty_full_partial_and_invalid_ranges() {
        let bytes = b"abcd";
        assert_eq!(Span32::new(0, 4).slice(bytes), Some(&bytes[..]));
        assert_eq!(Span32::new(1, 2).slice(bytes), Some(&bytes[1..3]));
        assert_eq!(Span32::new(4, 0).slice(bytes), Some(&bytes[4..]));
        assert_eq!(Span32::new(0, 0).slice(&[]), Some(&[][..]));
        assert_eq!(Span32::new(5, 0).slice(bytes), None);
        assert_eq!(Span32::new(3, 2).slice(bytes), None);
        assert_eq!(Span32::new(u32::MAX, 1).slice(bytes), None);
        assert_eq!(Span32::new(u32::MAX, 0).slice(bytes), None);
    }

    #[test]
    fn references_preserve_source_kind_and_index() {
        let raw = StrRef {
            span: Span32::new(1, 2), source: SpanSource::RawPayload, scalar_len: 1,
        };
        let decoded = StrRef { source: SpanSource::DecodedArena, ..raw };
        assert_ne!(raw, decoded);
        for kind in [
            NumberLexKind::UnsignedInt, NumberLexKind::SignedInt,
            NumberLexKind::FloatDecimal, NumberLexKind::QuotedDecimal,
        ] {
            let number = NumberRef { raw: raw.span, kind, slot_index: u32::MAX };
            assert_eq!(number.raw, raw.span);
            assert_eq!(number.kind, kind);
            assert_eq!(number.slot_index, u32::MAX);
        }
        fn assert_hash<T: core::hash::Hash>() {}
        assert_hash::<StrRef>();
        assert_hash::<NumberRef>();
        assert_hash::<NodeId>();
        assert_hash::<SpanId>();
    }

    #[test]
    fn fixed_layouts_match_their_storage() {
        assert_eq!(size_of::<Span32>(), 8);
        assert_eq!(align_of::<Span32>(), align_of::<u32>());
        assert_eq!(size_of::<NodeId>(), size_of::<u32>());
        assert_eq!(align_of::<NodeId>(), align_of::<u32>());
        assert_eq!(size_of::<SpanId>(), size_of::<u32>());
        assert_eq!(align_of::<SpanId>(), align_of::<u32>());
        assert_eq!(NodeId::default().0, 0);
        assert_eq!(SpanId::default().0, 0);
    }
}
