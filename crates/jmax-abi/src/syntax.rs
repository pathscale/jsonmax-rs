//! Tokens, streaming events, parser limits, and reusable scratch storage.

use alloc::vec::Vec;
use crate::span::{Span32, StrRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    Comma,
    String,
    Number,
    True,
    False,
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarKind {
    String,
    Number,
    True,
    False,
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FramePhase {
    RootStart,
    RootDone,
    ObjectStart,
    ObjectKeyOrEnd,
    ObjectColon,
    ObjectValue,
    ObjectCommaOrEnd,
    ArrayStart,
    ArrayValueOrEnd,
    ArrayCommaOrEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    ObjectStart { offset: u32 },
    ObjectEnd { offset: u32 },
    ArrayStart { offset: u32 },
    ArrayEnd { offset: u32 },
    Key { str_ref: StrRef },
    Scalar { kind: ScalarKind, span: Span32, decoded: Option<StrRef> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStep {
    Emit(Event),
    NeedMore,
    Finished,
}

/// Inclusive resource ceilings and object-key validation policy.
/// A zero numeric limit permits no resource of that kind; it is not unlimited.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_payload_bytes: u32,
    pub max_decoded_bytes: u32,
    pub max_depth: u16,
    pub max_fields_per_object: u32,
    pub max_string_bytes: u32,
    pub max_number_digits: u16,
    pub max_array_items: u32,
    pub reject_duplicate_keys: bool,
    pub reject_unknown_keys: bool,
}

impl Limits {
    /// Finite defaults for untrusted input: 16 MiB payload and decoded arena,
    /// depth 128, 65,536 fields per object, 1 MiB strings, 1,024 number digits,
    /// and 1,048,576 items per array. Duplicate and unknown keys are rejected
    /// by consumers that perform the corresponding object/schema validation.
    pub const fn strict_default() -> Self {
        Self {
            max_payload_bytes: 16 * 1024 * 1024,
            max_decoded_bytes: 16 * 1024 * 1024,
            max_depth: 128,
            max_fields_per_object: 65_536,
            max_string_bytes: 1024 * 1024,
            max_number_digits: 1_024,
            max_array_items: 1_048_576,
            reject_duplicate_keys: true,
            reject_unknown_keys: true,
        }
    }

    /// Uses the largest representable resource ceilings and allows duplicate
    /// and unknown keys. Syntax, UTF-8, bounds, and numeric correctness still
    /// require validation; these limits do not authorize skipping it.
    pub const fn permissive_default() -> Self {
        Self {
            max_payload_bytes: u32::MAX,
            max_decoded_bytes: u32::MAX,
            max_depth: u16::MAX,
            max_fields_per_object: u32::MAX,
            max_string_bytes: u32::MAX,
            max_number_digits: u16::MAX,
            max_array_items: u32::MAX,
            reject_duplicate_keys: false,
            reject_unknown_keys: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CursorState {
    pub byte_offset: u32,
    pub depth: u16,
    pub phase: Option<FramePhase>,
    pub finished: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Scratch {
    pub decoded_buf: Vec<u8>,
    pub stack: Vec<FramePhase>,
    pub seen_words: Vec<u64>,
}

impl Scratch {
    /// Removes all per-message state while retaining allocations for reuse.
    /// This is a logical reset, not a secure erasure of retained memory.
    pub fn clear(&mut self) {
        self.decoded_buf.clear();
        self.stack.clear();
        self.seen_words.clear();
    }
}
