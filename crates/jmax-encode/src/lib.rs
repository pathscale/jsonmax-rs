//! Bounded JSON output with syntax state and explicit validated-fragment ownership.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::Decimal128;
pub use jmax_abi::node::ValidatedDocument;
pub use jmax_abi::schema::{SchemaFieldIr, SchemaIr, SlotValue, ValidatedSlots};
pub use jmax_abi::traits::{ByteSink, JsonEncode};

pub mod sink;
pub mod writer;
pub mod raw;
pub mod typed;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1_024 * 1_024;
pub const DEFAULT_MAX_DEPTH: u16 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeLimits {
    pub max_output_bytes: usize,
    pub max_depth: u16,
    pub max_container_items: u32,
    pub max_string_bytes: u32,
    pub max_number_bytes: u32,
}

impl Default for EncodeLimits {
    fn default() -> Self {
        Self { max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES, max_depth: DEFAULT_MAX_DEPTH,
            max_container_items: 1_000_000, max_string_bytes: 4 * 1_024 * 1_024,
            max_number_bytes: 1_024 }
    }
}

#[derive(Clone, Debug)]
pub struct VecSink { pub(crate) bytes: Vec<u8>, pub(crate) limit: usize }

impl VecSink {
    pub const fn new(limit: usize) -> Self { Self { bytes: Vec::new(), limit } }
    pub fn as_slice(&self) -> &[u8] { &self.bytes }
    pub fn into_vec(self) -> Vec<u8> { self.bytes }
    pub const fn limit(&self) -> usize { self.limit }
}

/// Limits newly appended bytes independently of existing bytes in the inner sink.
pub struct LimitedSink<'a> {
    pub(crate) inner: &'a mut dyn ByteSink,
    pub(crate) start_len: usize,
    pub(crate) limit: usize,
    pub(crate) poisoned: bool,
}

impl<'a> LimitedSink<'a> {
    pub fn new(inner: &'a mut dyn ByteSink, limit: usize) -> Self {
        let start_len = inner.len();
        Self { inner, start_len, limit, poisoned: false }
    }
    pub fn appended_len(&self) -> usize { self.inner.len().saturating_sub(self.start_len) }
    pub const fn is_poisoned(&self) -> bool { self.poisoned }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteContainer { Object, Array }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteFrame {
    pub kind: WriteContainer,
    pub count: u32,
    pub expecting_key: bool,
}

pub struct JsonWriter<'a> {
    pub(crate) sink: LimitedSink<'a>,
    pub(crate) limits: EncodeLimits,
    pub(crate) frames: Vec<WriteFrame>,
    pub(crate) root_written: bool,
    pub(crate) poisoned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentKind { Null, Boolean, Number, String, Object, Array }

/// The only public constructor is the strict parser in `raw::validate`.
#[derive(Clone, Copy, Debug)]
pub struct RawFragment<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) kind: FragmentKind,
    pub(crate) depth: u16,
}

impl<'a> RawFragment<'a> {
    pub fn as_bytes(self) -> &'a [u8] { self.bytes }
    pub const fn kind(self) -> FragmentKind { self.kind }
    /// Maximum simultaneously open containers, relative to this fragment's root.
    pub const fn depth(self) -> u16 { self.depth }
}

/// Exact quoted JSON length for the canonical minimal escaping used by the writer.
pub fn escaped_string_len(text: &str) -> Result<usize> {
    let mut size = 2usize;
    let mut bytes = text.as_bytes();
    let policy = jmax_string::EscapePolicy::default();
    while let Some(index) = jmax_string::escape::find_escape(bytes, policy) {
        let width = match bytes[index] {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
            _ => 6,
        };
        size = checked_output_len(size, index, usize::MAX)?;
        size = checked_output_len(size, width, usize::MAX)?;
        bytes = &bytes[index + 1..];
    }
    checked_output_len(size, bytes.len(), usize::MAX)
}

pub fn checked_output_len(current: usize, additional: usize, limit: usize) -> Result<usize> {
    let next = current.checked_add(additional).ok_or_else(||
        encode_error(ErrorCode::PayloadTooLarge, "output size overflow"))?;
    if next > limit {
        return Err(encode_error(ErrorCode::PayloadTooLarge, "output size limit exceeded"));
    }
    Ok(next)
}

pub(crate) fn encode_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}

/// Container frames retained between independent output messages.
#[derive(Default)]
pub struct WriterScratch { frames: Vec<WriteFrame> }

impl WriterScratch {
    pub fn retained_bytes(&self) -> usize {
        self.frames.capacity() * core::mem::size_of::<WriteFrame>()
    }
}
