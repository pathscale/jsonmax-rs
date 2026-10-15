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
