//! Bounded JSON output with syntax state and explicit validated-fragment ownership.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec::Vec;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::node::ValidatedDocument;
pub use jmax_abi::number::Decimal128;
pub use jmax_abi::schema::{SchemaFieldIr, SchemaIr, SlotValue, ValidatedSlots};
pub use jmax_abi::traits::{ByteSink, JsonEncode};

pub mod raw;
pub mod sink;
pub mod typed;
pub mod writer;

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
        Self {
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_container_items: 1_000_000,
            max_string_bytes: 4 * 1_024 * 1_024,
            max_number_bytes: 1_024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct VecSink {
    pub(crate) bytes: Vec<u8>,
    pub(crate) limit: usize,
}

impl VecSink {
    pub const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

/// Limits newly appended bytes independently of existing bytes in the inner sink.
pub struct LimitedSink<'a> {
    pub(crate) inner: &'a mut dyn ByteSink,
    pub(crate) start_len: usize,
    pub(crate) limit: usize,
    pub(crate) poisoned: bool,
    /// Current token's reserved endpoint, measured from this message's start.
    pub(crate) reserved_len: usize,
    /// Capacity hint may exceed the current token's pending reservation.
    pub(crate) capacity_len: usize,
}

impl<'a> LimitedSink<'a> {
    pub fn new(inner: &'a mut dyn ByteSink, limit: usize) -> Self {
        let start_len = inner.len();
        Self {
            inner,
            start_len,
            limit,
            poisoned: false,
            reserved_len: 0,
            capacity_len: 0,
        }
    }
    pub fn appended_len(&self) -> usize {
        self.inner.len().saturating_sub(self.start_len)
    }
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteContainer {
    Object,
    Array,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteFrame {
    pub tape_end: u32,
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
    pub(crate) escape_masker: jmax_string::EscapeMasker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentKind {
    Null,
    Boolean,
    Number,
    String,
    Object,
    Array,
}

/// The only public constructor is the strict parser in `raw::validate`.
#[derive(Clone, Copy, Debug)]
pub struct RawFragment<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) kind: FragmentKind,
    pub(crate) depth: u16,
}

impl<'a> RawFragment<'a> {
    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }
    pub const fn kind(self) -> FragmentKind {
        self.kind
    }
    /// Maximum simultaneously open containers, relative to this fragment's root.
    pub const fn depth(self) -> u16 {
        self.depth
    }
}

/// Exact quoted JSON length for the canonical minimal escaping used by the writer.
pub fn escaped_string_len(text: &str) -> Result<usize> {
    let bytes = text.as_bytes();
    let mut size = checked_output_len(bytes.len(), 2, usize::MAX)?;
    for index in EscapePositions::new(bytes, jmax_string::EscapeMasker::new()) {
        size = checked_output_len(size, escape_extra(bytes[index]), usize::MAX)?;
    }
    Ok(size)
}

/// Visits every mandatory escape once, retaining each block's remaining mask.
/// Full blocks borrow the input directly; the bounded tail needs no staging.
struct EscapePositions<'a> {
    bytes: &'a [u8],
    masker: jmax_string::EscapeMasker,
    scanned: usize,
    base: usize,
    remaining: u64,
}

impl<'a> EscapePositions<'a> {
    fn new(bytes: &'a [u8], masker: jmax_string::EscapeMasker) -> Self {
        Self {
            bytes,
            masker,
            scanned: 0,
            base: 0,
            remaining: 0,
        }
    }
}

impl Iterator for EscapePositions<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.remaining != 0 {
                let index = self.remaining.trailing_zeros() as usize;
                self.remaining &= self.remaining - 1;
                return Some(self.base + index);
            }
            if self.scanned == self.bytes.len() {
                return None;
            }
            self.base = self.scanned;
            let rest = &self.bytes[self.scanned..];
            if let Some(block) = rest.first_chunk::<64>() {
                self.remaining = self.masker.classify(block).escape_or_control();
                self.scanned += 64;
            } else {
                for (index, &byte) in rest.iter().enumerate() {
                    if byte == b'"' || byte == b'\\' || byte < 0x20 {
                        self.remaining |= 1u64 << index;
                    }
                }
                self.scanned = self.bytes.len();
            }
        }
    }
}

fn escape_extra(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 1,
        _ => 5,
    }
}

pub fn checked_output_len(current: usize, additional: usize, limit: usize) -> Result<usize> {
    let next = current
        .checked_add(additional)
        .ok_or_else(|| encode_error(ErrorCode::PayloadTooLarge, "output size overflow"))?;
    if next > limit {
        return Err(encode_error(
            ErrorCode::PayloadTooLarge,
            "output size limit exceeded",
        ));
    }
    Ok(next)
}

pub(crate) fn encode_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}

/// Container frames retained between independent output messages.
#[derive(Default)]
pub struct WriterScratch {
    frames: Vec<WriteFrame>,
}

impl WriterScratch {
    pub fn retained_bytes(&self) -> usize {
        self.frames.capacity() * core::mem::size_of::<WriteFrame>()
    }
}
