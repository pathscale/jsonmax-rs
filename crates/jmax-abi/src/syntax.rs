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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SpanSource;

    #[test]
    fn strict_limits_are_const_finite_and_reject_ambiguous_keys() {
        const LIMITS: Limits = Limits::strict_default();
        assert_eq!(LIMITS.max_payload_bytes, 16 * 1024 * 1024);
        assert_eq!(LIMITS.max_decoded_bytes, 16 * 1024 * 1024);
        assert_eq!(LIMITS.max_depth, 128);
        assert_eq!(LIMITS.max_fields_per_object, 65_536);
        assert_eq!(LIMITS.max_string_bytes, 1024 * 1024);
        assert_eq!(LIMITS.max_number_digits, 1_024);
        assert_eq!(LIMITS.max_array_items, 1_048_576);
        assert!(LIMITS.reject_duplicate_keys);
        assert!(LIMITS.reject_unknown_keys);
    }

    #[test]
    fn permissive_limits_use_all_representable_capacity() {
        const LIMITS: Limits = Limits::permissive_default();
        assert_eq!(LIMITS.max_payload_bytes, u32::MAX);
        assert_eq!(LIMITS.max_decoded_bytes, u32::MAX);
        assert_eq!(LIMITS.max_depth, u16::MAX);
        assert_eq!(LIMITS.max_fields_per_object, u32::MAX);
        assert_eq!(LIMITS.max_string_bytes, u32::MAX);
        assert_eq!(LIMITS.max_number_digits, u16::MAX);
        assert_eq!(LIMITS.max_array_items, u32::MAX);
        assert!(!LIMITS.reject_duplicate_keys);
        assert!(!LIMITS.reject_unknown_keys);
        assert_ne!(LIMITS, Limits::strict_default());
    }

    #[test]
    fn cursor_default_is_unstarted_and_clone_is_independent() {
        let cursor = CursorState::default();
        assert_eq!(cursor.byte_offset, 0);
        assert_eq!(cursor.depth, 0);
        assert_eq!(cursor.phase, None);
        assert!(!cursor.finished);
        let mut clone = cursor.clone();
        clone.byte_offset = u32::MAX;
        clone.depth = u16::MAX;
        clone.phase = Some(FramePhase::RootDone);
        clone.finished = true;
        assert_eq!(cursor.byte_offset, 0);
        assert_eq!(cursor.phase, None);
        assert!(!cursor.finished);
        assert_eq!(clone.phase, Some(FramePhase::RootDone));
    }

    #[test]
    fn scratch_clear_resets_every_buffer_and_preserves_capacity() {
        let mut scratch = Scratch::default();
        scratch.clear();
        assert!(scratch.decoded_buf.is_empty());
        assert!(scratch.stack.is_empty());
        assert!(scratch.seen_words.is_empty());
        scratch.decoded_buf.extend_from_slice(b"decoded");
        scratch.stack.extend_from_slice(&[FramePhase::RootStart, FramePhase::ObjectValue]);
        scratch.seen_words.extend_from_slice(&[u64::MAX, 1]);
        let capacities = (
            scratch.decoded_buf.capacity(), scratch.stack.capacity(), scratch.seen_words.capacity(),
        );
        let cloned = scratch.clone();
        scratch.clear();
        scratch.clear();
        assert!(scratch.decoded_buf.is_empty());
        assert!(scratch.stack.is_empty());
        assert!(scratch.seen_words.is_empty());
        assert_eq!((
            scratch.decoded_buf.capacity(), scratch.stack.capacity(), scratch.seen_words.capacity(),
        ), capacities);
        assert_eq!(cloned.decoded_buf, b"decoded");
        assert_eq!(cloned.stack.len(), 2);
        assert_eq!(cloned.seen_words, [u64::MAX, 1]);
        scratch.decoded_buf.push(b'x');
        scratch.stack.push(FramePhase::ArrayValueOrEnd);
        scratch.seen_words.push(0);
        assert_eq!(scratch.decoded_buf, b"x");
        assert_eq!(scratch.stack, [FramePhase::ArrayValueOrEnd]);
        assert_eq!(scratch.seen_words, [0]);
    }

    #[test]
    fn events_preserve_offsets_spans_and_decoded_references() {
        let string = StrRef {
            span: Span32::new(7, 2), source: SpanSource::DecodedArena, scalar_len: 1,
        };
        let events = [
            Event::ObjectStart { offset: 0 }, Event::ObjectEnd { offset: u32::MAX },
            Event::ArrayStart { offset: 2 }, Event::ArrayEnd { offset: 3 },
            Event::Key { str_ref: string },
            Event::Scalar { kind: ScalarKind::String, span: Span32::new(4, 8), decoded: Some(string) },
        ];
        for event in events {
            let step = ScanStep::Emit(event);
            match step {
                ScanStep::Emit(copy) => assert_eq!(copy, event),
                _ => panic!("emitted event was not preserved"),
            }
            assert_ne!(step, ScanStep::NeedMore);
            assert_ne!(step, ScanStep::Finished);
        }
        assert_ne!(ScanStep::NeedMore, ScanStep::Finished);
        for kind in [ScalarKind::Number, ScalarKind::True, ScalarKind::False, ScalarKind::Null] {
            let span = Span32::new(10, 4);
            let event = Event::Scalar { kind, span, decoded: None };
            match event {
                Event::Scalar { kind: actual, span: actual_span, decoded } => {
                    assert_eq!(actual, kind);
                    assert_eq!(actual_span, span);
                    assert_eq!(decoded, None);
                }
                _ => panic!("scalar event changed variant"),
            }
        }
    }

    #[test]
    fn token_and_phase_variants_remain_distinct() {
        let tokens = [
            TokenKind::LBrace, TokenKind::RBrace, TokenKind::LBracket, TokenKind::RBracket,
            TokenKind::Colon, TokenKind::Comma, TokenKind::String, TokenKind::Number,
            TokenKind::True, TokenKind::False, TokenKind::Null,
        ];
        for (index, token) in tokens.iter().enumerate() {
            assert!(!tokens[index + 1..].contains(token));
        }
        let phases = [
            FramePhase::RootStart, FramePhase::RootDone, FramePhase::ObjectStart,
            FramePhase::ObjectKeyOrEnd, FramePhase::ObjectColon, FramePhase::ObjectValue,
            FramePhase::ObjectCommaOrEnd, FramePhase::ArrayStart, FramePhase::ArrayValueOrEnd,
            FramePhase::ArrayCommaOrEnd,
        ];
        for (index, phase) in phases.iter().enumerate() {
            assert!(!phases[index + 1..].contains(phase));
        }
        fn assert_hash<T: core::hash::Hash>() {}
        assert_hash::<TokenKind>();
        assert_hash::<ScalarKind>();
        assert_hash::<FramePhase>();
    }
}
