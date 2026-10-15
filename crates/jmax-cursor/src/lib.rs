//! Forward event traversal and shared extraction with explicit input ownership.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub mod walk;
pub mod skip;
pub mod select;
pub mod resume;

pub use jmax_abi::buffer::{Payload, UnvalidatedSpan};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::{CursorState, Event, Limits, ScalarKind, ScanStep};
pub use jmax_syntax::SyntaxState;

pub const MAX_SELECTOR_NODES: usize = 65_536;
pub const MAX_SELECTION_SLOTS: u32 = 65_536;

/// Query permits partial traversal; strict requires validation through EOF.
/// Both modes validate every token they actually consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipMode { Strict, Query }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueKind { Object, Array, String, Number, Bool, Null }

/// A completed value. Container spans include opening and closing delimiters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedValue {
    pub kind: ValueKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipStep {
    NeedMore,
    Complete { value: SelectedValue, validated: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SkipProgress {
    pub start: u32,
    pub kind: ValueKind,
    pub depth: u16,
    pub mode: SkipMode,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment { Key(String), Index(u32) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorEdge {
    pub segment: Segment,
    pub target: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectorNode {
    pub edges: Vec<SelectorEdge>,
    pub slots: Vec<u32>,
}

/// An acyclic, reachable selector graph rooted at node zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorPlan {
    pub(crate) nodes: Vec<SelectorNode>,
    pub(crate) slot_count: u32,
}

impl SelectorPlan {
    /// Targets must be greater than their source node. Each slot occurs once.
    pub fn new(nodes: Vec<SelectorNode>, slot_count: u32) -> Result<Self> {
        if nodes.is_empty() || nodes.len() > MAX_SELECTOR_NODES
            || slot_count > MAX_SELECTION_SLOTS {
            return Err(cursor_error(ErrorCode::ConstraintViolation, 0,
                "selector graph size is outside supported bounds"));
        }
        let mut reachable = Vec::new();
        let mut slots = Vec::new();
        reachable.try_reserve_exact(nodes.len()).map_err(|_| cursor_error(
            ErrorCode::BudgetExhausted, 0, "selector validation allocation failed"))?;
        slots.try_reserve_exact(slot_count as usize).map_err(|_| cursor_error(
            ErrorCode::BudgetExhausted, 0, "selector slot allocation failed"))?;
        reachable.resize(nodes.len(), false);
        slots.resize(slot_count as usize, false);
        reachable[0] = true;
        for (index, node) in nodes.iter().enumerate() {
            if !reachable[index] {
                return Err(cursor_error(ErrorCode::ConstraintViolation, 0,
                    "selector contains an unreachable node"));
            }
            let mut segments: Vec<&Segment> = Vec::new();
            segments.try_reserve_exact(node.edges.len()).map_err(|_| cursor_error(
                ErrorCode::BudgetExhausted, 0, "selector edge validation allocation failed"))?;
            segments.extend(node.edges.iter().map(|edge| &edge.segment));
            segments.sort_unstable();
            if segments.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(cursor_error(ErrorCode::ConstraintViolation, 0,
                    "selector contains duplicate sibling edges"));
            }
            for edge in &node.edges {
                let target = edge.target as usize;
                if target <= index || target >= nodes.len() {
                    return Err(cursor_error(ErrorCode::ConstraintViolation, 0,
                        "selector edge is cyclic or out of bounds"));
                }
                reachable[target] = true;
            }
            for &slot in &node.slots {
                let seen = slots.get_mut(slot as usize).ok_or_else(|| cursor_error(
                    ErrorCode::ConstraintViolation, 0, "selector slot is out of bounds"))?;
                if *seen {
                    return Err(cursor_error(ErrorCode::ConstraintViolation, 0,
                        "selector slot is assigned more than once"));
                }
                *seen = true;
            }
        }
        if slots.iter().any(|seen| !seen) {
            return Err(cursor_error(ErrorCode::ConstraintViolation, 0,
                "selector slot has no terminal node"));
        }
        Ok(Self { nodes, slot_count })
    }
    pub fn nodes(&self) -> &[SelectorNode] { &self.nodes }
    pub const fn slot_count(&self) -> u32 { self.slot_count }
}

#[derive(Debug)]
pub struct Cursor {
    pub(crate) syntax: SyntaxState,
    pub(crate) mode: SkipMode,
    pub(crate) current: Option<Event>,
    pub(crate) skipping: Option<SkipProgress>,
}

impl Cursor {
    pub fn new(limits: Limits, mode: SkipMode) -> Self {
        Self { syntax: SyntaxState::new(limits), mode, current: None, skipping: None }
    }
    pub const fn mode(&self) -> SkipMode { self.mode }
    pub fn state(&self) -> CursorState { self.syntax.state() }
    pub const fn current(&self) -> Option<Event> { self.current }
    pub fn raw_bytes(&self) -> &[u8] { self.syntax.raw_bytes() }
    pub fn decoded_bytes(&self) -> &[u8] { self.syntax.decoded_bytes() }
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        self.syntax.resolve_str(reference)
    }
}

/// Move-only continuation retaining parser buffers, current event, and limits.
#[derive(Debug)]
pub struct SuspendedCursor { pub(crate) cursor: Cursor }

#[derive(Clone, Debug)]
pub struct CursorOutput {
    pub payload: Payload,
    pub decoded: Vec<u8>,
    pub fully_validated: bool,
}

/// Results cannot outlive the payload and arena that own their references.
#[derive(Clone, Debug)]
pub struct SelectedValues {
    pub(crate) payload: Payload,
    pub(crate) decoded: Vec<u8>,
    pub(crate) slots: Vec<Option<SelectedValue>>,
    pub(crate) fully_validated: bool,
}

impl SelectedValues {
    pub fn payload(&self) -> &Payload { &self.payload }
    pub fn decoded_bytes(&self) -> &[u8] { &self.decoded }
    pub fn slots(&self) -> &[Option<SelectedValue>] { &self.slots }
    pub const fn fully_validated(&self) -> bool { self.fully_validated }
    pub fn raw(&self, slot: u32) -> Option<&[u8]> {
        self.slots.get(slot as usize)?.as_ref()?.span.slice(self.payload.as_slice())
    }
    pub fn string(&self, slot: u32) -> Option<&str> {
        let value = self.slots.get(slot as usize)?.as_ref()?;
        if value.kind != ValueKind::String { return None; }
        let reference = value.decoded?;
        let bytes = match reference.source {
            SpanSource::RawPayload => reference.span.slice(self.payload.as_slice())?,
            SpanSource::DecodedArena => reference.span.slice(&self.decoded)?,
        };
        core::str::from_utf8(bytes).ok()
    }
}

pub(crate) const fn cursor_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Cursor, offset, detail)
}
