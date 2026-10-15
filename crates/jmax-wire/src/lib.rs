//! Owned envelope boundaries, bounded framing, and transport-neutral dispatch.
#![no_std]

extern crate alloc;

use alloc::{boxed::Box, string::String, vec::Vec};
pub use jmax_abi::buffer::{Payload, ValidationProof};
pub use jmax_abi::endpoint::{EndpointSchemaSpec, ServiceSpec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::ValidatedSlots;
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::Limits;
pub use jmax_abi::traits::{ByteSink, JsonDecode, JsonEncode, ResourceBudget, SchemaProvider};
pub use jmax_abi::wire::{EnvelopeKind, FramedHeader, WireEnvelopeView};

pub mod envelope;
pub mod framed;
pub mod dispatch;
pub mod responses;

pub const CONTRACT_VERSION: u16 = 1;
/// Four little-endian length bytes followed by one kind byte.
pub const FRAME_HEADER_BYTES: usize = 5;
pub const DEFAULT_MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvelopeLimits {
    pub max_payload_bytes: u32,
    pub max_decoded_bytes: u32,
    pub max_depth: u16,
    pub max_fields: u32,
}

impl Default for EnvelopeLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_decoded_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_depth: 128,
            max_fields: 4096,
        }
    }
}

/// Parsing owns both address spaces used by the canonical envelope view.
#[derive(Clone, Debug)]
pub struct OwnedEnvelope {
    pub(crate) payload: Payload,
    pub(crate) decoded: Box<[u8]>,
    pub(crate) view: WireEnvelopeView,
}

impl OwnedEnvelope {
    pub fn payload(&self) -> &Payload { &self.payload }
    pub fn decoded_bytes(&self) -> &[u8] { &self.decoded }
    pub fn view(&self) -> WireEnvelopeView { self.view }
    pub fn resolve(&self, span: Span32) -> Option<&[u8]> { span.slice(self.payload.as_slice()) }
    pub fn method(&self) -> Option<&str> {
        let reference = self.view.method?;
        let bytes = match reference.source {
            SpanSource::RawPayload => self.payload.as_slice(),
            SpanSource::DecodedArena => &self.decoded,
        };
        core::str::from_utf8(reference.span.slice(bytes)?).ok()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_payload_bytes: u32,
    /// An empty list denies every frame kind.
    pub allowed_kinds: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub header: FramedHeader,
    pub payload: Payload,
}

#[derive(Debug)]
pub struct FrameDecoder {
    pub(crate) limits: FrameLimits,
    pub(crate) buffer: Vec<u8>,
    pub(crate) failed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RequestContext<'a> {
    pub roles: &'a [String],
    pub budget: ResourceBudget,
}

#[derive(Clone, Copy, Debug)]
pub struct Route<'a> {
    pub service: &'a ServiceSpec,
    pub endpoint: &'a EndpointSchemaSpec,
}

/// Implementations validate the entire parameter object against the endpoint.
pub trait ParameterValidator {
    fn validate(&self, endpoint: &EndpointSchemaSpec, payload: Payload) -> Result<ValidatedSlots>;
}

/// Results remain typed encoders; no unchecked raw JSON crosses this boundary.
pub trait EndpointHandler {
    fn handle(
        &self,
        route: Route<'_>,
        params: ValidatedSlots,
        context: RequestContext<'_>,
    ) -> Result<Box<dyn JsonEncode + Send + Sync>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcError<'a> {
    pub code: i32,
    pub message: &'a str,
    pub retryable: bool,
}

pub enum Response<'a> {
    Immediate { seq: u64, code: u32, result: &'a dyn JsonEncode },
    Stream { seq: u64, code: u32, index: u64, result: &'a dyn JsonEncode },
    Error { seq: Option<u64>, code: Option<u32>, error: RpcError<'a> },
    Log { seq: Option<u64>, level: &'a str, message: &'a str },
    Forwarded { seq: u64, code: u32, target: &'a str },
    Close { seq: Option<u64>, reason: &'a str },
}

/// Fallible, bounded output shared with protocol adapters.
#[derive(Clone, Debug)]
pub struct BufferSink {
    bytes: Vec<u8>,
    max_bytes: u32,
}

impl BufferSink {
    pub fn new(max_bytes: u32) -> Self { Self { bytes: Vec::new(), max_bytes } }
    pub fn as_bytes(&self) -> &[u8] { &self.bytes }
    pub fn into_payload(self) -> Payload { Payload::from_vec(self.bytes) }
    pub fn into_vec(self) -> Vec<u8> { self.bytes }
    pub fn remaining(&self) -> usize { (self.max_bytes as usize).saturating_sub(self.bytes.len()) }
}

impl ByteSink for BufferSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if additional > self.remaining() {
            return Err(wire_error(ErrorCode::PayloadTooLarge, "encoded payload exceeds limit"));
        }
        self.bytes.try_reserve(additional)
            .map_err(|_| wire_error(ErrorCode::BudgetExhausted, "output allocation failed"))
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> usize { self.bytes.len() }
}

/// Writes a complete JSON string, including quotes, without temporary allocation.
pub fn write_json_string(value: &str, sink: &mut dyn ByteSink) -> Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    sink.write_byte(b'"')?;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        let escape: Option<&[u8]> = match byte {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            8 => Some(b"\\b"),
            12 => Some(b"\\f"),
            _ => None,
        };
        if escape.is_some() || byte < 0x20 {
            sink.write_bytes(&value.as_bytes()[start..index])?;
            if let Some(escaped) = escape {
                sink.write_bytes(escaped)?;
            } else {
                sink.write_bytes(&[b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]])?;
            }
            start = index + 1;
        }
    }
    sink.write_bytes(&value.as_bytes()[start..])?;
    sink.write_byte(b'"')
}

pub fn role_allowed(endpoint: &EndpointSchemaSpec, roles: &[String]) -> bool {
    endpoint.roles.is_empty() || endpoint.roles.iter().any(|required| roles.contains(required))
}

pub(crate) const fn wire_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Wire, 0, detail)
}
