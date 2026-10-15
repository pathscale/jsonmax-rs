//! Bounded path grammar, selector compilation, and owner-bound query results.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub mod parse;
pub mod compile;
pub mod query;
pub mod extensions;

pub use jmax_abi::buffer::Payload;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::Limits;
pub use jmax_cursor::{Segment, SelectedValue, SelectedValues, SelectorEdge, SelectorNode,
    SelectorPlan, SkipMode, ValueKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathLimits {
    pub max_path_bytes: u32,
    pub max_segments: u32,
    pub max_key_bytes: u32,
    pub max_paths: u32,
    pub max_trie_nodes: u32,
    pub max_matches: u32,
    pub max_visits: u32,
}

impl Default for PathLimits {
    fn default() -> Self {
        Self { max_path_bytes: 65_536, max_segments: 256, max_key_bytes: 4_096,
            max_paths: 4_096, max_trie_nodes: 65_536, max_matches: 4_096,
            max_visits: 1_000_000 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Path { pub(crate) segments: Vec<Segment> }

impl Path {
    pub fn root() -> Self { Self::default() }
    pub fn segments(&self) -> &[Segment] { &self.segments }
    pub fn from_segments(segments: Vec<Segment>, limits: PathLimits) -> Result<Self> {
        if segments.len() > limits.max_segments as usize {
            return Err(path_error(ErrorCode::BudgetExhausted, 0, "path segment limit exceeded"));
        }
        for segment in &segments {
            if let Segment::Key(key) = segment {
                if key.len() > limits.max_key_bytes as usize {
                    return Err(path_error(ErrorCode::BudgetExhausted, 0, "path key limit exceeded"));
                }
            }
        }
        Ok(Self { segments })
    }
}

/// Input path order is slot order, including repeated paths.
#[derive(Clone, Debug)]
pub struct CompiledPaths {
    pub(crate) paths: Vec<Path>,
    pub(crate) plan: SelectorPlan,
}

impl CompiledPaths {
    pub fn paths(&self) -> &[Path] { &self.paths }
    pub fn plan(&self) -> &SelectorPlan { &self.plan }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterValue { Null, Bool(bool), String(String), Number(String) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionSegment {
    Exact(Segment),
    /// Select every immediate array element or object member value.
    Wildcard,
    /// Select immediate array elements with a matching immediate scalar member.
    FilterEq { key: String, value: FilterValue },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPath {
    pub(crate) segments: Vec<ExtensionSegment>,
    pub(crate) limits: PathLimits,
}

impl ExtensionPath {
    pub fn segments(&self) -> &[ExtensionSegment] { &self.segments }
    pub const fn limits(&self) -> PathLimits { self.limits }
}

/// An extension query can yield multiple matches, in document order.
#[derive(Clone, Debug)]
pub struct QueryResults {
    pub(crate) payload: Payload,
    pub(crate) decoded: Vec<u8>,
    pub(crate) matches: Vec<SelectedValue>,
    pub(crate) fully_validated: bool,
}

impl QueryResults {
    pub fn payload(&self) -> &Payload { &self.payload }
    pub fn decoded_bytes(&self) -> &[u8] { &self.decoded }
    pub fn matches(&self) -> &[SelectedValue] { &self.matches }
    pub const fn fully_validated(&self) -> bool { self.fully_validated }
    pub fn raw(&self, index: usize) -> Option<&[u8]> {
        self.matches.get(index)?.span.slice(self.payload.as_slice())
    }
    pub fn string(&self, index: usize) -> Option<&str> {
        let value = self.matches.get(index)?;
        if value.kind != ValueKind::String { return None; }
        let reference = value.decoded?;
        let bytes = match reference.source {
            SpanSource::RawPayload => reference.span.slice(self.payload.as_slice())?,
            SpanSource::DecodedArena => reference.span.slice(&self.decoded)?,
        };
        core::str::from_utf8(bytes).ok()
    }
}

pub(crate) const fn path_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Cursor, offset, detail)
}
