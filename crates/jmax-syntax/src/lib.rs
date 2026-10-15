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
    pub escaped: bool,
}

/// Root is represented separately and does not consume container depth.
#[derive(Clone, Debug)]
pub struct Frame {
    pub(crate) kind: ContainerKind,
    pub(crate) phase: FramePhase,
    pub(crate) start_offset: u32,
    pub(crate) count: u32,
    pub(crate) seen_keys: Vec<Vec<u8>>,
}

impl Frame {
    pub const fn kind(&self) -> ContainerKind { self.kind }
    pub const fn phase(&self) -> FramePhase { self.phase }
    pub const fn start_offset(&self) -> u32 { self.start_offset }
    pub const fn count(&self) -> u32 { self.count }
}

/// Retains all input so previously emitted references survive further feeds.
/// Once an error is recorded, every mutating operation returns that error.
#[derive(Debug)]
pub struct SyntaxState {
    pub(crate) limits: Limits,
    pub(crate) input: Vec<u8>,
    pub(crate) decoded: Vec<u8>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) root_phase: FramePhase,
    pub(crate) byte_offset: u32,
    pub(crate) final_input: bool,
    pub(crate) finished: bool,
    pub(crate) failure: Option<Error>,
    pub(crate) pending: Option<PendingToken>,
}

impl SyntaxState {
    pub fn new(limits: Limits) -> Self {
        Self { limits, input: Vec::new(), decoded: Vec::new(), frames: Vec::new(),
            root_phase: FramePhase::RootStart, byte_offset: 0, final_input: false,
            finished: false, failure: None, pending: None }
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
        let bytes = match reference.source {
            SpanSource::RawPayload => reference.span.slice(&self.input)?,
            SpanSource::DecodedArena => reference.span.slice(&self.decoded)?,
        };
        core::str::from_utf8(bytes).ok()
    }
    pub(crate) fn fail(&mut self, error: Error) -> Error {
        self.failure.get_or_insert(error).clone()
    }
}

/// Completed syntax ownership, without any schema validation claim.
#[derive(Clone, Debug)]
pub struct SyntaxOutput {
    pub payload: Payload,
    pub decoded: Vec<u8>,
}

pub(crate) const fn syntax_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Syntax, offset, detail)
}
