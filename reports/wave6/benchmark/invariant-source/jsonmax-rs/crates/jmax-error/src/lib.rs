//! Bounded diagnostics and explicitly redacted adapters for canonical ABI errors.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::Span32;

pub mod codes;
pub mod paths;
pub mod diagnostic;
pub mod redaction;

pub const DEFAULT_MAX_PATH_SEGMENTS: u16 = 64;
pub const DEFAULT_MAX_PATH_BYTES: u32 = 4096;
pub const DEFAULT_MAX_DISPLAY_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    Syntax,
    Encoding,
    Number,
    Limit,
    Schema,
    Validation,
    Framing,
    Transport,
    Authentication,
    Cancelled,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathKind { Json, Schema }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathSegment { Key(String), Index(u32) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathLimits {
    pub max_segments: u16,
    /// Includes every slash and JSON Pointer escape in the encoded path.
    pub max_encoded_bytes: u32,
}

impl Default for PathLimits {
    fn default() -> Self {
        Self {
            max_segments: DEFAULT_MAX_PATH_SEGMENTS,
            max_encoded_bytes: DEFAULT_MAX_PATH_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedPath {
    pub(crate) kind: PathKind,
    pub(crate) limits: PathLimits,
    pub(crate) segments: Vec<PathSegment>,
    pub(crate) encoded_len: u32,
}

impl BoundedPath {
    pub const fn kind(&self) -> PathKind { self.kind }
    pub const fn limits(&self) -> PathLimits { self.limits }
    pub fn segments(&self) -> &[PathSegment] { &self.segments }
    pub const fn encoded_len(&self) -> u32 { self.encoded_len }
    pub fn is_empty(&self) -> bool { self.segments.is_empty() }
}

/// One-based byte columns; u64 counters include EOF after u32::MAX newlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    pub byte_offset: u32,
    pub line: u64,
    pub column: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextLimits {
    pub before: u16,
    pub after: u16,
}

impl Default for ContextLimits {
    fn default() -> Self { Self { before: 24, after: 24 } }
}

/// Owned raw bytes. Displaying these bytes requires an explicit redaction policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteContext {
    pub(crate) bytes: Vec<u8>,
    pub(crate) span: Span32,
    pub(crate) focus: u32,
}

impl ByteContext {
    pub fn bytes(&self) -> &[u8] { &self.bytes }
    pub const fn span(&self) -> Span32 { self.span }
    /// Offset relative to the context, possibly equal to its length at EOF.
    pub const fn focus(&self) -> u32 { self.focus }
}

/// Direct Debug output is for trusted use; use SafeError for external reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub error: Error,
    pub json_path: Option<BoundedPath>,
    pub schema_path: Option<BoundedPath>,
    pub location: Option<SourceLocation>,
    pub context: Option<ByteContext>,
}

impl Diagnostic {
    pub const fn new(error: Error) -> Self {
        Self { error, json_path: None, schema_path: None, location: None, context: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedactionPolicy {
    pub include_detail: bool,
    pub include_paths: bool,
    pub include_context: bool,
    /// Hard UTF-8 byte budget, including all labels and truncation markers.
    pub max_output_bytes: usize,
}

impl RedactionPolicy {
    pub const fn safe_default() -> Self {
        Self {
            include_detail: false,
            include_paths: false,
            include_context: false,
            max_output_bytes: DEFAULT_MAX_DISPLAY_BYTES,
        }
    }
}

impl Default for RedactionPolicy {
    fn default() -> Self { Self::safe_default() }
}

/// Debug must use the same redacted representation as Display.
#[derive(Clone, Copy)]
pub struct SafeDisplay<'a> {
    pub(crate) diagnostic: &'a Diagnostic,
    pub(crate) policy: RedactionPolicy,
}

/// Implements core::error::Error without exposing the underlying error as source.
#[derive(Clone, Copy)]
pub struct SafeError<'a> {
    pub(crate) diagnostic: &'a Diagnostic,
    pub(crate) policy: RedactionPolicy,
}

/// Shared error construction for bounded path mutations.
pub(crate) fn path_error(kind: PathKind, code: ErrorCode, detail: &'static str) -> Error {
    let source = match kind {
        PathKind::Json => ErrorSource::Syntax,
        PathKind::Schema => ErrorSource::SchemaCompile,
    };
    Error::new(code, source, 0, detail)
}
