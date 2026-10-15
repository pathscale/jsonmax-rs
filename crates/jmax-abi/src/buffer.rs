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

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> ParsedMessage<StrRef> {
        // The literal JSON string and its decoded value have been validated by
        // inspection. Fingerprint 7 identifies this test's string schema.
        let payload = Payload::from_slice(b"\"a\\u00e9\"");
        let decoded = "aé".as_bytes().to_vec().into_boxed_slice();
        let value = StrRef {
            span: Span32::new(0, 3), source: SpanSource::DecodedArena, scalar_len: 2,
        };
        // SAFETY: The literal payload is 9 bytes, the decoded arena is 3 bytes,
        // and the string reference covers two valid Unicode scalars.
        let proof = unsafe { ValidationProof::issue_unchecked(7, 9, 3) };
        // SAFETY: These are the exact immutable buffers and value proven above.
        unsafe { ParsedMessage::new_proven(payload, decoded, value, proof) }
    }

    #[test]
    fn payload_constructors_own_bytes_and_clone_without_copying() {
        let mut source = alloc::vec![1, 2, 3];
        let shared = Payload::from_slice(&source);
        source[0] = 9;
        assert_eq!(shared.as_slice(), &[1, 2, 3]);
        let owned = Payload::from_vec(source);
        assert_eq!(owned.as_slice(), &[9, 2, 3]);
        for payload in [shared, owned, Payload::Bytes(bytes::Bytes::from_static(b"abc"))] {
            let clone = payload.clone();
            assert_eq!(clone.as_slice(), payload.as_slice());
            assert_eq!(clone.as_slice().as_ptr(), payload.as_slice().as_ptr());
            assert_eq!(clone.len(), 3);
            assert!(!clone.is_empty());
            drop(payload);
            assert_eq!(clone.len(), 3);
        }
    }

    #[test]
    fn empty_payloads_support_both_storage_variants() {
        for payload in [Payload::from_slice(&[]), Payload::from_vec(Vec::new())] {
            assert_eq!(payload.len(), 0);
            assert!(payload.is_empty());
            assert_eq!(payload.as_slice(), &[]);
        }
    }

    #[test]
    fn proof_accessors_are_const_and_preserve_metadata() {
        // SAFETY: The literal `null` is valid for this test's fingerprint 0
        // schema, has length 4, and needs neither a decoded arena nor slots.
        const PROOF: ValidationProof = unsafe { ValidationProof::issue_unchecked(0, 4, 0) };
        const FINGERPRINT: u64 = PROOF.schema_fingerprint();
        const PAYLOAD_LEN: u32 = PROOF.payload_len();
        const DECODED_LEN: u32 = PROOF.decoded_len();
        assert_eq!((FINGERPRINT, PAYLOAD_LEN, DECODED_LEN), (0, 4, 0));
        let message = message();
        assert_eq!(message.proof().schema_fingerprint(), 7);
        assert_eq!(message.proof().payload_len() as usize, message.raw_bytes().len());
        assert_eq!(message.proof().decoded_len() as usize, message.decoded_bytes().len());
    }

    #[test]
    fn message_accessors_and_owned_value_extraction() {
        let message = message();
        assert_eq!(message.payload().as_slice(), b"\"a\\u00e9\"");
        assert_eq!(message.raw_bytes(), message.payload().as_slice());
        assert_eq!(message.decoded_bytes(), "aé".as_bytes());
        let value = *message.values();
        assert_eq!(message.resolve_str(value), Some("aé"));
        assert_eq!(message.into_values(), value);
    }

    #[test]
    fn resolves_raw_and_decoded_strings_and_empty_ranges() {
        let message = message();
        let raw = StrRef {
            span: Span32::new(1, 1), source: SpanSource::RawPayload, scalar_len: 1,
        };
        assert_eq!(message.resolve_str(raw), Some("a"));
        assert_eq!(message.resolve_raw_span(Span32::new(2, 6)), Some(&b"\\u00e9"[..]));
        for (source, offset) in [(SpanSource::RawPayload, 9), (SpanSource::DecodedArena, 3)] {
            assert_eq!(message.resolve_str(StrRef {
                span: Span32::new(offset, 0), source, scalar_len: 0,
            }), Some(""));
        }
    }

    #[test]
    fn arbitrary_references_cannot_bypass_bounds_or_utf8_checks() {
        let message = message();
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for span in [Span32::new(10, 0), Span32::new(0, 10), Span32::new(u32::MAX, 2)] {
                assert_eq!(message.resolve_str(StrRef { span, source, scalar_len: 0 }), None);
            }
        }
        for span in [Span32::new(1, 1), Span32::new(2, 1), Span32::new(0, 2)] {
            assert_eq!(message.resolve_str(StrRef {
                span, source: SpanSource::DecodedArena, scalar_len: 1,
            }), None);
        }
        assert_eq!(message.resolve_raw_span(Span32::new(9, 1)), None);
        assert_eq!(message.resolve_raw_span(Span32::new(u32::MAX, 1)), None);
        assert_eq!(message.resolve_raw_span(Span32::new(9, 0)), Some(&[][..]));
    }

    #[test]
    fn unvalidated_span_preserves_token_metadata() {
        let span = UnvalidatedSpan { span: Span32::new(2, 3), token_kind: u8::MAX };
        let copied = span;
        assert_eq!(copied, span);
        assert_eq!(copied.token_kind, u8::MAX);
    }
}
