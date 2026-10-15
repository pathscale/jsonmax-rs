# jmax-http frozen contract, version 1

The root owns shared message models, HttpTransport/BodyValidator/WebhookVerifier/
HostedHandler traits, bounded BodySink, and validated-body proof binding. Modules
import `super::*`. All module public functions below are exact. This crate owns
no socket, server, runtime, TLS configuration, retry policy, or credentials.
Host nago-http integration implements HttpTransport; its future must report
transport errors and complete decoded bodies faithfully. No executor or HTTP
runtime API is guessed in the root. This is a JSON body API: empty-body responses
and HEAD exchanges require a separate host API, not a fabricated JSON null.

## Wave 1: client.rs

```rust
pub async fn call<T, R>(
    transport: &T, validator: &dyn BodyValidator,
    request: OutgoingRequest, limits: HttpLimits,
) -> Result<ParsedMessage<R>>
where
    T: HttpTransport,
    R: JsonDecode + SchemaProvider;
pub async fn call_json<T, Q, R>(
    transport: &T, validator: &dyn BodyValidator, method: HttpMethod,
    uri: &str, headers: Vec<Header>, value: &Q, limits: HttpLimits,
) -> Result<ParsedMessage<R>>
where
    T: HttpTransport,
    Q: JsonEncode + SchemaProvider,
    R: JsonDecode + SchemaProvider;
```

call preflights limits, absolute URI, method (HEAD rejected), canonical headers,
body size and its existing ownership/proof consistency before invoking transport
exactly once. A publicly assembled OutgoingRequest must pass the same framing
checks as outgoing::request. Require exactly one JSON content type and exactly
one canonical Content-Length on outgoing messages. A validator-produced body
may not be modified or re-encoded after validation. Await HttpTransport::send,
then use incoming::decode_response with R's exact schema fingerprint. Transport
errors propagate unchanged. No retry, redirect follow, implicit method change,
body truncation, or error-to-success conversion. call_json builds a request via
outgoing::request using Q's schema, then delegates to call. No async CPU offload
is implied by call: validators must suit the host's workload; callers needing
CPU offload use incoming::decode_on explicitly. nago-http adapters preserve
cancellation and deadline behavior of their own futures rather than starting
untracked work. No detached tasks are spawned by this module.

## Wave 2: incoming.rs

```rust
pub fn check_headers(headers: &[Header], body_len: usize, limits: HttpLimits) -> Result<()>;
pub fn validate_uri(uri: &str, absolute: bool) -> Result<()>;
pub fn check_response(response: &IncomingResponse, limits: HttpLimits) -> Result<()>;
pub fn check_request(request: &IncomingRequest, limits: HttpLimits) -> Result<()>;
pub fn validate_response(
    response: IncomingResponse, validator: &dyn BodyValidator,
    schema_fingerprint: u64, limits: HttpLimits,
) -> Result<ValidatedBody>;
pub fn decode_response<T: JsonDecode + SchemaProvider>(
    response: IncomingResponse, validator: &dyn BodyValidator, limits: HttpLimits,
) -> Result<ParsedMessage<T>>;
pub fn decode_on<E, V, T>(
    executor: &E, validator: Arc<V>, response: IncomingResponse,
    limits: HttpLimits, lease: ScratchLease, cancellation: CancellationToken,
) -> Result<E::Task<Admitted<ParsedMessage<T>>>>>
where
    E: CpuExecutor,
    V: ParseValidator,
    T: JsonDecode + SchemaProvider + Send + 'static;
```

Use `alloc::sync::Arc` for decode_on. All incoming entry points first validate
limits with HttpLimits::effective_syntax. check_headers uses checked sums:
header storage bytes = 2 + sum(name.len + value.len + 4), counting colon-space
and CRLF. Enforce max_headers, max_header_bytes, and body length <= effective
syntax.max_payload_bytes. Header::new already validates token names and prevents
CR/LF/control injection; names are lowercase and comparisons case insensitive.
Unknown header fields may be repeated, but content-length, content-type, and
content-encoding must not be repeated, even with identical values.

A Content-Length, if present, must be a nonempty unsigned decimal digit string
surrounded only by optional ASCII space/tab, fit u64 and usize, and exactly
match decoded body length. No signs, comma lists, or arithmetic overflow. A
larger declared length is TransportIncomplete; smaller is FramingError. Leading
zeroes are accepted on input but normalized on output. Missing Content-Length
is allowed only because completion is explicitly supplied. Any Transfer-Encoding
header is FramingError: adapters must remove transfer framing metadata after
de-framing. Content-Encoding must be absent or identity; other encodings must
be decoded and metadata normalized by the transport adapter before entering.

Require exactly one Content-Type: application/json or application/<nonempty
valid-token>+json, ASCII case insensitive. Optional parameters may only be one
charset=utf-8, with OWS and optionally quoted utf-8; other charset values,
unknown/duplicate parameters, malformed token/quotes, or missing type are
TypeMismatch. Do not sniff content or accept JSON prefixes with extra values.
This strict boundary expects already decoded bytes, not HTTP wire chunks.

check_response requires completion Complete before decoding, status in
100..=599, then successful status 200..=299 excluding 204 and 205. Invalid status
or non-success is ConstraintViolation; a bodyless success is TypeMismatch.
Incomplete always yields TransportIncomplete regardless of status. Then check
headers/body limits. check_request likewise requires Complete, non-HEAD method,
valid origin-form or absolute URI, then header/body checks. Header framing errors
are never ignored on a nominal success. HEAD is UnsupportedSchemaFeature.

validate_uri requires ASCII and rejects controls, spaces, backslashes, fragments,
and invalid percent triplets. With absolute=true accept only lowercase http://
or https:// with a nonempty host and no userinfo. Parse a bracketed IPv6 host as
an actual IPv6 address; an optional decimal port must be 1..=65535. Unbracketed
host is ASCII letters/digits/hyphen/dot, has nonempty labels of at most 63 bytes,
no leading/trailing hyphen and total host length <=253; a trailing root dot is
allowed. No raw colon in an unbracketed host. With absolute=false also accept
origin-form beginning with a single slash, excluding `//` authority form. Paths
and query use RFC 3986 unreserved, sub-delimiter, percent-encoded characters and
legal path/query separators, with no raw disallowed bytes. Invalid URI is
ConstraintViolation. No DNS, IDNA conversion, network access, or normalization
that changes the caller's request target.

validate_response calls check_response then root validate_body with the exact
expected schema fingerprint. decode_response derives that fingerprint from T,
then calls T::decode_slots and transfers the proven owners into ParsedMessage<T>.
No proof is issued by HTTP. decode_on checks transport metadata before any CPU
submission, builds a ParseJob with the effective syntax limits, and calls
jmax_nagoya::submit::submit_typed. Admission and cooperative cancellation rules
are exactly the Nagoya contract; its Admitted wrapper keeps accounting held
through result retention. No mutable scratch/borrowed response body crosses the
CPU boundary. Preflight errors drop the supplied lease without executor use.

## Wave 3: outgoing.rs

```rust
pub fn encode_body<T: JsonEncode + SchemaProvider>(
    value: &T, validator: &dyn BodyValidator, limits: HttpLimits,
) -> Result<ValidatedBody>;
pub fn request<T: JsonEncode + SchemaProvider>(
    method: HttpMethod, uri: &str, headers: Vec<Header>, value: &T,
    validator: &dyn BodyValidator, limits: HttpLimits,
) -> Result<OutgoingRequest>;
pub fn response<T: JsonEncode + SchemaProvider>(
    status: u16, headers: Vec<Header>, value: &T,
    validator: &dyn BodyValidator, limits: HttpLimits,
) -> Result<OutgoingResponse>;
```

encode_body encodes exactly once into BodySink bounded by effective body cap,
then root validate_body checks the actual encoded bytes against
T::schema_fingerprint. The schema validator, not an encoder assertion, must
issue proof. Never validate one buffer and send another. No hidden trailing
newline, lossy string conversion, or nonfinite JSON number substitution.

request rejects HEAD, validates absolute URI, builds/validates body, and produces
canonical headers: preserve caller header order except drop any one supplied
Content-Length after verifying it exactly matches actual body length; reject
duplicate length/type/encoding and forbidden transfer/content encodings.
If Content-Type is absent append application/json; if supplied require a valid
JSON media type under incoming policy. Append one decimal Content-Length after
Content-Type handling. Check total final header limits. Invalid supplied lengths
are errors, not silently repaired. The same normalization applies to response,
which requires status 200..=299 excluding 204/205. Other statuses are
ConstraintViolation here; bodyless statuses are TypeMismatch. This typed success
builder does not fabricate HTTP error status bodies; hosted handlers may assemble
an OutgoingResponse with a validated error-schema body and status 400..=599.
Those responses are verified by hosted::dispatch before release. All byte lengths
refer to transmitted JSON UTF-8 bytes, not Unicode character count. On any error
return no partially assembled message and do not touch a network transport.

## Wave 4: hosted.rs

```rust
pub fn accept_request(
    request: IncomingRequest, verifier: Option<&dyn WebhookVerifier>,
    validator: &dyn BodyValidator, schema_fingerprint: u64, limits: HttpLimits,
) -> Result<HostedRequest>;
pub fn dispatch(
    request: IncomingRequest, verifier: Option<&dyn WebhookVerifier>,
    validator: &dyn BodyValidator, schema_fingerprint: u64,
    handler: &dyn HostedHandler, limits: HttpLimits,
) -> Result<OutgoingResponse>;
```

accept_request checks completion, URI, method, headers, and size, then, when
configured, verifies the exact unmodified body bytes and original headers before
JSON validation/normalization. Propagate verifier errors (AuthFailed or
SignatureInvalid preferred). No handler or JSON validator call occurs after a
verification failure. With verifier=None the host explicitly opts into an
unverified HTTP boundary; no authentication is invented. Then root validate_body
checks schema/ownership and constructs HostedRequest retaining method, URI,
headers and exact validated bytes. Validation never changes signed bytes.

 dispatch calls accept_request then the handler exactly once. Before returning,
verify handler response status (200..=599 excluding 204,205,304 and all 1xx),
canonical JSON headers with exactly one Content-Length and Content-Type, limits,
proof length consistency, decoded byte cap, and payload ownership. A typed
error-schema response may have 400..=599 status; this is data returned to the
host, not a local Err or forced success. Host handlers choose the response schema;
dispatch does not compare its fingerprint with the request schema. Invalid
handler output is a local error and is not transmitted. No listener, webhook
registration, network send, retry, or signature scheme is implemented here.
BodySink failed writes preserve its existing prefix; owned builders discard it.

## Cross-module invariants, errors, and ownership

The effective payload cap is min(HttpLimits.max_body_bytes,
syntax.max_payload_bytes); other syntax limits remain unchanged. Zero payload
caps, zero max_headers, max_header_bytes <2 or zero max_depth are invalid
configuration. All casts and additions are checked. Payloads must fit ABI u32
proof lengths. validate_body checks original byte equality, actual payload and
arena lengths against proof, expected fingerprint, and decoded arena cap.
It is not a substitute for checking HTTP transport completion. BodyValidator
must validate the complete JSON value and preserve the provided bytes.

Canonical local errors have source Transport and offset zero. Use FramingError
for malformed/ambiguous lengths or transport metadata, TransportIncomplete for
incomplete bodies, TypeMismatch for media/body-shape incompatibility,
PayloadTooLarge for body/header byte limits, BudgetExhausted for header count or
fallible allocation failure, ConstraintViolation for invalid limits/URI/status,
SchemaImageInvalid for fingerprint mismatch, DecodedArenaOverflow for excess
decoded bytes, InternalError for inconsistent proof/ownership. Preserve parser,
validator, encoder, verifier and transport errors without erasing offsets or
schema/field metadata. No ABI structs are redefined. ValidatedBody fields are
private and only actual validation paths construct it. Its raw_bytes owner is
never mutable through this API. Headers validate injection safety, not transport
completeness or body semantics.

Dependencies are acyclic: incoming uses root and Nagoya submit public functions;
outgoing uses incoming public metadata helpers and root; client uses outgoing
and incoming; hosted uses incoming and root. Each module can compile against
these frozen public declarations without private sibling imports. Allowed
dependencies: jmax-abi, jmax-buffer, jmax-schema-vm, jmax-encode, jmax-nagoya.
The root supports no_std + alloc; adapters supply concrete runtime behavior.

## Exact shared root source

All shared type/trait definitions, root implementations, and constants are frozen
here. Wave functions above must not redefine their storage or issue ABI proofs.

```rust
//! Complete-body HTTP boundaries independent of any client or server runtime.
#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use core::future::Future;
pub use jmax_abi::buffer::{ParsedMessage, Payload, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::ValidatedSlots;
pub use jmax_abi::syntax::Limits;
pub use jmax_abi::traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider};
pub use jmax_nagoya::{Admitted, CancellationToken, CpuExecutor, ParseJob, ParseValidator, ScratchLease};

pub mod client;
pub mod incoming;
pub mod outgoing;
pub mod hosted;

pub const CONTRACT_VERSION: u16 = 1;
pub const JSON_MEDIA_TYPE: &str = "application/json";
pub const DEFAULT_MAX_BODY_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

impl HttpMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    name: String,
    value: String,
}

impl Header {
    pub fn new(name: &str, value: &str) -> Result<Self> {
        fn token(byte: u8) -> bool {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        }
        if name.is_empty() || !name.bytes().all(token) {
            return Err(http_error(ErrorCode::FramingError, "invalid HTTP header name"));
        }
        if value.bytes().any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f) {
            return Err(http_error(ErrorCode::FramingError, "invalid HTTP header value"));
        }
        let mut normalized = String::new();
        let mut owned_value = String::new();
        normalized.try_reserve(name.len())
            .map_err(|_| http_error(ErrorCode::BudgetExhausted, "header name allocation failed"))?;
        owned_value.try_reserve(value.len())
            .map_err(|_| http_error(ErrorCode::BudgetExhausted, "header value allocation failed"))?;
        for byte in name.bytes() {
            normalized.push(char::from(byte.to_ascii_lowercase()));
        }
        owned_value.push_str(value);
        Ok(Self { name: normalized, value: owned_value })
    }

    pub fn name(&self) -> &str { &self.name }
    pub fn value(&self) -> &str { &self.value }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpLimits {
    pub max_body_bytes: u32,
    pub max_header_bytes: u32,
    pub max_headers: u16,
    pub syntax: Limits,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_header_bytes: 64 * 1024,
            max_headers: 128,
            syntax: Limits::strict_default(),
        }
    }
}

impl HttpLimits {
    /// Combines transport and syntax caps without widening either limit.
    pub fn effective_syntax(&self) -> Result<Limits> {
        if self.max_body_bytes == 0 || self.max_header_bytes < 2 || self.max_headers == 0
            || self.syntax.max_payload_bytes == 0 || self.syntax.max_depth == 0
        {
            return Err(http_error(ErrorCode::ConstraintViolation, "invalid HTTP limits"));
        }
        let mut syntax = self.syntax;
        syntax.max_payload_bytes = syntax.max_payload_bytes.min(self.max_body_bytes);
        Ok(syntax)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyCompletion {
    Complete,
    Incomplete,
}

/// Headers describe the de-framed, decoded body supplied here. Transport
/// adapters must normalize Content-Length after decoding content encodings.
#[derive(Clone, Debug)]
pub struct IncomingResponse {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Payload,
    pub completion: BodyCompletion,
}

#[derive(Clone, Debug)]
pub struct IncomingRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: Payload,
    pub completion: BodyCompletion,
}

/// Only schema-validation paths inside this crate construct this wrapper.
#[derive(Debug)]
pub struct ValidatedBody {
    pub(crate) slots: ValidatedSlots,
}

impl ValidatedBody {
    pub fn payload(&self) -> &Payload { &self.slots.payload }
    pub fn proof(&self) -> ValidationProof { self.slots.proof }
    pub fn slots(&self) -> &ValidatedSlots { &self.slots }
    pub fn into_slots(self) -> ValidatedSlots { self.slots }
}

#[derive(Debug)]
pub struct OutgoingRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: ValidatedBody,
}

#[derive(Debug)]
pub struct OutgoingResponse {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: ValidatedBody,
}

#[derive(Debug)]
pub struct HostedRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: ValidatedBody,
}

pub trait BodyValidator {
    /// Validate one complete JSON value using the requested fingerprint.
    /// The returned owner must retain exactly the supplied payload bytes.
    fn validate(&self, payload: Payload, schema_fingerprint: u64, limits: Limits) -> Result<ValidatedSlots>;
}

/// A nago-http adapter implements this trait without exposing runtime types to
/// JSON codecs. Completion is explicit even on a nominally successful exchange.
pub trait HttpTransport {
    type ResponseFuture<'a>: Future<Output = Result<IncomingResponse>> + Send + 'a
    where Self: 'a;

    fn send(&self, request: OutgoingRequest) -> Self::ResponseFuture<'_>;
}

pub trait WebhookVerifier {
    /// Verification sees the exact body bytes before any JSON decoding.
    fn verify(&self, headers: &[Header], body: &[u8]) -> Result<()>;
}

pub trait HostedHandler {
    fn handle(&self, request: HostedRequest) -> Result<OutgoingResponse>;
}

/// Binds an owned payload to the validator's actual schema proof. No HTTP
/// framing is implied; incoming modules must first verify body completion.
pub fn validate_body(
    payload: Payload,
    validator: &dyn BodyValidator,
    schema_fingerprint: u64,
    limits: HttpLimits,
) -> Result<ValidatedBody> {
    let syntax = limits.effective_syntax()?;
    if payload.len() > syntax.max_payload_bytes as usize {
        return Err(http_error(ErrorCode::PayloadTooLarge, "HTTP body exceeds validation limit"));
    }
    let original = payload.clone();
    let slots = validator.validate(payload, schema_fingerprint, syntax)?;
    check_proof(&slots, schema_fingerprint, original.as_slice())?;
    if slots.decoded.len() > syntax.max_decoded_bytes as usize {
        return Err(http_error(ErrorCode::DecodedArenaOverflow, "validator exceeded decoded arena limit"));
    }
    Ok(ValidatedBody { slots })
}

/// A bounded encoder target. Failed writes leave the existing prefix intact.
#[derive(Clone, Debug)]
pub struct BodySink {
    bytes: Vec<u8>,
    limit: u32,
}

impl BodySink {
    pub fn new(limit: u32) -> Self { Self { bytes: Vec::new(), limit } }
    pub fn as_bytes(&self) -> &[u8] { &self.bytes }
    pub fn into_payload(self) -> Payload { Payload::from_vec(self.bytes) }
}

impl ByteSink for BodySink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if additional > (self.limit as usize).saturating_sub(self.bytes.len()) {
            return Err(http_error(ErrorCode::PayloadTooLarge, "HTTP body exceeds limit"));
        }
        self.bytes.try_reserve(additional)
            .map_err(|_| http_error(ErrorCode::BudgetExhausted, "HTTP body allocation failed"))
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> usize { self.bytes.len() }
}

/// Common ownership/proof check after a validator returns. This does not issue
/// a proof; correctness of schema validation remains the validator's contract.
pub(crate) fn check_proof(slots: &ValidatedSlots, fingerprint: u64, original: &[u8]) -> Result<()> {
    if slots.proof.schema_fingerprint() != fingerprint {
        return Err(http_error(ErrorCode::SchemaImageInvalid, "response schema fingerprint mismatch"));
    }
    if slots.payload.as_slice() != original
        || slots.payload.len() != slots.proof.payload_len() as usize
        || slots.decoded.len() != slots.proof.decoded_len() as usize
    {
        return Err(http_error(ErrorCode::InternalError, "validator returned inconsistent ownership proof"));
    }
    Ok(())
}

pub(crate) const fn http_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Transport, 0, detail)
}
```
