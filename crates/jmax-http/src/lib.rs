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
