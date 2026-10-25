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
