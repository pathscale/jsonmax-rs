//! Bounded buffering for owned completions delivered in sequence order.
//!
//! Limits apply to buffered completions and their caller-declared retained
//! weights, including nested allocations. A directly returned completion is
//! outside those limits. Missing sequences are never synthesized or skipped.
//! Keep the queue on one coordinator, or synchronize access externally.

use super::*;

impl<T> OrderedCompletions<T> {
    /// Create an empty queue beginning at `first_sequence`.
    ///
    /// The pending-count limit must be nonzero. A zero byte limit permits only
    /// zero-weight buffered completions. Construction does not allocate nodes;
    /// the standard `BTreeMap` has no stable fallible reservation or insertion.
    pub fn new(options: OrderedOptions) -> Result<Self> {
        if options.max_pending == 0 {
            return Err(runtime_error(
                ErrorCode::ConstraintViolation,
                "ordered pending-count limit must be nonzero",
            ));
        }
        Ok(Self {
            options,
            next: Some(options.first_sequence),
            pending_bytes: 0,
            pending: BTreeMap::new(),
            cancelled: false,
        })
    }

    /// Deliver the next completion directly, or retain a future completion.
    ///
    /// Deliver a returned `Some` before calling `pop_ready`. Direct delivery
    /// bypasses storage limits so a full queue cannot block its missing head.
    /// Errors carried by completions are ordered exactly like successful values.
    ///
    /// Rejection leaves this queue unchanged and returns the original completion,
    /// including any result owners and admission reservation. Cancellation takes
    /// precedence over exhaustion; duplicate sequences take precedence over
    /// storage limits. After emitting `u64::MAX`, the sequence is exhausted.
    /// Buffered insertion uses the allocator's normal allocation-failure behavior.
    pub fn insert(
        &mut self,
        completion: Completion<T>,
    ) -> core::result::Result<Option<Completion<T>>, InsertError<T>> {
        if self.cancelled {
            return Err(rejected(
                completion,
                ErrorCode::Cancelled,
                "ordered completion queue cancelled",
            ));
        }
        let next = match self.next {
            Some(next) => next,
            None => {
                return Err(rejected(
                    completion,
                    ErrorCode::ConstraintViolation,
                    "ordered completion sequence exhausted",
                ));
            }
        };
        // A buffered sequence can become `next` after direct delivery of its
        // predecessor. Reject its duplicate before considering direct delivery.
        if completion.sequence < next || self.pending.contains_key(&completion.sequence) {
            return Err(rejected(
                completion,
                ErrorCode::DuplicateObjectKey,
                "ordered completion sequence already received or passed",
            ));
        }
        if completion.sequence == next {
            self.next = next.checked_add(1);
            return Ok(Some(completion));
        }
        if self.pending.len() >= self.options.max_pending {
            return Err(rejected(
                completion,
                ErrorCode::BudgetExhausted,
                "ordered pending-count limit exceeded",
            ));
        }
        let pending_bytes = match self.pending_bytes.checked_add(completion.retained_bytes) {
            Some(bytes) if bytes <= self.options.max_pending_bytes => bytes,
            _ => {
                return Err(rejected(
                    completion,
                    ErrorCode::BudgetExhausted,
                    "ordered pending-byte limit exceeded or sum overflowed",
                ));
            }
        };

        // All recoverable failures precede mutation. The duplicate check and
        // exclusive access guarantee this insertion cannot replace an owner.
        self.pending.insert(completion.sequence, completion);
        self.pending_bytes = pending_bytes;
        Ok(None)
    }

    /// Remove only the exact next sequence, transferring its owners to the caller.
    ///
    /// A gap, exhaustion, or cancellation returns `None` without advancing.
    /// Removing a completion releases queue accounting, not its admission lease:
    /// that lease remains held by the returned value until consumed or dropped.
    pub fn pop_ready(&mut self) -> Option<Completion<T>> {
        let next = self.next?;
        let completion = self.pending.remove(&next)?;
        self.pending_bytes -= completion.retained_bytes;
        self.next = next.checked_add(1);
        Some(completion)
    }

    /// The next required sequence, or `None` after exhaustion or cancellation.
    pub fn next_sequence(&self) -> Option<u64> {
        self.next
    }

    /// Number of completions currently buffered, excluding delivered values.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Sum of the caller-declared retained weights of buffered completions.
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Whether the entire queue was cancelled, independently of exhaustion.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Permanently cancel this queue and drop every buffered result and lease.
    ///
    /// This operation is idempotent. It does not cancel external CPU jobs or a
    /// separate `CancellationToken`. To cancel only one sequence, insert an error
    /// completion for that sequence instead, allowing later results to proceed.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.next = None;
        self.pending_bytes = 0;
        // Detach before dropping user values so even a destructor unwind leaves
        // the observable queue empty and consistently cancelled.
        drop(core::mem::take(&mut self.pending));
    }
}

fn rejected<T>(completion: Completion<T>, code: ErrorCode, detail: &'static str) -> InsertError<T> {
    InsertError { error: runtime_error(code, detail), completion }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{rc::Rc, vec};
    use core::cell::Cell;

    fn options(first: u64, count: usize, bytes: usize) -> OrderedOptions {
        OrderedOptions {
            first_sequence: first,
            max_pending: count,
            max_pending_bytes: bytes,
        }
    }

    fn success(sequence: u64, retained_bytes: usize) -> Completion<u64> {
        Completion { sequence, retained_bytes, result: Ok(sequence) }
    }

    fn diagnostic() -> Error {
        Error::new(ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 37, "expected integer")
            .with_schema(11, 19)
    }

    fn assert_runtime_error(error: &Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_state<T>(
        queue: &OrderedCompletions<T>,
        next: Option<u64>,
        count: usize,
        bytes: usize,
        cancelled: bool,
    ) {
        assert_eq!(queue.next_sequence(), next);
        assert_eq!(queue.pending_len(), count);
        assert_eq!(queue.pending_bytes(), bytes);
        assert_eq!(queue.is_cancelled(), cancelled);
        assert!(count <= queue.options.max_pending);
        assert!(bytes <= queue.options.max_pending_bytes);
        let mut total = 0usize;
        for (sequence, completion) in &queue.pending {
            assert_eq!(*sequence, completion.sequence);
            assert!(*sequence >= next.expect("pending entries require a next sequence"));
            total = total.checked_add(completion.retained_bytes).unwrap();
        }
        assert_eq!(total, bytes);
        if next.is_none() {
            assert_eq!(count, 0);
        }
    }

    fn assert_success(completion: Completion<u64>, sequence: u64, bytes: usize) {
        assert_eq!(completion.sequence, sequence);
        assert_eq!(completion.retained_bytes, bytes);
        assert_eq!(completion.result, Ok(sequence));
    }

    fn assert_rejected<T: core::fmt::Debug>(
        queue: &mut OrderedCompletions<T>,
        completion: Completion<T>,
        code: ErrorCode,
    ) -> Completion<T> {
        let before = (
            queue.next_sequence(), queue.pending_len(), queue.pending_bytes(), queue.is_cancelled(),
        );
        let sequence = completion.sequence;
        let retained_bytes = completion.retained_bytes;
        let rejected = queue.insert(completion).unwrap_err();
        assert_runtime_error(&rejected.error, code);
        assert_eq!(rejected.completion.sequence, sequence);
        assert_eq!(rejected.completion.retained_bytes, retained_bytes);
        assert_state(queue, before.0, before.1, before.2, before.3);
        rejected.completion
    }

    #[test]
    fn construction_vectors_and_empty_pop_preserve_initial_sequence() {
        for first in [0, 1, 43, u64::MAX - 1, u64::MAX] {
            for bytes in [0, 1, usize::MAX] {
                let error = OrderedCompletions::<u64>::new(options(first, 0, bytes)).unwrap_err();
                assert_runtime_error(&error, ErrorCode::ConstraintViolation);
                for count in [1, 7, usize::MAX] {
                    let expected = options(first, count, bytes);
                    let mut queue = OrderedCompletions::<u64>::new(expected).unwrap();
                    assert_eq!(queue.options, expected);
                    for _ in 0..3 {
                        assert!(queue.pop_ready().is_none());
                        assert_state(&queue, Some(first), 0, 0, false);
                    }
                }
            }
        }
        let queue = OrderedCompletions::<u64>::new(OrderedOptions::default()).unwrap();
        assert_state(&queue, Some(0), 0, 0, false);
        assert_eq!(queue.options.max_pending, DEFAULT_REORDER_LIMIT);
    }

    #[test]
    fn direct_delivery_bypasses_zero_byte_budget_and_never_charges_storage() {
        let mut queue = OrderedCompletions::new(options(40, 1, 0)).unwrap();
        for (sequence, bytes) in [(40, 0), (41, 1), (42, usize::MAX)] {
            let completion = queue.insert(success(sequence, bytes)).unwrap().unwrap();
            assert_success(completion, sequence, bytes);
            assert_state(&queue, Some(sequence + 1), 0, 0, false);
            assert!(queue.pop_ready().is_none());
        }
    }

    #[test]
    fn full_queue_accepts_missing_head_then_drains_only_contiguous_results() {
        let mut queue = OrderedCompletions::new(options(10, 2, 7)).unwrap();
        assert!(queue.insert(success(12, 4)).unwrap().is_none());
        assert!(queue.insert(success(11, 3)).unwrap().is_none());
        assert_state(&queue, Some(10), 2, 7, false);
        assert!(queue.pop_ready().is_none());
        let rejected = assert_rejected(&mut queue, success(13, 0), ErrorCode::BudgetExhausted);
        assert_success(rejected, 13, 0);
        assert_success(queue.insert(success(10, usize::MAX)).unwrap().unwrap(), 10, usize::MAX);
        assert_state(&queue, Some(11), 2, 7, false);
        assert_success(queue.pop_ready().unwrap(), 11, 3);
        assert_state(&queue, Some(12), 1, 4, false);
        assert_success(queue.pop_ready().unwrap(), 12, 4);
        assert_state(&queue, Some(13), 0, 0, false);
        assert!(queue.pop_ready().is_none());
        assert_success(queue.insert(success(13, 0)).unwrap().unwrap(), 13, 0);
    }

    #[test]
    fn gaps_never_skip_even_when_later_completions_are_buffered() {
        let mut queue = OrderedCompletions::new(options(4, 3, 9)).unwrap();
        for sequence in [8, 6] {
            assert!(queue.insert(success(sequence, 3)).unwrap().is_none());
        }
        assert_success(queue.insert(success(4, 1)).unwrap().unwrap(), 4, 1);
        for _ in 0..4 {
            assert!(queue.pop_ready().is_none());
            assert_state(&queue, Some(5), 2, 6, false);
        }
        assert_success(queue.insert(success(5, 1)).unwrap().unwrap(), 5, 1);
        assert_success(queue.pop_ready().unwrap(), 6, 3);
        assert!(queue.pop_ready().is_none());
        assert_state(&queue, Some(7), 1, 3, false);
        assert_success(queue.insert(success(7, 1)).unwrap().unwrap(), 7, 1);
        assert_success(queue.pop_ready().unwrap(), 8, 3);
        assert_state(&queue, Some(9), 0, 0, false);
    }

    #[test]
    fn old_and_pending_sequences_are_duplicates_before_capacity_checks() {
        let mut queue = OrderedCompletions::new(options(7, 1, 1)).unwrap();
        assert!(queue.insert(success(8, 1)).unwrap().is_none());
        for sequence in [0, 6, 8] {
            let returned = assert_rejected(
                &mut queue, success(sequence, usize::MAX), ErrorCode::DuplicateObjectKey,
            );
            assert_success(returned, sequence, usize::MAX);
        }
        assert_success(queue.insert(success(7, 2)).unwrap().unwrap(), 7, 2);
        // Sequence 8 is now the head, but its original owner is still buffered.
        assert_success(assert_rejected(
            &mut queue, success(8, 9), ErrorCode::DuplicateObjectKey,
        ), 8, 9);
        assert_success(queue.pop_ready().unwrap(), 8, 1);
        for sequence in [7, 8] {
            assert_success(assert_rejected(
                &mut queue, success(sequence, 0), ErrorCode::DuplicateObjectKey,
            ), sequence, 0);
        }
        assert_state(&queue, Some(9), 0, 0, false);
    }

    #[test]
    fn zero_weight_entries_still_consume_count_capacity() {
        let mut queue = OrderedCompletions::new(options(0, 2, 0)).unwrap();
        assert_success(assert_rejected(
            &mut queue, success(1, 1), ErrorCode::BudgetExhausted,
        ), 1, 1);
        assert!(queue.insert(success(1, 0)).unwrap().is_none());
        assert!(queue.insert(success(2, 0)).unwrap().is_none());
        assert_success(assert_rejected(
            &mut queue, success(3, 0), ErrorCode::BudgetExhausted,
        ), 3, 0);
        assert_state(&queue, Some(0), 2, 0, false);
        assert_success(queue.insert(success(0, usize::MAX)).unwrap().unwrap(), 0, usize::MAX);
        assert_success(queue.pop_ready().unwrap(), 1, 0);
        assert!(queue.insert(success(3, 0)).unwrap().is_none());
        assert_success(queue.pop_ready().unwrap(), 2, 0);
        assert_success(queue.pop_ready().unwrap(), 3, 0);
        assert_state(&queue, Some(4), 0, 0, false);
    }

    #[test]
    fn inclusive_byte_limit_and_rejected_owner_can_be_retried_after_drain() {
        let mut queue = OrderedCompletions::new(options(0, 5, 5)).unwrap();
        assert!(queue.insert(success(1, 2)).unwrap().is_none());
        assert!(queue.insert(success(2, 3)).unwrap().is_none());
        assert!(queue.insert(success(4, 0)).unwrap().is_none());
        let retry = assert_rejected(&mut queue, success(3, 2), ErrorCode::BudgetExhausted);
        assert_state(&queue, Some(0), 3, 5, false);
        assert_success(queue.insert(success(0, 0)).unwrap().unwrap(), 0, 0);
        assert_success(queue.pop_ready().unwrap(), 1, 2);
        assert!(queue.insert(retry).unwrap().is_none());
        assert_state(&queue, Some(2), 3, 5, false);
        assert_success(queue.pop_ready().unwrap(), 2, 3);
        assert_success(queue.pop_ready().unwrap(), 3, 2);
        assert_success(queue.pop_ready().unwrap(), 4, 0);
        assert_state(&queue, Some(5), 0, 0, false);
    }

    #[test]
    fn maximum_declared_weights_use_checked_sum_without_allocating_weight() {
        let mut queue = OrderedCompletions::new(options(0, usize::MAX, usize::MAX)).unwrap();
        assert!(queue.insert(success(1, usize::MAX - 1)).unwrap().is_none());
        assert_success(assert_rejected(
            &mut queue, success(2, 2), ErrorCode::BudgetExhausted,
        ), 2, 2);
        assert!(queue.insert(success(2, 1)).unwrap().is_none());
        assert!(queue.insert(success(3, 0)).unwrap().is_none());
        assert_state(&queue, Some(0), 3, usize::MAX, false);
        assert_success(assert_rejected(
            &mut queue, success(4, 1), ErrorCode::BudgetExhausted,
        ), 4, 1);
        assert_success(queue.insert(success(0, usize::MAX)).unwrap().unwrap(), 0, usize::MAX);
        assert_success(queue.pop_ready().unwrap(), 1, usize::MAX - 1);
        assert_state(&queue, Some(2), 2, 1, false);
        assert_success(queue.pop_ready().unwrap(), 2, 1);
        assert_success(queue.pop_ready().unwrap(), 3, 0);
        assert_state(&queue, Some(4), 0, 0, false);
    }

    #[test]
    fn direct_maximum_sequence_exhausts_without_wrapping_or_cancelling() {
        let mut queue = OrderedCompletions::new(options(u64::MAX, 1, 0)).unwrap();
        assert_success(queue.insert(success(u64::MAX, 1)).unwrap().unwrap(), u64::MAX, 1);
        assert_state(&queue, None, 0, 0, false);
        assert!(queue.pop_ready().is_none());
        for sequence in [0, u64::MAX - 1, u64::MAX] {
            assert_success(assert_rejected(
                &mut queue, success(sequence, usize::MAX), ErrorCode::ConstraintViolation,
            ), sequence, usize::MAX);
        }
        queue.cancel();
        assert_state(&queue, None, 0, 0, true);
        assert_success(assert_rejected(
            &mut queue, success(u64::MAX, 0), ErrorCode::Cancelled,
        ), u64::MAX, 0);
    }

    #[test]
    fn buffered_maximum_sequence_exhausts_after_exact_predecessor() {
        let mut queue = OrderedCompletions::new(options(u64::MAX - 1, 1, 1)).unwrap();
        assert!(queue.insert(success(u64::MAX, 1)).unwrap().is_none());
        assert!(queue.pop_ready().is_none());
        assert_success(queue.insert(success(u64::MAX - 1, 0)).unwrap().unwrap(), u64::MAX - 1, 0);
        assert_success(assert_rejected(
            &mut queue, success(u64::MAX, 0), ErrorCode::DuplicateObjectKey,
        ), u64::MAX, 0);
        assert_success(queue.pop_ready().unwrap(), u64::MAX, 1);
        assert_state(&queue, None, 0, 0, false);
        assert!(queue.pop_ready().is_none());
        assert_success(assert_rejected(
            &mut queue, success(0, 0), ErrorCode::ConstraintViolation,
        ), 0, 0);
    }

    #[test]
    fn parser_errors_and_individual_cancellation_preserve_order_and_metadata() {
        let mut queue = OrderedCompletions::new(options(20, 2, 5)).unwrap();
        let parser_error = diagnostic();
        assert!(queue.insert(Completion {
            sequence: 22, retained_bytes: 5, result: Err(parser_error.clone()),
        }).unwrap().is_none());
        let token = CancellationToken::new();
        token.cancel();
        assert!(queue.insert(Completion {
            sequence: 21, retained_bytes: 0, result: token.check().map(|()| 21u64),
        }).unwrap().is_none());
        assert_state(&queue, Some(20), 2, 5, false);
        assert_success(queue.insert(success(20, 0)).unwrap().unwrap(), 20, 0);
        let cancelled_job = queue.pop_ready().unwrap();
        assert_eq!(cancelled_job.sequence, 21);
        assert_runtime_error(&cancelled_job.result.unwrap_err(), ErrorCode::Cancelled);
        let failed_job = queue.pop_ready().unwrap();
        assert_eq!((failed_job.sequence, failed_job.retained_bytes), (22, 5));
        assert_eq!(failed_job.result.unwrap_err(), parser_error);
        assert_success(queue.insert(success(23, 0)).unwrap().unwrap(), 23, 0);
        assert_state(&queue, Some(24), 0, 0, false);
    }

    #[test]
    fn failed_completions_deliver_directly_and_survive_every_rejection_kind() {
        let mut queue = OrderedCompletions::<u64>::new(options(4, 1, 1)).unwrap();
        assert!(queue.insert(success(6, 1)).unwrap().is_none());
        for (sequence, code) in [
            (3, ErrorCode::DuplicateObjectKey),
            (6, ErrorCode::DuplicateObjectKey),
            (5, ErrorCode::BudgetExhausted),
        ] {
            let returned = assert_rejected(&mut queue, Completion {
                sequence, retained_bytes: 7, result: Err(diagnostic()),
            }, code);
            assert_eq!(returned.result, Err(diagnostic()));
        }
        let delivered = queue.insert(Completion {
            sequence: 4, retained_bytes: usize::MAX, result: Err(diagnostic()),
        }).unwrap().unwrap();
        assert_eq!((delivered.sequence, delivered.retained_bytes), (4, usize::MAX));
        assert_eq!(delivered.result, Err(diagnostic()));
        assert_state(&queue, Some(5), 1, 1, false);
        queue.cancel();
        let returned = assert_rejected(&mut queue, Completion {
            sequence: 5, retained_bytes: 7, result: Err(diagnostic()),
        }, ErrorCode::Cancelled);
        assert_eq!(returned.result, Err(diagnostic()));

        let mut exhausted = OrderedCompletions::<u64>::new(options(u64::MAX, 1, 0)).unwrap();
        let delivered = exhausted.insert(Completion {
            sequence: u64::MAX, retained_bytes: 0, result: Err(diagnostic()),
        }).unwrap().unwrap();
        assert_eq!(delivered.result, Err(diagnostic()));
        let returned = assert_rejected(&mut exhausted, Completion {
            sequence: 0, retained_bytes: 7, result: Err(diagnostic()),
        }, ErrorCode::ConstraintViolation);
        assert_eq!(returned.result, Err(diagnostic()));
    }

    #[derive(Debug)]
    struct Tracked {
        id: u64,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    fn tracked(sequence: u64, bytes: usize, drops: &Rc<Cell<usize>>) -> Completion<Tracked> {
        Completion {
            sequence,
            retained_bytes: bytes,
            result: Ok(Tracked { id: sequence, drops: Rc::clone(drops) }),
        }
    }

    #[test]
    fn non_clone_non_send_owners_are_returned_intact_on_rejection() {
        let drops = Rc::new(Cell::new(0));
        let mut queue = OrderedCompletions::new(options(5, 1, 1)).unwrap();
        assert!(queue.insert(tracked(7, 1, &drops)).unwrap().is_none());
        for (sequence, code) in [
            (4, ErrorCode::DuplicateObjectKey),
            (7, ErrorCode::DuplicateObjectKey),
            (6, ErrorCode::BudgetExhausted),
        ] {
            let before = drops.get();
            let returned = assert_rejected(&mut queue, tracked(sequence, 2, &drops), code);
            let value = returned.result.as_ref().unwrap();
            assert_eq!(value.id, sequence);
            assert!(Rc::ptr_eq(&value.drops, &drops));
            assert_eq!(drops.get(), before);
            drop(returned);
            assert_eq!(drops.get(), before + 1);
        }
        assert_eq!(drops.get(), 3);
        let head = queue.insert(tracked(5, 1, &drops)).unwrap().unwrap();
        assert_eq!(drops.get(), 3);
        drop(head);
        assert_eq!(drops.get(), 4);
        queue.cancel();
        assert_eq!(drops.get(), 5);
        let returned = assert_rejected(&mut queue, tracked(6, 1, &drops), ErrorCode::Cancelled);
        assert_eq!(drops.get(), 5);
        drop(returned);
        assert_eq!(drops.get(), 6);
        queue.cancel();
        drop(queue);
        assert_eq!(drops.get(), 6);
    }

    #[test]
    fn cancellation_drops_only_buffered_values_and_is_permanent_and_idempotent() {
        let drops = Rc::new(Cell::new(0));
        let token = CancellationToken::new();
        let mut queue = OrderedCompletions::new(options(0, 3, 6)).unwrap();
        assert!(queue.insert(tracked(1, 1, &drops)).unwrap().is_none());
        assert!(queue.insert(tracked(2, 2, &drops)).unwrap().is_none());
        assert!(queue.insert(tracked(3, 3, &drops)).unwrap().is_none());
        let direct = queue.insert(tracked(0, 8, &drops)).unwrap().unwrap();
        let popped = queue.pop_ready().unwrap();
        assert_eq!(drops.get(), 0);
        for _ in 0..3 {
            queue.cancel();
            assert_state(&queue, None, 0, 0, true);
            assert_eq!(drops.get(), 2);
            assert!(queue.pop_ready().is_none());
            assert!(!token.is_cancelled());
        }
        for sequence in [0, 1, 2, u64::MAX] {
            let before = drops.get();
            let returned = assert_rejected(
                &mut queue, tracked(sequence, usize::MAX, &drops), ErrorCode::Cancelled,
            );
            assert_eq!(drops.get(), before);
            drop(returned);
        }
        drop(queue);
        assert_eq!(drops.get(), 6);
        assert_eq!(direct.result.as_ref().unwrap().id, 0);
        assert_eq!(popped.result.as_ref().unwrap().id, 1);
        drop(direct);
        drop(popped);
        assert_eq!(drops.get(), 8);
    }

    #[test]
    fn cancelling_empty_queues_and_dropping_pending_queues_release_owners() {
        for first in [0, u64::MAX] {
            let mut queue = OrderedCompletions::<u64>::new(options(first, 1, 0)).unwrap();
            queue.cancel();
            queue.cancel();
            assert_state(&queue, None, 0, 0, true);
            assert!(queue.pop_ready().is_none());
            assert_success(assert_rejected(
                &mut queue, success(first, 0), ErrorCode::Cancelled,
            ), first, 0);
        }
        let drops = Rc::new(Cell::new(0));
        let mut queue = OrderedCompletions::new(options(0, 2, 2)).unwrap();
        assert!(queue.insert(tracked(1, 1, &drops)).unwrap().is_none());
        assert!(queue.insert(tracked(2, 1, &drops)).unwrap().is_none());
        assert_eq!(drops.get(), 0);
        drop(queue);
        assert_eq!(drops.get(), 2);
    }

    fn admitted(
        admission: &Admission,
        sequence: u64,
        bytes: usize,
    ) -> Completion<Admitted<u64>> {
        let lease = admission.try_acquire(ResourceRequest {
            jobs: 1, retained_bytes: bytes, scratch_bytes: 2,
        }).unwrap();
        Completion {
            sequence,
            retained_bytes: bytes,
            result: Ok(Admitted { value: sequence, lease }),
        }
    }

    #[test]
    fn admission_integration_holds_leases_through_rejection_delivery_and_cancel() {
        let admission = Admission::new(AdmissionLimits {
            max_jobs: 3, max_retained_bytes: 12, max_scratch_bytes: 6,
        }).unwrap();
        let mut queue = OrderedCompletions::new(options(0, 1, 4)).unwrap();
        assert!(queue.insert(admitted(&admission, 1, 4)).unwrap().is_none());
        let retry = assert_rejected(
            &mut queue, admitted(&admission, 2, 4), ErrorCode::BudgetExhausted,
        );
        assert_eq!(retry.result.as_ref().unwrap().value(), &2);
        assert_eq!(admission.snapshot(), AdmissionSnapshot {
            jobs: 2, retained_bytes: 8, scratch_bytes: 4,
        });
        let direct = queue.insert(admitted(&admission, 0, 4)).unwrap().unwrap();
        let popped = queue.pop_ready().unwrap();
        assert_state(&queue, Some(2), 0, 0, false);
        assert_eq!(admission.snapshot(), AdmissionSnapshot {
            jobs: 3, retained_bytes: 12, scratch_bytes: 6,
        });
        assert_runtime_error(&admission.try_acquire(ResourceRequest {
            jobs: 1, retained_bytes: 0, scratch_bytes: 0,
        }).unwrap_err(), ErrorCode::BudgetExhausted);
        let delivered_retry = queue.insert(retry).unwrap().unwrap();
        queue.cancel();
        assert_eq!(admission.snapshot().jobs, 3);
        drop(direct);
        drop(popped);
        assert_eq!(admission.snapshot(), AdmissionSnapshot {
            jobs: 1, retained_bytes: 4, scratch_bytes: 2,
        });
        let (_, reservation) = delivered_retry.result.unwrap().into_parts();
        assert_eq!(admission.snapshot().jobs, 1);
        drop(reservation);
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());

        let mut buffered = OrderedCompletions::new(options(0, 2, 8)).unwrap();
        assert!(buffered.insert(admitted(&admission, 1, 4)).unwrap().is_none());
        assert!(buffered.insert(admitted(&admission, 2, 4)).unwrap().is_none());
        buffered.cancel();
        buffered.cancel();
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
        let returned = assert_rejected(
            &mut buffered, admitted(&admission, 0, 4), ErrorCode::Cancelled,
        );
        assert_eq!(admission.snapshot().jobs, 1);
        drop(returned);
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());

        let mut dropped = OrderedCompletions::new(options(0, 1, 4)).unwrap();
        assert!(dropped.insert(admitted(&admission, 1, 4)).unwrap().is_none());
        drop(dropped);
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    }

    #[test]
    fn small_capacity_boundary_matrix_preserves_queue_on_every_rejection() {
        for count_limit in 1..=3 {
            for byte_limit in 0..=3 {
                for held_weight in 0..=byte_limit {
                    for candidate_weight in 0..=4 {
                        let mut queue = OrderedCompletions::new(options(0, count_limit, byte_limit)).unwrap();
                        assert!(queue.insert(success(2, held_weight)).unwrap().is_none());
                        if count_limit == 1 || held_weight + candidate_weight > byte_limit {
                            assert_success(assert_rejected(
                                &mut queue, success(1, candidate_weight), ErrorCode::BudgetExhausted,
                            ), 1, candidate_weight);
                            assert_state(&queue, Some(0), 1, held_weight, false);
                        } else {
                            assert!(queue.insert(success(1, candidate_weight)).unwrap().is_none());
                            assert_state(&queue, Some(0), 2, held_weight + candidate_weight, false);
                        }
                        assert_success(queue.insert(success(0, usize::MAX)).unwrap().unwrap(), 0, usize::MAX);
                        if queue.pending_len() == 2 {
                            assert_success(queue.pop_ready().unwrap(), 1, candidate_weight);
                        } else {
                            assert!(queue.pop_ready().is_none());
                            assert_success(queue.insert(success(1, candidate_weight)).unwrap().unwrap(), 1, candidate_weight);
                        }
                        assert_success(queue.pop_ready().unwrap(), 2, held_weight);
                        assert_state(&queue, Some(3), 0, 0, false);
                    }
                }
            }
        }
    }

    fn exercise_permutation(order: &[u64; 5], first: u64) {
        let mut queue = OrderedCompletions::new(options(first, 4, 10)).unwrap();
        let mut delivered = Vec::new();
        for &offset in order {
            let sequence = first + offset;
            let completion = Completion {
                sequence,
                retained_bytes: offset as usize,
                result: if offset == 2 { Err(diagnostic()) } else { Ok(sequence) },
            };
            if let Some(head) = queue.insert(completion).unwrap() {
                delivered.push(head);
            }
            while let Some(head) = queue.pop_ready() {
                delivered.push(head);
            }
            let expected_next = first.checked_add(delivered.len() as u64);
            let count = queue.pending.len();
            let weight = queue.pending.values().map(|item| item.retained_bytes).sum();
            assert_state(&queue, expected_next, count, weight, false);
            for (index, item) in delivered.iter().enumerate() {
                assert_eq!(item.sequence, first + index as u64);
            }
        }
        assert_eq!(delivered.len(), 5);
        for (offset, completion) in delivered.into_iter().enumerate() {
            assert_eq!(completion.sequence, first + offset as u64);
            assert_eq!(completion.retained_bytes, offset);
            if offset == 2 {
                assert_eq!(completion.result, Err(diagnostic()));
            } else {
                assert_eq!(completion.result, Ok(first + offset as u64));
            }
        }
        assert_state(&queue, first.checked_add(5), 0, 0, false);
    }

    fn permutations(order: &mut [u64; 5], position: usize, visited: &mut usize) {
        if position == order.len() {
            for first in [0, 41, u64::MAX - 4] {
                exercise_permutation(order, first);
            }
            *visited += 1;
            return;
        }
        for index in position..order.len() {
            order.swap(position, index);
            permutations(order, position + 1, visited);
            order.swap(position, index);
        }
    }

    #[test]
    fn all_five_element_permutations_preserve_values_errors_weights_and_order() {
        let mut visited = 0;
        permutations(&mut [0, 1, 2, 3, 4], 0, &mut visited);
        assert_eq!(visited, 120);
    }

    #[test]
    fn owned_payload_identity_and_queue_send_boundary_are_preserved() {
        fn assert_send<T: Send + 'static>() {}
        assert_send::<OrderedCompletions<Admitted<Payload>>>();
        assert_send::<Completion<Admitted<Payload>>>();
        assert_send::<InsertError<Admitted<Payload>>>();

        let bytes = vec![0u8, 1, 127, 128, 255].into_boxed_slice();
        let original = bytes.as_ptr();
        let mut queue = OrderedCompletions::new(options(0, 1, 5)).unwrap();
        assert!(queue.insert(Completion {
            sequence: 1, retained_bytes: 5, result: Ok(bytes),
        }).unwrap().is_none());
        let head = queue.insert(Completion {
            sequence: 0, retained_bytes: 0, result: Err(diagnostic()),
        }).unwrap().unwrap();
        assert_eq!(head.result.unwrap_err(), diagnostic());
        let delivered = queue.pop_ready().unwrap();
        let bytes = delivered.result.unwrap();
        assert_eq!(bytes.as_ptr(), original);
        assert_eq!(&*bytes, &[0, 1, 127, 128, 255]);
        assert_state(&queue, Some(2), 0, 0, false);
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn cancellation_establishes_empty_state_before_dropping_user_values() {
        extern crate std;

        #[derive(Debug)]
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("deliberate completion destructor unwind");
            }
        }

        let admission = Admission::new(AdmissionLimits {
            max_jobs: 1, max_retained_bytes: 1, max_scratch_bytes: 0,
        }).unwrap();
        let lease = admission.try_acquire(ResourceRequest {
            jobs: 1, retained_bytes: 1, scratch_bytes: 0,
        }).unwrap();
        let mut queue = OrderedCompletions::new(options(0, 1, 1)).unwrap();
        assert!(queue.insert(Completion {
            sequence: 1,
            retained_bytes: 1,
            result: Ok(Admitted { value: PanicOnDrop, lease }),
        }).unwrap().is_none());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| queue.cancel()));
        assert!(unwind.is_err());
        assert_state(&queue, None, 0, 0, true);
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
        queue.cancel();
        assert!(queue.pop_ready().is_none());
    }
}
