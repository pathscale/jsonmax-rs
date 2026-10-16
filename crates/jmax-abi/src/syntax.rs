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

/// Validated metadata for a parsed JSON string token.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringMeta {
    /// Raw content span inside quotes (excluding the opening and closing `"`).
    pub raw_content: Span32,
    /// Raw token span including quotes (`raw_content.offset - 1 .. raw_content.end + 1`).
    pub raw_token: Span32,
    /// Cooked string reference (`RawPayload` when unescaped, `DecodedArena` when escaped).
    pub cooked: StrRef,
    /// True when the string contained no backslash escapes (`cooked.source == RawPayload`).
    pub is_unescaped: bool,
    /// True when all bytes in the string are ASCII (`< 0x80`).
    pub is_ascii: bool,
}

/// Validated metadata for an object key token, carrying raw/cooked spans and SWAR/hash prefix.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyMeta {
    pub string: StringMeta,
    /// First up-to-8 cooked bytes packed in little-endian SWAR order (matching `KeyEntry::swar_head`).
    pub swar_head: u64,
    /// Optional 64-bit hash of the cooked key bytes (FNV-1a 64-bit).
    pub cooked_hash: u64,
}

impl KeyMeta {
    /// Computes the canonical 64-bit little-endian SWAR head for up to 8 cooked bytes.
    pub fn compute_swar_head(cooked: &[u8]) -> u64 {
        let mut head = 0u64;
        let n = cooked.len().min(8);
        let mut i = 0;
        while i < n {
            head |= (cooked[i] as u64) << (i * 8);
            i += 1;
        }
        head
    }

    /// Computes a deterministic 64-bit FNV-1a hash of the cooked key bytes.
    pub fn compute_fnv1a64(cooked: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut i = 0;
        while i < cooked.len() {
            hash ^= cooked[i] as u64;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
            i += 1;
        }
        hash
    }
}

/// Explicit single owner for object duplicate-key enforcement.
///
/// Parser and Schema VM must coordinate so duplicate checking is performed
/// once by the designated owner and never disabled in both when `Limits::reject_duplicate_keys` is true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DuplicatePolicyOwner {
    /// Syntax / `BlockCursor` enforces duplicate-key policy for all objects.
    Syntax,
    /// Schema VM enforces duplicate-key policy (known keys via `seen_words`, unknown keys via span-hash table).
    SchemaVm,
    /// Duplicate keys are permitted by policy (`!limits.reject_duplicate_keys`).
    Disabled,
}

/// Subtree skip mode with explicit proof and validation guarantees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipMode {
    /// Structural bitmask boundary jump for query-only unselected subtrees.
    /// Never issues a `ValidationProof`.
    QueryOnly,
    /// Full lexical, UTF-8, escape, number, duplicate, and limit validation
    /// without constructing interior DOM nodes or slot captures.
    StrictValidate,
    /// Structural skip authorized by a verified `ValidationCapability` on the
    /// exact same immutable owner, schema, and limits.
    ValidatedOwner(crate::buffer::ValidationCapability),
}

impl Limits {
    /// Deterministic 64-bit fingerprint of resource limits and key policies.
    pub const fn policy_fingerprint(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let words = [
            self.max_payload_bytes as u64,
            self.max_decoded_bytes as u64,
            self.max_depth as u64,
            self.max_fields_per_object as u64,
            self.max_string_bytes as u64,
            self.max_number_digits as u64,
            self.max_array_items as u64,
            (self.reject_duplicate_keys as u64) | ((self.reject_unknown_keys as u64) << 1),
        ];
        let mut i = 0;
        while i < words.len() {
            h ^= words[i];
            h = h.wrapping_mul(0x0100_0000_01b3);
            i += 1;
        }
        h
    }
}
