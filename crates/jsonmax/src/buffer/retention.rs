//! Retained backing accounting and independent ownership of selected raw bytes.
//!
//! A Bytes value exposes its visible length, but not its backing allocation.
//! Accounting therefore reports a lower bound for that variant. Compaction
//! copies only the selected bytes into a shared slice with a known exact length.
//! Neither operation validates contents or carries a message validation proof.

use super::{
    checked_range, checked_span, Arc, BackingSize, DecodedArena, Error, ErrorCode,
    ErrorSource, Payload, Range, Result, RetainedRange, RetentionPolicy,
    RetentionStats, Span32, SpanSource,
};

/// Reports payload backing and decoded allocation attributable to these owners.
///
/// Shared slices have an exact backing length. Bytes values, including empty
/// and sliced values, have only a visible-length lower bound. Decoded capacity
/// includes spare allocation even after truncation or clearing. Counts exclude
/// reference-count control blocks, allocator overhead, and other owners; they
/// do not describe exclusive ownership or process resident memory.
pub fn account(payload: &Payload, decoded: &DecodedArena) -> RetentionStats {
    let payload_backing_bytes = match payload {
        Payload::Shared(bytes) => BackingSize::Exact(bytes.len() as u64),
        Payload::SharedView(view) => BackingSize::Exact(view.backing_len() as u64),
        Payload::Bytes(bytes) => BackingSize::AtLeast(bytes.len() as u64),
    };
    RetentionStats {
        payload_visible_bytes: payload.len() as u64,
        payload_backing_bytes,
        decoded_len: decoded.len() as u64,
        decoded_capacity: decoded.capacity() as u64,
    }
}

/// Retains a checked span, copying when the policy requires compaction.
///
/// Payload ABI length and span bounds are checked before the selected length's
/// budget. A fitting selection is copied if the entire visible owner exceeds
/// the budget, if positive visible savings reach the configured threshold, or
/// if the backing is unknown and the policy requests copying it. Otherwise the
/// whole original owner is cloned, preserving its variant and visible range.
///
/// Copies use `Arc<[u8]>`, rebase the retained span to zero, and detach empty
/// selections as well. The input coordinates remain available separately. The
/// budget limits visible retained bytes, not unknown underlying Bytes backing.
/// Allocation and cloning follow the ABI constructors' normal allocator
/// behavior; allocation failure is not converted into an ABI error here.
pub fn retain_span(
    payload: &Payload,
    span: Span32,
    policy: RetentionPolicy,
) -> Result<RetainedRange> {
    let (visible_len, range) = checked_retention_range(payload.len(), span, policy)?;
    // A checked range guarantees span.len <= visible_len, so this cannot wrap.
    let visible_savings = visible_len - span.len;
    let compacted = visible_len > policy.max_visible_bytes
        || (visible_savings > 0 && visible_savings >= policy.min_savings_bytes)
        || (policy.copy_unknown_backing && matches!(payload, Payload::Bytes(_)));

    let (owner, retained) = if compacted {
        (
            Payload::Shared(Arc::from(&payload.as_slice()[range])),
            Span32::new(0, span.len),
        )
    } else {
        (payload.clone(), span)
    };
    Ok(RetainedRange { payload: owner, original: span, retained, compacted })
}

// Length-only validation permits ABI boundary and error-precedence vectors
// without allocating multi-gigabyte buffers or constructing invalid slices.
fn checked_retention_range(
    payload_len: usize,
    span: Span32,
    policy: RetentionPolicy,
) -> Result<(u32, Range<usize>)> {
    let visible_len = checked_span(0, payload_len, SpanSource::RawPayload)?.len;
    let range = checked_range(span, payload_len)?;
    if span.len > policy.max_visible_bytes {
        return Err(Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Decode,
            span.offset,
            "retained span exceeds visible byte budget",
        ));
    }
    Ok((visible_len, range))
}

impl RetainedRange {
    /// Returns the entire retained owner, which may include unselected bytes.
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// Returns the selected coordinates relative to the original visible owner.
    pub fn original_span(&self) -> Span32 {
        self.original
    }

    /// Returns the selected coordinates relative to `self.payload()`.
    pub fn span(&self) -> Span32 {
        self.retained
    }

    /// Reports whether this operation copied and rebased the selected bytes.
    pub fn is_compacted(&self) -> bool {
        self.compacted
    }

    /// Borrows exactly the selected bytes from this independent owner.
    pub fn as_slice(&self) -> &[u8] {
        // Construction establishes bounds and the private immutable owner
        // preserves them. Resolve with checked ABI arithmetic and indexing.
        self.retained.slice(self.payload.as_slice()).unwrap_or(&[])
    }

    /// Transfers the whole owner without copying or selecting a further slice.
    ///
    /// Save `span()` before consuming an uncompacted result to select its bytes
    /// later. This owner carries no validation proof for any rebased reference.
    pub fn into_payload(self) -> Payload {
        self.payload
    }
}
