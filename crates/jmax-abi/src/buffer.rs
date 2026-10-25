//! Shared payload ownership and validated message storage.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{fmt, ops::Range};
use crate::error::{Error, ErrorCode, ErrorSource, Result};
use crate::span::{Span32, SpanSource, StrRef};

/// Any immutable reference-counted byte owner (`Vec<u8>`, `Box<[u8]>`, custom buffers)
/// that can be retained inside a [`Payload`] without copying its bytes or allocating
/// a wrapper control block (`Arc<T>` coerces directly to `Arc<dyn ByteOwner>`).
pub trait ByteOwner: Send + Sync + 'static {
    fn as_bytes(&self) -> &[u8];
}

impl<T: AsRef<[u8]> + Send + Sync + 'static> ByteOwner for T {
    fn as_bytes(&self) -> &[u8] {
        self.as_ref()
    }
}

#[derive(Clone)]
enum SharedBacking {
    Slice(Arc<[u8]>),
    Owner(Arc<dyn ByteOwner>),
}

impl SharedBacking {
    fn as_backing_slice(&self) -> &[u8] {
        match self {
            Self::Slice(bytes) => bytes.as_ref(),
            Self::Owner(owner) => owner.as_bytes(),
        }
    }
}

/// Owner-preserving subview into an `Arc<[u8]>` or generic `Arc<T: ByteOwner>`.
#[derive(Clone)]
pub struct SharedByteView {
    backing: SharedBacking,
    offset: u32,
    len: u32,
}

impl fmt::Debug for SharedByteView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedByteView")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .field("backing_len", &self.backing_len())
            .finish()
    }
}

impl SharedByteView {
    pub fn from_arc_slice(bytes: Arc<[u8]>, span: Span32) -> Result<Self> {
        if span.slice(bytes.as_ref()).is_none() {
            return Err(Error::new(
                ErrorCode::InvalidToken,
                ErrorSource::Decode,
                span.offset,
                "shared byte view span is out of bounds",
            ));
        }
        Ok(Self {
            backing: SharedBacking::Slice(bytes),
            offset: span.offset,
            len: span.len,
        })
    }

    pub fn from_arc_owner<T: AsRef<[u8]> + Send + Sync + 'static>(
        owner: Arc<T>,
        span: Span32,
    ) -> Result<Self> {
        let slice = (*owner).as_ref();
        if u32::try_from(slice.len()).is_err() || span.slice(slice).is_none() {
            return Err(Error::new(
                ErrorCode::PayloadTooLarge,
                ErrorSource::Decode,
                span.offset,
                "shared byte owner exceeds ABI range or span is out of bounds",
            ));
        }
        let owner: Arc<dyn ByteOwner> = owner;
        Ok(Self {
            backing: SharedBacking::Owner(owner),
            offset: span.offset,
            len: span.len,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        let backing = self.backing.as_backing_slice();
        let start = self.offset as usize;
        let end = start.saturating_add(self.len as usize);
        backing.get(start..end).unwrap_or(&[])
    }

    pub fn backing_slice(&self) -> &[u8] {
        self.backing.as_backing_slice()
    }

    pub fn backing_len(&self) -> usize {
        self.backing_slice().len()
    }

    pub const fn visible_span_in_backing(&self) -> Span32 {
        Span32::new(self.offset, self.len)
    }

    pub fn try_slice(&self, span: Span32) -> Result<Self> {
        let visible = self.as_slice();
        if span.slice(visible).is_none() {
            return Err(Error::new(
                ErrorCode::InvalidToken,
                ErrorSource::Decode,
                span.offset,
                "subview span is out of bounds",
            ));
        }
        let offset = self.offset.checked_add(span.offset).ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidToken,
                ErrorSource::Decode,
                span.offset,
                "subview offset overflow",
            )
        })?;
        Ok(Self {
            backing: self.backing.clone(),
            offset,
            len: span.len,
        })
    }
}

/// Allocation and visible-range provenance for a [`Payload`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackingIdentity {
    pub base_ptr: usize,
    pub visible_ptr: usize,
    pub visible_len: u32,
    pub backing_len: u64,
    pub exact_backing: bool,
    pub visible_span_in_backing: Span32,
}

#[derive(Clone, Debug)]
pub enum Payload {
    Shared(Arc<[u8]>),
    Bytes(bytes::Bytes),
    SharedView(SharedByteView),
}

impl Payload {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Shared(bytes) => bytes.as_ref(),
            Self::Bytes(bytes) => bytes.as_ref(),
            Self::SharedView(view) => view.as_slice(),
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

    /// Transfers the vector into reference-counted byte storage without copying.
    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self::Bytes(bytes::Bytes::from(vec))
    }

    /// Retains a generic `Arc<T>` byte owner without copying its bytes or allocating
    /// a second wrapper allocation (`Arc<T>` coerces to `Arc<dyn ByteOwner>`).
    pub fn from_arc_owner<T: AsRef<[u8]> + Send + Sync + 'static>(owner: Arc<T>) -> Result<Self> {
        let len = u32::try_from((*owner).as_ref().len()).map_err(|_| {
            Error::new(
                ErrorCode::PayloadTooLarge,
                ErrorSource::Decode,
                u32::MAX,
                "arc owner length exceeds ABI range",
            )
        })?;
        Ok(Self::SharedView(SharedByteView::from_arc_owner(
            owner,
            Span32::new(0, len),
        )?))
    }

    /// Produces an owner-preserving subview for `span` without copying bytes.
    pub fn try_slice(&self, span: Span32) -> Result<Self> {
        let visible = self.as_slice();
        if span.slice(visible).is_none() {
            return Err(Error::new(
                ErrorCode::InvalidToken,
                ErrorSource::Decode,
                span.offset,
                "payload subview span is out of bounds",
            ));
        }
        let start = span.offset as usize;
        let end = start + (span.len as usize);
        match self {
            Self::Bytes(bytes) => Ok(Self::Bytes(bytes.slice(start..end))),
            Self::Shared(arc) => {
                if start == 0 && end == arc.len() {
                    Ok(Self::Shared(Arc::clone(arc)))
                } else {
                    Ok(Self::SharedView(SharedByteView::from_arc_slice(
                        Arc::clone(arc),
                        span,
                    )?))
                }
            }
            Self::SharedView(view) => Ok(Self::SharedView(view.try_slice(span)?)),
        }
    }

    /// Produces an owner-preserving subview for `range` without copying bytes.
    pub fn slice(&self, range: Range<usize>) -> Result<Self> {
        let invalid = || {
            Error::new(
                ErrorCode::InvalidToken,
                ErrorSource::Decode,
                u32::try_from(range.start).unwrap_or(u32::MAX),
                "payload slice range is out of bounds",
            )
        };
        if range.start > range.end || range.end > self.len() {
            return Err(invalid());
        }
        let offset = u32::try_from(range.start).map_err(|_| invalid())?;
        let len = u32::try_from(range.end - range.start).map_err(|_| invalid())?;
        self.try_slice(Span32::new(offset, len))
    }

    /// Reports backing pointer, visible pointer, and backing length metadata.
    pub fn backing_identity(&self) -> BackingIdentity {
        let visible = self.as_slice();
        let visible_len = u32::try_from(visible.len()).unwrap_or(u32::MAX);
        let visible_ptr = visible.as_ptr() as usize;
        match self {
            Self::Shared(arc) => BackingIdentity {
                base_ptr: arc.as_ptr() as usize,
                visible_ptr,
                visible_len,
                backing_len: arc.len() as u64,
                exact_backing: true,
                visible_span_in_backing: Span32::new(0, visible_len),
            },
            Self::SharedView(view) => {
                let backing = view.backing_slice();
                BackingIdentity {
                    base_ptr: backing.as_ptr() as usize,
                    visible_ptr,
                    visible_len,
                    backing_len: backing.len() as u64,
                    exact_backing: true,
                    visible_span_in_backing: view.visible_span_in_backing(),
                }
            }
            Self::Bytes(bytes) => BackingIdentity {
                base_ptr: visible_ptr,
                visible_ptr,
                visible_len,
                backing_len: bytes.len() as u64,
                exact_backing: false,
                visible_span_in_backing: Span32::new(0, visible_len),
            },
        }
    }

    /// Lower bound on retained backing bytes (`Exact` for `Shared`/`SharedView`, visible len for `Bytes`).
    pub fn backing_len_lower_bound(&self) -> usize {
        match self {
            Self::Shared(arc) => arc.len(),
            Self::SharedView(view) => view.backing_len(),
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    /// Exact backing byte length when known (`Shared` and `SharedView`).
    pub fn exact_backing_len(&self) -> Option<usize> {
        match self {
            Self::Shared(arc) => Some(arc.len()),
            Self::SharedView(view) => Some(view.backing_len()),
            Self::Bytes(_) => None,
        }
    }

    /// True if `other`'s visible slice lies within the same backing memory range.
    pub fn shares_backing_with(&self, other: &Self) -> bool {
        let a = self.backing_identity();
        let b = other.backing_identity();
        if a.exact_backing && b.exact_backing && a.base_ptr == b.base_ptr && a.backing_len == b.backing_len {
            return true;
        }
        let a_end = a.visible_ptr.saturating_add(a.visible_len as usize);
        let b_end = b.visible_ptr.saturating_add(b.visible_len as usize);
        if a.visible_len == 0 && b.visible_len == 0 {
            return a.visible_ptr == b.visible_ptr;
        }
        (b.visible_ptr >= a.visible_ptr && b_end <= a_end)
            || (a.visible_ptr >= b.visible_ptr && a_end <= b_end)
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

/// Capability token proving that a specific immutable byte range and policy set
/// have already been strictly validated.
///
/// Unlike a plain `(fingerprint, len)` tuple, this binds the exact visible pointer,
/// base allocation pointer, visible byte length, decoded byte length, schema
/// fingerprint, and policy/limits fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidationCapability {
    base_ptr: usize,
    visible_ptr: usize,
    visible_len: u32,
    decoded_len: u32,
    schema_fingerprint: u64,
    policy_fingerprint: u64,
}

impl ValidationCapability {
    /// Issues a capability for an immutable owner/slice after strict validation.
    ///
    /// # Safety
    /// The caller must have strictly validated the immutable bytes at
    /// `visible_ptr..visible_ptr + visible_len` (within `base_ptr`) under
    /// `schema_fingerprint` and `policy_fingerprint`.
    pub const unsafe fn issue_unchecked(
        base_ptr: usize,
        visible_ptr: usize,
        visible_len: u32,
        decoded_len: u32,
        schema_fingerprint: u64,
        policy_fingerprint: u64,
    ) -> Self {
        Self {
            base_ptr,
            visible_ptr,
            visible_len,
            decoded_len,
            schema_fingerprint,
            policy_fingerprint,
        }
    }

    /// Issues a capability directly for an immutable [`Payload`].
    ///
    /// # Safety
    /// Same requirements as [`Self::issue_unchecked`].
    pub unsafe fn issue_for_payload(
        payload: &Payload,
        decoded_len: u32,
        schema_fingerprint: u64,
        policy_fingerprint: u64,
    ) -> Self {
        let id = payload.backing_identity();
        Self {
            base_ptr: id.base_ptr,
            visible_ptr: id.visible_ptr,
            visible_len: id.visible_len,
            decoded_len,
            schema_fingerprint,
            policy_fingerprint,
        }
    }

    pub const fn schema_fingerprint(&self) -> u64 {
        self.schema_fingerprint
    }

    pub const fn policy_fingerprint(&self) -> u64 {
        self.policy_fingerprint
    }

    pub const fn visible_len(&self) -> u32 {
        self.visible_len
    }

    pub const fn decoded_len(&self) -> u32 {
        self.decoded_len
    }

    /// Verifies that `payload` has the exact same immutable backing pointer, visible
    /// range, decoded length, schema fingerprint, and policy fingerprint.
    pub fn matches_payload(
        &self,
        payload: &Payload,
        decoded_len: usize,
        schema_fingerprint: u64,
        policy_fingerprint: u64,
    ) -> bool {
        let id = payload.backing_identity();
        id.base_ptr == self.base_ptr
            && id.visible_ptr == self.visible_ptr
            && id.visible_len == self.visible_len
            && u32::try_from(decoded_len).ok() == Some(self.decoded_len)
            && self.schema_fingerprint == schema_fingerprint
            && self.policy_fingerprint == policy_fingerprint
    }

    /// Verifies that `slice` has the exact same pointer, length, schema, and policy.
    pub fn matches_slice(
        &self,
        slice: &[u8],
        decoded_len: usize,
        schema_fingerprint: u64,
        policy_fingerprint: u64,
    ) -> bool {
        let ptr = slice.as_ptr() as usize;
        ptr == self.visible_ptr
            && u32::try_from(slice.len()).ok() == Some(self.visible_len)
            && u32::try_from(decoded_len).ok() == Some(self.decoded_len)
            && self.schema_fingerprint == schema_fingerprint
            && self.policy_fingerprint == policy_fingerprint
    }
}

#[derive(Debug)]
pub struct ParsedMessage<T> {
    payload: Payload,
    decoded: Box<[u8]>,
    values: T,
    proof: ValidationProof,
    capability: Option<ValidationCapability>,
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
        let cap = unsafe {
            ValidationCapability::issue_for_payload(
                &payload,
                proof.decoded_len(),
                proof.schema_fingerprint(),
                0,
            )
        };
        Self { payload, decoded, values, proof, capability: Some(cap) }
    }

    /// Combines validated values with backing storage and an explicit capability.
    ///
    /// # Safety
    /// Same requirements as [`Self::new_proven`], plus `capability` must match
    /// `payload`, `decoded`, and `proof`.
    pub unsafe fn new_proven_with_capability(
        payload: Payload,
        decoded: Box<[u8]>,
        values: T,
        proof: ValidationProof,
        capability: ValidationCapability,
    ) -> Self {
        Self { payload, decoded, values, proof, capability: Some(capability) }
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

    pub fn capability(&self) -> Option<ValidationCapability> {
        self.capability
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
