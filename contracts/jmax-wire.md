# jmax-wire frozen contract, version 1

Shared types and traits are root-owned. Each wave module may import `super::*`.
The exact root source is included below. No runtime or network I/O is performed.
Canonical Result, Error, spans, proof, slots, and envelope types are ABI reexports.

## Wave 1: envelope.rs

```rust
pub fn decode_request(payload: Payload, limits: EnvelopeLimits) -> Result<OwnedEnvelope>;
pub fn decode_response(payload: Payload, limits: EnvelopeLimits) -> Result<OwnedEnvelope>;
pub fn decode_envelope(payload: Payload, limits: EnvelopeLimits) -> Result<OwnedEnvelope>;
```

Input is exactly one complete UTF-8 JSON object, with no trailing content other
than JSON whitespace, no duplicate decoded keys (including escaped aliases), and
no unknown top-level keys. Depth, per-object fields, decoded arena bytes, and raw
payload bytes are bounded by EnvelopeLimits before growth. All limits must be
nonzero. Integer fields use strict unquoted JSON integer lexemes, no exponent,
fraction, negative zero, or overflow. seq/index are u64; code is u32; error.code
is i32. Escaped strings and surrogate pairs must decode correctly.

The exact endpoint wire grammar is:

* Request: {"kind":"request","seq":u64,"code":u32,"params":object}, with
  optional `method`:string. Maps to EndpointRequest.
* Immediate: {"kind":"immediate","seq":u64,"code":u32,"result":value}.
* Stream: {"kind":"stream","seq":u64,"code":u32,"index":u64,"result":value}.
* Error: {"kind":"error","error":{"code":i32,"message":string,"retryable":bool}},
  with optional seq:u64 and code:u32.
* Log: {"kind":"log","level":string,"message":string}, optional seq:u64.
  level is one of trace, debug, info, warn, error.
* Forwarded: {"kind":"forwarded","seq":u64,"code":u32,"target":nonempty-string}.
* Close: {"kind":"close","reason":string}, optional seq:u64.

These map to the correspondingly named canonical Endpoint variants.
decode_request accepts only Request, decode_response rejects Request, and
 decode_envelope accepts all seven kinds. MCP variants are handled by jmax-mcp,
not accepted here. Optional means absent, never null. No result/error/params
combination outside its kind is accepted. Nested error objects are closed.
Opaque params/result values still receive full syntax, UTF-8, depth, and
 duplicate-key validation, but application keys are not treated as envelope keys.

OwnedEnvelope retains the original payload and decoded arena. view.method is a
StrRef into one of those owners, with exact Unicode scalar_len. params_span,
result_span, error_span cover complete raw JSON values, excluding surrounding
whitespace; other absent fields are None. seq/code reflect present fields.
Stream index and log/forward/close metadata remain accessible in the raw payload;
they are validated but not added to the frozen canonical WireEnvelopeView.
No schema ValidationProof is forged by envelope parsing. Methods return borrowed
views only for the lifetime of the owner.

## Wave 2: framed.rs

```rust
pub fn decode_header(bytes: &[u8], limits: &FrameLimits) -> Result<FramedHeader>;
pub fn encode_frame(frame: &Frame, limits: &FrameLimits, sink: &mut dyn ByteSink) -> Result<()>;
impl FrameDecoder {
    pub fn new(limits: FrameLimits) -> Result<Self>;
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize>;
    pub fn next_frame(&mut self) -> Result<Option<Frame>>;
    pub fn finish(&self) -> Result<()>;
    pub fn buffered_len(&self) -> usize;
    pub fn reset(&mut self);
}
```

Wire header is exactly 5 bytes: unsigned u32 LE payload length then u8 frame kind.
No flags, padding, magic, native endianness, or transport-specific types. Payload
length excludes header. decode_header reads the first five bytes and ignores any
following payload; fewer than five returns TransportIncomplete. It rejects zero
length, over-limit length, and kinds outside allowed_kinds. max_payload_bytes
must be nonzero and fit usize together with the header; duplicate allowed kinds
are invalid configuration; an empty kind allowlist deliberately denies all kinds.

FrameDecoder buffers at most one frame, including its header, and owns no borrowed
input. push accepts only the prefix needed for that frame, returns consumed input
bytes, and never silently drops a suffix. A full buffered frame causes push to
return Ok(0) until next_frame consumes it. Validate the header as soon as complete,
before accepting body bytes. Empty input is allowed. next_frame returns None
until complete; on success it returns an owned frame and empties the buffer.
Parse/limit failures set failed and clear retained input; subsequent push,
next_frame, and finish return FramingError until reset. reset clears failed and
buffer length, preserving limits. Retained input length and requested allocation
size never exceed the one-frame bound; allocator overhead is outside that bound. finish is Ok only for an empty nonfailed decoder; any partial header/body
is TransportIncomplete and is not discarded. buffered_len is current stored
bytes. encode_frame verifies actual length equals header.payload_len, validates
limits/kind, then writes header and original payload. It preflights length/kind
before writing; sink failure may leave a prefix and must propagate unchanged.

## Wave 3: dispatch.rs

```rust
pub fn resolve_route<'a>(services: &'a [ServiceSpec], code: u32, roles: &[String]) -> Result<Route<'a>>;
pub fn validate_params(
    request: &OwnedEnvelope, route: Route<'_>, validator: &dyn ParameterValidator,
) -> Result<ValidatedSlots>;
pub fn decode_params<T: JsonDecode + SchemaProvider>(slots: &ValidatedSlots) -> Result<T>;
pub fn dispatch(
    request: &OwnedEnvelope, services: &[ServiceSpec], context: RequestContext<'_>,
    validator: &dyn ParameterValidator, handler: &dyn EndpointHandler,
) -> Result<Box<dyn JsonEncode + Send + Sync>>;
```

Resolve validates unique service IDs/names, globally unique endpoint codes and
service/name identities, matching endpoint.service, and nonempty unique role
entries before lookup. Unknown code is ConstraintViolation. Public endpoints have
no required roles; others require exact any-role intersection, otherwise
AuthFailed. Duplicate metadata is DuplicateObjectKey. Do not expose a route to a
handler before authorization. Request kind must be EndpointRequest with seq,
code, and object params. route.endpoint.code must equal request code. If method
is present, it must equal `<service>.<endpoint>`; disagreement is ConstraintViolation.
Copy the exact raw params bytes into an owned Payload for validation. Validator
must validate the whole object with the endpoint params schema, reject unknown
and duplicate keys, and return slots retaining identical bytes; verify owner,
payload_len, and decoded_len before returning. Bad validator ownership is
InternalError. decode_params compares T::schema_fingerprint() with slots.proof
before invoking T::decode_slots; mismatch is SchemaImageInvalid. dispatch checks
cpu_units, retained_bytes, and timeout_ms are nonzero and retained_bytes covers
request payload length, otherwise BudgetExhausted, then resolves, validates,
and calls the handler exactly once. It is a synchronous boundary, so timeout is
metadata for the host and cannot preempt a handler. It never spawns or blocks.

## Wave 4: responses.rs

```rust
pub fn encode_response(response: &Response<'_>, sink: &mut dyn ByteSink) -> Result<()>;
pub fn response_payload(response: &Response<'_>, max_bytes: u32) -> Result<Payload>;
impl JsonEncode for Response<'_> {
    fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()>;
}
```

Encode the exact grammar from Wave 1 in documented field order, with kind first,
then seq, code, index, result/error or remaining variant fields as applicable.
Error nested order is code, message, retryable. Optional None seq/code fields
are omitted. Numbers are exact decimal integer text; strings use full JSON
escaping (root write_json_string). Reject invalid log levels or empty forwarded
target before any writes. Encoders supplied through JsonEncode must produce one
complete JSON value. Their errors propagate. response_payload uses BufferSink
and returns no partial result. Direct sink APIs may leave a prefix on sink or
nested encoder failure; callers must discard that prefix. No implicit newline.

## Errors, ownership, and dependencies

Malformed JSON uses the scanner/parser's canonical syntax code and actual byte
offset. Invalid envelopes use MissingRequiredField, UnknownObjectKey,
DuplicateObjectKey, TypeMismatch, InvalidNumber, NumberOverflow, or
ConstraintViolation as appropriate, source Wire for local failures. Unknown kind
is InvalidToken. Invalid framing metadata/configuration is FramingError,
over-limit bytes PayloadTooLarge, incomplete transport TransportIncomplete,
fallible allocation failure BudgetExhausted. Framing errors do not become syntax
errors. All arithmetic is checked before allocation or offset conversion. Spans
must fit u32 and stay within their owning buffers. No unchecked JSON fragments or
borrowed task-local arenas cross boundaries. ByteSink::reserve never increases
logical len; BufferSink errors leave existing bytes intact. Root
write_json_string is streaming and can leave a prefix if the sink fails.

Modules are acyclic: responses and envelope use only root helpers; framed uses
only root helpers; dispatch may call envelope public APIs. No std requirement,
runtime, socket, platform endianness, or global state. Allowed dependencies:
jmax-abi, jmax-schema-vm, jmax-cursor, jmax-buffer, jmax-encode,
jmax-endpoint-model. Do not rely on extra manifest dependencies.

## Exact shared root source

This is the authoritative shared public/private layout available to each wave.
Module-owned impls above extend these root-owned types without redeclaring them.

```rust
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
```
