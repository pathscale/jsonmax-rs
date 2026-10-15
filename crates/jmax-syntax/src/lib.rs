//! Incremental JSON syntax state. References use absolute, owner-relative offsets.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
pub mod machine;
pub mod frames;
pub mod feed;
pub mod finish;

pub use jmax_abi::buffer::Payload;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::{CursorState, Event, FramePhase, Limits, ScalarKind, ScanStep,
    Scratch, TokenKind};

pub const JSON_WHITESPACE: [u8; 4] = [b' ', b'\t', b'\r', b'\n'];

/// A complete lexical token; strings carry a validated, cooked reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAction { None, PushObject, PushArray, Pop }

/// Parent phase advances when a value starts, before a child frame is pushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub next_phase: FramePhase,
    pub action: FrameAction,
    pub event: Option<Event>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerKind { Object, Array }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingKind { String, Number, Literal(TokenKind) }

/// Boundary discovery carry. Decoding occurs once the complete token is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingToken {
    pub kind: PendingKind,
    pub start: u32,
    pub scan_offset: u32,
    pub first_escape: Option<u32>,
    pub non_ascii: bool,
    pub control: bool,
    pub digits: u32,
    pub excess_digit: Option<u32>,
}

/// Complete input retains its original shared owner. Only incremental input
/// needs a growable byte buffer; neither traversal nor finalization copies it.
#[derive(Debug)]
pub(crate) enum InputStorage {
    Incremental(Vec<u8>),
    Complete(Payload),
}

impl InputStorage {
    pub(crate) fn into_payload(self) -> Payload {
        match self {
            Self::Incremental(bytes) => Payload::from_vec(bytes),
            Self::Complete(payload) => payload,
        }
    }
}

impl core::ops::Deref for InputStorage {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Incremental(bytes) => bytes,
            Self::Complete(payload) => payload.as_slice(),
        }
    }
}

/// One cached block, including the carry needed to extend a partial tail after
/// another feed. Classification may look ahead, but never consumes an event.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClassifiedBlock {
    pub start: u32,
    pub end: u32,
    pub before: jmax_dispatch::ScanCarry,
    pub masks: jmax_dispatch::BlockMasks,
}

/// Root is represented separately and does not consume container depth.
#[derive(Clone, Debug)]
pub struct Frame {
    pub(crate) kind: ContainerKind,
    pub(crate) phase: FramePhase,
    pub(crate) start_offset: u32,
    pub(crate) count: u32,
}

impl Frame {
    pub const fn kind(&self) -> ContainerKind { self.kind }
    pub const fn phase(&self) -> FramePhase { self.phase }
    pub const fn start_offset(&self) -> u32 { self.start_offset }
    pub const fn count(&self) -> u32 { self.count }
}

/// Retains input ownership so previously emitted references survive further feeds.
/// Once an error is recorded, every mutating operation returns that error.
#[derive(Debug)]
pub struct SyntaxState {
    pub(crate) limits: Limits,
    pub(crate) numeric: Option<jmax_abi::number::NumericPolicy>,
    pub(crate) input: InputStorage,
    pub(crate) decoded: Vec<u8>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) root_phase: FramePhase,
    pub(crate) byte_offset: u32,
    pub(crate) final_input: bool,
    pub(crate) finished: bool,
    pub(crate) failure: Option<Error>,
    pub(crate) pending: Option<PendingToken>,
    pub(crate) key_tables: Vec<frames::KeyTable>,
    pub(crate) dispatch: Option<jmax_dispatch::Dispatch>,
    pub(crate) block: Option<ClassifiedBlock>,
    pub(crate) scan_offset: u32,
    pub(crate) scan_carry: jmax_dispatch::ScanCarry,
}

impl SyntaxState {
    pub fn new(limits: Limits) -> Self {
        Self { limits, numeric: None, input: InputStorage::Incremental(Vec::new()),
            decoded: Vec::new(), frames: Vec::new(),
            root_phase: FramePhase::RootStart, byte_offset: 0, final_input: false,
            finished: false, failure: None, pending: None, key_tables: Vec::new(),
            dispatch: None, block: None, scan_offset: 0,
            scan_carry: jmax_dispatch::ScanCarry::default() }
    }

    /// Retains a complete payload without copying its bytes and declares EOF.
    /// Syntax errors are diagnosed lazily as events are consumed.
    pub fn from_payload(payload: Payload, limits: Limits) -> Result<Self> {
        frames::check_payload_growth(0, payload.len(), limits)?;
        let mut state = Self::new(limits);
        state.input = InputStorage::Complete(payload);
        state.final_input = true;
        Ok(state)
    }

    /// Retains complete input and enforces conversion policy as numbers arrive.
    pub fn from_payload_with_numeric(
        payload: Payload,
        limits: Limits,
        numeric: jmax_abi::number::NumericPolicy,
    ) -> Result<Self> {
        if numeric.max_exponent_abs < 0 {
            return Err(syntax_error(ErrorCode::ConstraintViolation, 0,
                "numeric exponent limit cannot be negative"));
        }
        let mut state = Self::from_payload(payload, limits)?;
        state.numeric = Some(numeric);
        Ok(state)
    }
    /// Start another complete input, retaining frames, duplicate tables and arena.
    pub fn reset_with_payload(&mut self, payload: Payload, limits: Limits,
        numeric: Option<jmax_abi::number::NumericPolicy>) -> Result<()> {
        frames::check_payload_growth(0, payload.len(), limits)?;
        if numeric.is_some_and(|p| p.max_exponent_abs < 0) {
            return Err(syntax_error(ErrorCode::ConstraintViolation, 0,
                "numeric exponent limit cannot be negative"));
        }
        self.limits = limits;
        self.numeric = numeric;
        self.input = InputStorage::Complete(payload);
        self.decoded.clear();
        self.frames.clear();
        self.root_phase = FramePhase::RootStart;
        self.byte_offset = 0;
        self.final_input = true;
        self.finished = false;
        self.failure = None;
        self.pending = None;
        self.dispatch = None;
        self.block = None;
        self.scan_offset = 0;
        self.scan_carry = jmax_dispatch::ScanCarry::default();
        Ok(())
    }
    /// Swap owner-bearing decoded storage after EOF or before resetting input.
    pub fn swap_decoded(&mut self, arena: &mut Vec<u8>) {
        core::mem::swap(&mut self.decoded, arena);
    }
    pub const fn limits(&self) -> Limits { self.limits }
    pub fn raw_bytes(&self) -> &[u8] { &self.input }
    pub fn decoded_bytes(&self) -> &[u8] { &self.decoded }
    pub const fn is_final(&self) -> bool { self.final_input }
    pub const fn is_finished(&self) -> bool { self.finished }
    pub fn failure(&self) -> Option<&Error> { self.failure.as_ref() }
    pub fn state(&self) -> CursorState {
        CursorState { byte_offset: self.byte_offset, depth: self.frames.len() as u16,
            phase: Some(self.frames.last().map_or(self.root_phase, |f| f.phase)),
            finished: self.finished }
    }
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        let bytes = self.resolve_bytes(reference)?;
        core::str::from_utf8(bytes).ok()
    }
    pub(crate) fn resolve_bytes(&self, reference: StrRef) -> Option<&[u8]> {
        match reference.source {
            SpanSource::RawPayload => reference.span.slice(&self.input),
            SpanSource::DecodedArena => reference.span.slice(&self.decoded),
        }
    }

    /// Transfers backing storage without consuming or validating more events.
    /// The caller must retain its own record of whether traversal reached EOF.
    pub fn into_storage(self) -> Result<SyntaxOutput> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        Ok(SyntaxOutput { payload: self.input.into_payload(), decoded: self.decoded })
    }
    pub(crate) fn fail(&mut self, error: Error) -> Error {
        self.failure.get_or_insert(error).clone()
    }
}

/// Syntax backing storage, without a syntax or schema validation claim.
/// `finish::into_output` validates before returning this type; `into_storage`
/// deliberately permits callers to retain a partially traversed query input.
#[derive(Clone, Debug)]
pub struct SyntaxOutput {
    pub payload: Payload,
    pub decoded: Vec<u8>,
}

pub(crate) const fn syntax_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Syntax, offset, detail)
}
