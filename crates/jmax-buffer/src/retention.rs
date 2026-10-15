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
        self.retained.slice(self.payload.as_slice())
            .expect("retained span must remain within its owner")
    }

    /// Transfers the whole owner without copying or selecting a further slice.
    ///
    /// Save `span()` before consuming an uncompacted result to select its bytes
    /// later. This owner carries no validation proof for any rebased reference.
    pub fn into_payload(self) -> Payload {
        self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{payload, BufferLimits, OwnerBytes, StrRef, Vec, MAX_BUFFER_BYTES};

    fn policy(max: u32, savings: u32, copy_unknown: bool) -> RetentionPolicy {
        RetentionPolicy {
            max_visible_bytes: max,
            min_savings_bytes: savings,
            copy_unknown_backing: copy_unknown,
        }
    }

    // Exercise canonical Bytes without adding or importing another dependency.
    fn sliced_payload(range: Range<usize>) -> Payload {
        match Payload::from_vec(b"prefix:body:suffix".to_vec()) {
            Payload::Bytes(bytes) => Payload::Bytes(bytes.slice(range)),
            Payload::Shared(_) => panic!("ABI vector constructor must produce Bytes"),
        }
    }

    fn assert_error(error: Error, code: ErrorCode, offset: u32, detail: &'static str) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert_eq!(error.detail, detail);
    }

    fn assert_bounds_error(error: Error, offset: u32) {
        assert_error(
            error, ErrorCode::InvalidToken, offset,
            "buffer span is out of bounds",
        );
    }

    fn assert_budget_error(error: Error, offset: u32) {
        assert_error(
            error, ErrorCode::BudgetExhausted, offset,
            "retained span exceeds visible byte budget",
        );
    }

    #[test]
    fn accounting_distinguishes_exact_and_unknown_backing_including_empty() {
        let decoded = DecodedArena::new(0);
        for payload in [
            Payload::from_slice(b"body"), Payload::from_slice(b""),
            Payload::from_vec(b"body".to_vec()), Payload::from_vec(Vec::new()),
            sliced_payload(7..11), sliced_payload(7..7),
        ] {
            let len = payload.len() as u64;
            let expected = match &payload {
                Payload::Shared(_) => BackingSize::Exact(len),
                Payload::Bytes(_) => BackingSize::AtLeast(len),
            };
            let stats = account(&payload, &decoded);
            assert_eq!(stats, RetentionStats {
                payload_visible_bytes: len,
                payload_backing_bytes: expected,
                decoded_len: 0,
                decoded_capacity: 0,
            });
            assert_eq!(stats.minimum_retained_bytes(), len);
            assert_eq!(stats.exact_retained_bytes(), expected.exact());
        }
    }

    #[test]
    fn accounting_uses_decoded_capacity_after_append_truncate_and_clear() {
        let payload = Payload::from_slice(b"body");
        let mut decoded = DecodedArena::with_capacity(32, 32).unwrap();
        let capacity = decoded.capacity() as u64;
        assert!(capacity >= 32);
        assert_eq!(account(&payload, &decoded).decoded_len, 0);
        decoded.append(b"abcdef").unwrap();
        for expected_len in [6, 3, 0] {
            if expected_len == 3 {
                decoded.truncate(3).unwrap();
            } else if expected_len == 0 {
                decoded.clear();
            }
            let stats = account(&payload, &decoded);
            assert_eq!(stats.decoded_len, expected_len);
            assert_eq!(stats.decoded_capacity, capacity);
            assert_eq!(stats.minimum_retained_bytes(), 4 + capacity);
            assert_eq!(stats.exact_retained_bytes(), Some(4 + capacity));
        }
        let unknown = account(&sliced_payload(7..11), &decoded);
        assert_eq!(unknown.minimum_retained_bytes(), 4 + capacity);
        assert_eq!(unknown.exact_retained_bytes(), None);
    }

    #[test]
    fn accounting_does_not_depend_on_other_shared_owners_or_clone_them() {
        let bytes: Arc<[u8]> = Arc::from(&b"shared"[..]);
        let payload = Payload::Shared(Arc::clone(&bytes));
        let decoded = DecodedArena::new(0);
        let count = Arc::strong_count(&bytes);
        let stats = account(&payload, &decoded);
        assert_eq!(Arc::strong_count(&bytes), count);
        let other = payload.clone();
        assert_eq!(account(&payload, &decoded), stats);
        drop(other);
        drop(bytes);
        assert_eq!(account(&payload, &decoded), stats);
        assert_eq!(stats.payload_backing_bytes, BackingSize::Exact(6));
    }

    #[test]
    fn statistic_totals_handle_unknown_backing_and_overflow() {
        for backing in [BackingSize::Exact(u64::MAX), BackingSize::AtLeast(u64::MAX)] {
            let mut stats = RetentionStats {
                payload_visible_bytes: 0,
                payload_backing_bytes: backing,
                decoded_len: 1,
                decoded_capacity: 0,
            };
            assert_eq!(stats.minimum_retained_bytes(), u64::MAX);
            assert_eq!(stats.exact_retained_bytes(), backing.exact());
            stats.decoded_capacity = 1;
            assert_eq!(stats.minimum_retained_bytes(), u64::MAX);
            assert_eq!(stats.exact_retained_bytes(), None);
        }
    }

    #[test]
    fn deterministic_policy_vectors_cover_each_copy_trigger_and_threshold() {
        // Selection length, budget, savings threshold, unknown policy, copy.
        let vectors = [
            (10, 10, 0, false, false),
            (10, 10, 0, true, false),
            (4, 10, 7, false, false),
            (4, 10, 6, false, true),
            (4, 10, 5, false, true),
            (9, 10, 0, false, true),
            (4, 4, u32::MAX, false, true),
            (4, 9, u32::MAX, false, true),
            (4, 10, u32::MAX, true, false),
            (0, 10, 10, false, true),
            (0, 10, 11, false, false),
            (0, 0, u32::MAX, false, true),
        ];
        for bytes_variant in [false, true] {
            let source = if bytes_variant {
                Payload::from_vec(b"0123456789".to_vec())
            } else {
                Payload::from_slice(b"0123456789")
            };
            for (len, max, savings, copy_unknown, shared_copy) in vectors {
                let span = Span32::new(10 - len, len);
                let retained = retain_span(&source, span, policy(max, savings, copy_unknown)).unwrap();
                let copied = shared_copy || (bytes_variant && copy_unknown);
                assert_eq!(retained.is_compacted(), copied);
                assert_eq!(retained.original_span(), span);
                assert_eq!(retained.span(), if copied { Span32::new(0, len) } else { span });
                assert_eq!(retained.as_slice(), span.slice(source.as_slice()).unwrap());
                assert_eq!(retained.payload().len(), if copied { len as usize } else { 10 });
                assert!(retained.payload().len() <= max as usize);
                if copied {
                    assert!(matches!(retained.payload(), Payload::Shared(_)));
                    assert_eq!(
                        account(retained.payload(), &DecodedArena::new(0)).payload_backing_bytes,
                        BackingSize::Exact(u64::from(len)),
                    );
                } else {
                    assert_eq!(retained.payload().as_slice().as_ptr(), source.as_slice().as_ptr());
                    assert_eq!(matches!(retained.payload(), Payload::Bytes(_)), bytes_variant);
                }
                assert_eq!(source.as_slice(), b"0123456789");
            }
        }
    }

    #[test]
    fn default_policy_threshold_has_an_inclusive_boundary() {
        let mut bytes = alloc::vec![b'_'; 4097];
        bytes[0] = b'x';
        let payload = Payload::from_slice(&bytes);
        let default = RetentionPolicy::default();
        assert_eq!(default.min_savings_bytes, 4096);
        assert!(default.copy_unknown_backing);
        assert_eq!(default.max_visible_bytes, crate::DEFAULT_MAX_PAYLOAD_BYTES);
        let below = retain_span(&payload, Span32::new(0, 2), default).unwrap();
        assert!(!below.is_compacted());
        let equal = retain_span(&payload, Span32::new(0, 1), default).unwrap();
        assert!(equal.is_compacted());
        assert_eq!(equal.as_slice(), b"x");
        assert_eq!(equal.payload().len(), 1);
    }

    #[test]
    fn no_copy_preserves_whole_owner_span_variant_and_shared_identity() {
        let bytes: Arc<[u8]> = Arc::from(&b"prefix:body:suffix"[..]);
        let shared = Payload::Shared(Arc::clone(&bytes));
        let initial_count = Arc::strong_count(&bytes);
        let span = Span32::new(7, 4);
        let retained = retain_span(&shared, span, policy(17, 14, false)).unwrap();
        assert_eq!(Arc::strong_count(&bytes), initial_count + 1);
        match retained.payload() {
            Payload::Shared(owner) => assert!(Arc::ptr_eq(owner, &bytes)),
            Payload::Bytes(_) => panic!("uncompacted shared owner changed variant"),
        }
        assert!(!retained.is_compacted());
        assert_eq!(retained.original_span(), span);
        assert_eq!(retained.span(), span);
        assert_eq!(retained.as_slice(), b"body");
        assert_eq!(retained.as_slice().as_ptr(), bytes[7..11].as_ptr());
        let saved = retained.span();
        let owner = retained.into_payload();
        assert_eq!(owner.as_slice(), b"prefix:body:suffix");
        assert_eq!(saved.slice(owner.as_slice()), Some(&b"body"[..]));
        assert_eq!(Arc::strong_count(&bytes), initial_count + 1);
        drop(owner);
        assert_eq!(Arc::strong_count(&bytes), initial_count);
    }

    #[test]
    fn sliced_bytes_use_visible_coordinates_and_can_preserve_unknown_backing() {
        let source = sliced_payload(7..11);
        let retained = retain_span(&source, Span32::new(1, 2), policy(4, 3, false)).unwrap();
        assert!(!retained.is_compacted());
        assert!(matches!(retained.payload(), Payload::Bytes(_)));
        assert_eq!(retained.payload().as_slice(), b"body");
        assert_eq!(retained.payload().as_slice().as_ptr(), source.as_slice().as_ptr());
        assert_eq!(retained.as_slice(), b"od");
        assert_eq!(retained.original_span(), Span32::new(1, 2));
        assert_eq!(retained.span(), Span32::new(1, 2));
        let stats = account(retained.payload(), &DecodedArena::new(0));
        assert_eq!(stats.payload_backing_bytes, BackingSize::AtLeast(4));
        assert_eq!(stats.exact_retained_bytes(), None);
        let span = retained.span();
        let owner = retained.into_payload();
        drop(source);
        assert_eq!(owner.as_slice(), b"body");
        assert_eq!(span.slice(owner.as_slice()), Some(&b"od"[..]));
    }

    #[test]
    fn unknown_backing_copy_is_exact_even_without_visible_savings() {
        for span in [Span32::new(0, 4), Span32::new(1, 2), Span32::new(4, 0)] {
            let source = sliced_payload(7..11);
            let retained = retain_span(&source, span, policy(4, u32::MAX, true)).unwrap();
            assert!(retained.is_compacted());
            assert_eq!(retained.original_span(), span);
            assert_eq!(retained.span(), Span32::new(0, span.len));
            assert_eq!(retained.as_slice(), span.slice(source.as_slice()).unwrap());
            assert!(matches!(retained.payload(), Payload::Shared(_)));
            if span.len != 0 {
                assert_ne!(retained.as_slice().as_ptr(), span.slice(source.as_slice()).unwrap().as_ptr());
            }
            let stats = account(retained.payload(), &DecodedArena::new(0));
            assert_eq!(stats.payload_backing_bytes, BackingSize::Exact(u64::from(span.len)));
            assert_eq!(stats.exact_retained_bytes(), Some(u64::from(span.len)));
            let expected = retained.as_slice().to_vec();
            drop(source);
            assert_eq!(retained.into_payload().as_slice(), expected.as_slice());
        }
    }

    #[test]
    fn empty_payloads_respect_positive_savings_rule_at_zero_threshold() {
        for unknown in [false, true] {
            for source in [Payload::from_slice(b""), sliced_payload(7..7)] {
                let bytes_variant = matches!(&source, Payload::Bytes(_));
                let retained = retain_span(&source, Span32::new(0, 0), policy(0, 0, unknown)).unwrap();
                assert_eq!(retained.is_compacted(), bytes_variant && unknown);
                assert_eq!(retained.original_span(), Span32::new(0, 0));
                assert_eq!(retained.span(), Span32::new(0, 0));
                assert!(retained.as_slice().is_empty());
                assert!(retained.into_payload().is_empty());
            }
        }
    }

    #[test]
    fn empty_selections_detach_when_the_visible_owner_exceeds_budget() {
        let bytes: Arc<[u8]> = Arc::from(&b"large backing"[..]);
        let source = Payload::Shared(Arc::clone(&bytes));
        let count = Arc::strong_count(&bytes);
        for offset in [0, 5, 13] {
            let retained = retain_span(&source, Span32::new(offset, 0), policy(0, u32::MAX, false)).unwrap();
            assert!(retained.is_compacted());
            assert_eq!(Arc::strong_count(&bytes), count);
            assert_eq!(retained.original_span(), Span32::new(offset, 0));
            assert_eq!(retained.span(), Span32::new(0, 0));
            assert!(retained.payload().is_empty());
            match retained.payload() {
                Payload::Shared(owner) => assert!(!Arc::ptr_eq(owner, &bytes)),
                Payload::Bytes(_) => panic!("empty compaction must use shared storage"),
            }
        }
    }

    #[test]
    fn invalid_spans_precede_budget_errors_and_never_mutate_input() {
        let source = Payload::from_slice(b"abcd");
        let pointer = source.as_slice().as_ptr();
        for span in [
            Span32::new(0, 5), Span32::new(3, 2), Span32::new(5, 0),
            Span32::new(u32::MAX, 0), Span32::new(u32::MAX, 1),
            Span32::new(u32::MAX - 1, 2), Span32::new(1, u32::MAX),
        ] {
            for configured in [policy(0, 0, true), policy(u32::MAX, u32::MAX, false)] {
                assert_bounds_error(retain_span(&source, span, configured).unwrap_err(), span.offset);
            }
        }
        for (span, max) in [(Span32::new(0, 4), 3), (Span32::new(1, 2), 1), (Span32::new(3, 1), 0)] {
            assert_budget_error(retain_span(&source, span, policy(max, 0, true)).unwrap_err(), span.offset);
        }
        assert_eq!(source.as_slice(), b"abcd");
        assert_eq!(source.as_slice().as_ptr(), pointer);
    }

    #[test]
    fn abi_boundary_vectors_validate_without_large_allocations() {
        let unlimited = policy(u32::MAX, u32::MAX, false);
        for (len, span) in [
            (0, Span32::new(0, 0)), (1, Span32::new(1, 0)),
            (MAX_BUFFER_BYTES, Span32::new(0, u32::MAX)),
            (MAX_BUFFER_BYTES, Span32::new(u32::MAX, 0)),
            (MAX_BUFFER_BYTES, Span32::new(u32::MAX - 1, 1)),
        ] {
            let (visible, range) = checked_retention_range(len, span, unlimited).unwrap();
            assert_eq!(visible, u32::try_from(len).unwrap());
            assert_eq!(range.start, usize::try_from(span.offset).unwrap());
            assert_eq!(range.len(), usize::try_from(span.len).unwrap());
        }
        for span in [Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX)] {
            assert_bounds_error(checked_retention_range(MAX_BUFFER_BYTES, span, policy(0, 0, true)).unwrap_err(), span.offset);
        }
        assert_budget_error(
            checked_retention_range(MAX_BUFFER_BYTES, Span32::new(0, u32::MAX), policy(u32::MAX - 1, 0, false)).unwrap_err(),
            0,
        );
        if let Some(over_abi) = MAX_BUFFER_BYTES.checked_add(1) {
            for len in [over_abi, usize::MAX] {
                for span in [Span32::new(0, 0), Span32::new(u32::MAX, 1)] {
                    assert_error(
                        checked_retention_range(len, span, policy(0, 0, true)).unwrap_err(),
                        ErrorCode::PayloadTooLarge, u32::MAX,
                        "buffer span exceeds ABI range",
                    );
                }
            }
        }
    }

    #[test]
    fn every_small_selection_matches_an_independent_policy_oracle() {
        let bytes = b"\0\xffa\x80z";
        for source in [Payload::from_slice(bytes), Payload::from_vec(bytes.to_vec())] {
            for start in 0u32..=5 {
                for end in start..=5 {
                    let span = Span32::new(start, end - start);
                    let expected = &bytes[start as usize..end as usize];
                    for max in 0u32..=6 {
                        for threshold in [0, 1, 4, 5, 6, u32::MAX] {
                            for unknown in [false, true] {
                                let configured = policy(max, threshold, unknown);
                                let result = retain_span(&source, span, configured);
                                if span.len > max {
                                    assert_budget_error(result.unwrap_err(), start);
                                    continue;
                                }
                                let copied = if max < 5 {
                                    true
                                } else if unknown && matches!(&source, Payload::Bytes(_)) {
                                    true
                                } else {
                                    span.len < 5 && threshold <= 5 - span.len
                                };
                                let retained = result.unwrap();
                                assert_eq!(retained.as_slice(), expected);
                                assert_eq!(retained.original_span(), span);
                                assert_eq!(retained.is_compacted(), copied);
                                assert_eq!(retained.span(), if copied { Span32::new(0, span.len) } else { span });
                                let owner = OwnerBytes::new(retained.payload().as_slice(), &[]).unwrap();
                                assert_eq!(owner.view(SpanSource::RawPayload, retained.span()).unwrap().as_bytes(), expected);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn admission_arena_views_and_retention_integrate_with_rebased_strings() {
        let text = "a\u{e9}\u{20ac}\u{1f600}";
        let mut raw = b"prefix:".to_vec();
        raw.extend_from_slice(text.as_bytes());
        raw.extend_from_slice(b":suffix");
        let limits = BufferLimits { max_payload_bytes: 24, max_decoded_bytes: 32 };
        let payload = payload::from_vec(raw, limits).unwrap();
        let mut arena = DecodedArena::with_capacity(32, limits.max_decoded_bytes).unwrap();
        arena.append(b"\xff").unwrap();
        let decoded_ref = arena.append_str(text).unwrap();
        let original = StrRef { span: Span32::new(7, 10), source: SpanSource::RawPayload, scalar_len: 4 };
        let owner = OwnerBytes::new(payload.as_slice(), arena.as_slice()).unwrap();
        assert_eq!(owner.string(original).unwrap().as_str(), text);
        assert_eq!(owner.string(decoded_ref).unwrap().as_str(), text);
        let before = account(&payload, &arena);
        let retained = retain_span(&payload, original.span, policy(10, u32::MAX, false)).unwrap();
        assert!(retained.is_compacted());
        assert_eq!(retained.original_span(), original.span);
        assert_eq!(retained.span(), Span32::new(0, 10));
        let rebased = StrRef { span: retained.span(), ..original };
        let independent = OwnerBytes::new(retained.payload().as_slice(), &[]).unwrap();
        assert_eq!(independent.string(rebased).unwrap().as_str(), text);
        assert_bounds_error(independent.string(original).unwrap_err(), 7);
        assert_eq!(account(&payload, &arena), before);
        assert_eq!(owner.string(original).unwrap().as_str(), text);
        assert_eq!(owner.string(decoded_ref).unwrap().as_str(), text);
        let after = account(retained.payload(), &arena);
        assert_eq!(after.payload_backing_bytes, BackingSize::Exact(10));
        assert_eq!(after.decoded_capacity, before.decoded_capacity);
        assert_eq!(after.exact_retained_bytes(), Some(10 + before.decoded_capacity));
        drop(payload);
        drop(arena);
        let cloned = retained.clone();
        drop(retained);
        assert_eq!(cloned.into_payload().as_slice(), text.as_bytes());
    }

    #[test]
    fn retention_preserves_binary_and_partial_utf8_without_interpreting_them() {
        let source = Payload::from_slice(b"!\xc3\xa9\0\xff\\uD800?");
        for (span, expected) in [
            (Span32::new(1, 1), &b"\xc3"[..]),
            (Span32::new(2, 3), &b"\xa9\0\xff"[..]),
            (Span32::new(5, 6), &b"\\uD800"[..]),
        ] {
            let retained = retain_span(&source, span, policy(span.len, 0, true)).unwrap();
            assert_eq!(retained.as_slice(), expected);
            assert!(retained.is_compacted());
            let owner = OwnerBytes::new(retained.payload().as_slice(), &[]).unwrap();
            assert_eq!(owner.view(SpanSource::RawPayload, retained.span()).unwrap().as_bytes(), expected);
        }
    }

    #[test]
    fn repeated_retention_uses_each_immediate_owners_coordinates() {
        let source = Payload::from_slice(b"prefix:body:suffix");
        let first = retain_span(&source, Span32::new(7, 4), policy(4, u32::MAX, false)).unwrap();
        let second = retain_span(first.payload(), Span32::new(1, 2), policy(2, u32::MAX, false)).unwrap();
        assert_eq!(first.original_span(), Span32::new(7, 4));
        assert_eq!(first.span(), Span32::new(0, 4));
        assert_eq!(second.original_span(), Span32::new(1, 2));
        assert_eq!(second.span(), Span32::new(0, 2));
        assert_eq!(second.as_slice(), b"od");
        let third = retain_span(second.payload(), second.span(), policy(2, 0, true)).unwrap();
        assert!(!third.is_compacted());
        assert_eq!(third.as_slice().as_ptr(), second.as_slice().as_ptr());
        drop(source);
        drop(first);
        drop(second);
        assert_eq!(third.into_payload().as_slice(), b"od");
    }
}
