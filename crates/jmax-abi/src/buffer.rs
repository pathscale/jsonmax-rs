//! Shared payload ownership and validated message storage.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use crate::span::{Span32, SpanSource, StrRef};

#[derive(Clone, Debug)]
pub enum Payload {
    Shared(Arc<[u8]>),
    Bytes(bytes::Bytes),
}

impl Payload {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Shared(bytes) => bytes.as_ref(),
            Self::Bytes(bytes) => bytes.as_ref(),
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    /// Copies borrowed bytes into shared ownership.
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self::Shared(Arc::from(bytes))
    }

    /// Transfers the vector into reference-counted byte storage.
    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self::Bytes(bytes::Bytes::from(vec))
    }
}

/// Evidence issued by a validator for specific immutable buffers and values.
///
/// Lengths and a fingerprint alone do not establish validity. The fields are
/// private so safe code cannot manufacture evidence by filling in metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidationProof {
    schema_fingerprint: u64,
    payload_len: u32,
    decoded_len: u32,
}

impl ValidationProof {
    /// Issues evidence after validation has completed successfully.
    ///
    /// # Safety
    /// The caller must have validated the payload, decoded arena, and associated
    /// values against the schema identified by `schema_fingerprint`. Buffer
    /// lengths must exactly match the supplied lengths without truncation. All
    /// stored references must resolve within the appropriate buffer, and string
    /// references must identify valid UTF-8 with accurate scalar counts. The
    /// proof may only be used with those validated contents and values, or exact
    /// copies preserving their validation invariants.
    pub const unsafe fn issue_unchecked(
        schema_fingerprint: u64,
        payload_len: u32,
        decoded_len: u32,
    ) -> Self {
        Self { schema_fingerprint, payload_len, decoded_len }
    }

    pub const fn schema_fingerprint(&self) -> u64 {
        self.schema_fingerprint
    }

    pub const fn payload_len(&self) -> u32 {
        self.payload_len
    }

    pub const fn decoded_len(&self) -> u32 {
        self.decoded_len
    }
}

#[derive(Debug)]
pub struct ParsedMessage<T> {
    payload: Payload,
    decoded: Box<[u8]>,
    values: T,
    proof: ValidationProof,
}

impl<T> ParsedMessage<T> {
    /// Combines already validated values with their immutable backing storage.
    ///
    /// # Safety
    /// `proof` must have been issued for precisely these payload bytes, decoded
    /// bytes, and values under its schema fingerprint. Its lengths must match
    /// the buffers exactly. References in `values` must remain valid for these
    /// buffers, including bounds, UTF-8, and scalar counts for strings. Any
    /// interior mutation of `values` must preserve all validation invariants.
    pub unsafe fn new_proven(
        payload: Payload,
        decoded: Box<[u8]>,
        values: T,
        proof: ValidationProof,
    ) -> Self {
        Self { payload, decoded, values, proof }
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn raw_bytes(&self) -> &[u8] {
        self.payload.as_slice()
    }

    pub fn decoded_bytes(&self) -> &[u8] {
        &self.decoded
    }

    pub fn values(&self) -> &T {
        &self.values
    }

    /// Extracts values, dropping the backing storage and its validation proof.
    /// Any span values must be resolved before consuming the message, or used
    /// with separately retained copies of the same backing bytes.
    pub fn into_values(self) -> T {
        self.values
    }

    pub fn proof(&self) -> ValidationProof {
        self.proof
    }

    /// Resolves a range with checked bounds and UTF-8, even for a reference
    /// supplied independently of the validated values. `scalar_len` is metadata
    /// and is not used to index or truncate the returned string.
    pub fn resolve_str(&self, r: StrRef) -> Option<&str> {
        let bytes = match r.source {
            SpanSource::RawPayload => self.raw_bytes(),
            SpanSource::DecodedArena => self.decoded_bytes(),
        };
        core::str::from_utf8(r.span.slice(bytes)?).ok()
    }

    pub fn resolve_raw_span(&self, s: Span32) -> Option<&[u8]> {
        s.slice(self.raw_bytes())
    }
}

/// A range discovered before semantic validation has completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnvalidatedSpan {
    pub span: Span32,
    pub token_kind: u8,
}
