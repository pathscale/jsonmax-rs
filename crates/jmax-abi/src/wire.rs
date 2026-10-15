//! Transport-independent envelope views and framing metadata.

use crate::span::{Span32, StrRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EnvelopeKind {
    EndpointRequest,
    EndpointImmediate,
    EndpointStream,
    EndpointError,
    EndpointLog,
    EndpointForwarded,
    EndpointClose,
    McpRequest,
    McpNotification,
    McpResponse,
    McpError,
}

/// Borrow-free envelope metadata referencing externally owned message storage.
///
/// Presence requirements depend on the envelope kind and are enforced by the
/// wire decoder. A missing field remains distinct from a present zero value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireEnvelopeView {
    pub kind: EnvelopeKind,
    pub method: Option<StrRef>,
    pub seq: Option<u64>,
    pub code: Option<u32>,
    pub params_span: Option<Span32>,
    pub result_span: Option<Span32>,
    pub error_span: Option<Span32>,
}

/// Decoded framing metadata, not a directly serializable memory layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramedHeader {
    pub payload_len: u32,
    pub frame_kind: u8,
}
