//! Shared, nonblocking admission for jobs and declared memory weights.
//!
//! Each resource has an independent atomic counter. Acquisition reserves jobs,
//! retained bytes, then scratch bytes, rolling back on failure. A snapshot can
//! observe an acquisition or rollback in progress; it is not a transactionally
//! consistent view and must not be used to authorize work.
//!
//! Memory reservations describe logical capacities, not allocator bookkeeping
//! or a process-wide allocation limit. Callers and validators remain responsible
//! for bounded, fallible growth within their reservations.

use super::*;

impl Admission {
    /// Create an independent admission pool with immutable, inclusive limits.
    ///
    /// `max_jobs` must be nonzero. Zero retained-byte or scratch-byte limits
    /// allow only zero-weight requests for the corresponding resource.
    /// The shared state uses the allocator's normal allocation-failure behavior.
    pub fn new(limits: AdmissionLimits) -> Result<Self> {
        if limits.max_jobs == 0 {
            return Err(runtime_error(
                ErrorCode::ConstraintViolation,
                "admission job limit must be nonzero",
            ));
        }
        Ok(Self {
            state: Arc::new(AdmissionState {
                limits,
                jobs: AtomicUsize::new(0),
                retained_bytes: AtomicUsize::new(0),
                scratch_bytes: AtomicUsize::new(0),
            }),
        })
    }

    /// Return the immutable limits shared by every clone of this pool.
    pub fn limits(&self) -> AdmissionLimits {
        self.state.limits
    }

    /// Read advisory, per-counter usage with acquire ordering.
    ///
    /// Concurrent reservations and releases can occur between these loads.
    /// Temporary reservations that will subsequently roll back may be visible.
    pub fn snapshot(&self) -> AdmissionSnapshot {
        AdmissionSnapshot {
            jobs: self.state.jobs.load(Ordering::Acquire),
            retained_bytes: self.state.retained_bytes.load(Ordering::Acquire),
            scratch_bytes: self.state.scratch_bytes.load(Ordering::Acquire),
        }
    }

    /// Try to reserve every requested resource without waiting for capacity.
    ///
    /// A zero job count is a constraint violation. Insufficient capacity or
    /// arithmetic overflow returns `BudgetExhausted` after releasing any partial
    /// reservation. Compare/exchange retries resolve contention only; observing
    /// insufficient current capacity immediately fails the acquisition.
    ///
    /// The returned non-cloneable lease releases its reservation on drop. Keep
    /// it alive for as long as the corresponding work or retained owners live.
    pub fn try_acquire(&self, request: ResourceRequest) -> Result<AdmissionLease> {
        if request.jobs == 0 {
            return Err(runtime_error(
                ErrorCode::ConstraintViolation,
                "admission request must reserve at least one job",
            ));
        }

        let state = Arc::clone(&self.state);
        reserve(
            &state.jobs,
            request.jobs,
            state.limits.max_jobs,
            "admission job capacity exceeded",
        )?;

        // This private, partially populated guard owns exactly the counters
        // already reserved. The frozen root's Drop rolls them back in reverse
        // order on either subsequent error. Only a complete lease is returned.
        let mut lease = AdmissionLease {
            state,
            request: ResourceRequest {
                jobs: request.jobs,
                retained_bytes: 0,
                scratch_bytes: 0,
            },
        };
        reserve(
            &lease.state.retained_bytes,
            request.retained_bytes,
            lease.state.limits.max_retained_bytes,
            "admission retained-byte capacity exceeded",
        )?;
        lease.request.retained_bytes = request.retained_bytes;

        reserve(
            &lease.state.scratch_bytes,
            request.scratch_bytes,
            lease.state.limits.max_scratch_bytes,
            "admission scratch-byte capacity exceeded",
        )?;
        lease.request.scratch_bytes = request.scratch_bytes;
        Ok(lease)
    }

    /// Reserve once and attach fresh, empty scratch with zero backing capacity.
    ///
    /// No scratch storage is preallocated. Trusted users of `scratch_mut` and
    /// validators must grow storage fallibly within `budget_bytes()`, counting
    /// decoded-buffer capacity in bytes, stack capacity times `size_of` its
    /// frame type, and seen-word capacity times `size_of::<u64>()`. Clearing
    /// scratch resets lengths but neither frees capacity nor releases credits.
    /// Per-job payload and decoded limits still apply independently.
    pub fn try_scratch(&self, request: ResourceRequest) -> Result<ScratchLease> {
        let lease = self.try_acquire(request)?;
        Ok(ScratchLease { scratch: Scratch::default(), lease })
    }
}

// Every successful CAS adds only to the value actually observed by that CAS.
// Releases can race with reservations safely: a failed CAS refreshes `current`,
// and both the overflow and capacity checks are repeated before trying again.
fn reserve(
    counter: &AtomicUsize,
    amount: usize,
    limit: usize,
    detail: &'static str,
) -> Result<()> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(amount)
            .filter(|&next| next <= limit)
            .ok_or_else(|| runtime_error(ErrorCode::BudgetExhausted, detail))?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;
    use jmax_abi::syntax::FramePhase;

    fn limits(jobs: usize, bytes: usize, scratch: usize) -> AdmissionLimits {
        AdmissionLimits {
            max_jobs: jobs,
            max_retained_bytes: bytes,
            max_scratch_bytes: scratch,
        }
    }

    fn request(jobs: usize, bytes: usize, scratch: usize) -> ResourceRequest {
        ResourceRequest { jobs, retained_bytes: bytes, scratch_bytes: scratch }
    }

    fn assert_usage(admission: &Admission, expected: ResourceRequest) {
        assert_eq!(admission.snapshot(), AdmissionSnapshot {
            jobs: expected.jobs,
            retained_bytes: expected.retained_bytes,
            scratch_bytes: expected.scratch_bytes,
        });
    }

    fn assert_error<T>(result: Result<T>, expected: ErrorCode) {
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("expected admission failure"),
        };
        assert_eq!(error.code, expected);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_empty_scratch(scratch: &Scratch) {
        assert!(scratch.decoded_buf.is_empty());
        assert!(scratch.stack.is_empty());
        assert!(scratch.seen_words.is_empty());
    }

    #[test]
    fn constructor_rejects_zero_jobs_and_preserves_valid_limits() {
        for bytes in [0, 1, usize::MAX] {
            for scratch in [0, 1, usize::MAX] {
                assert_error(Admission::new(limits(0, bytes, scratch)),
                    ErrorCode::ConstraintViolation);
                for jobs in [1, 7, usize::MAX] {
                    let expected = limits(jobs, bytes, scratch);
                    let admission = Admission::new(expected).unwrap();
                    assert_eq!(admission.limits(), expected);
                    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
                    assert_eq!(admission.clone().limits(), expected);
                }
            }
        }
    }

    #[test]
    fn zero_job_requests_fail_before_any_capacity_reservation() {
        let admission = Admission::new(limits(1, 2, 3)).unwrap();
        let held = admission.try_acquire(request(1, 2, 3)).unwrap();
        for bytes in [0, 1, usize::MAX] {
            for scratch in [0, 1, usize::MAX] {
                let invalid = request(0, bytes, scratch);
                assert_error(admission.try_acquire(invalid), ErrorCode::ConstraintViolation);
                assert_error(admission.try_scratch(invalid), ErrorCode::ConstraintViolation);
                assert_usage(&admission, held.request());
            }
        }
        drop(held);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn zero_byte_capacities_admit_only_zero_weights() {
        let admission = Admission::new(limits(3, 0, 0)).unwrap();
        for excessive in [request(1, 1, 0), request(1, 0, 1), request(1, 1, 1)] {
            assert_error(admission.try_acquire(excessive), ErrorCode::BudgetExhausted);
            assert_usage(&admission, ResourceRequest::default());
        }
        let first = admission.try_acquire(request(2, 0, 0)).unwrap();
        let second = admission.try_scratch(request(1, 0, 0)).unwrap();
        assert_eq!(second.budget_bytes(), 0);
        assert_empty_scratch(second.scratch());
        assert_usage(&admission, request(3, 0, 0));
        assert_error(admission.try_acquire(request(1, 0, 0)), ErrorCode::BudgetExhausted);
        drop(first);
        assert_usage(&admission, request(1, 0, 0));
        drop(second);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn exact_limits_release_and_reacquire_without_leaking_credits() {
        let admission = Admission::new(limits(3, 11, 17)).unwrap();
        for _ in 0..16 {
            let first = admission.try_acquire(request(1, 4, 6)).unwrap();
            let second = admission.try_acquire(request(2, 7, 11)).unwrap();
            assert_eq!(first.request(), request(1, 4, 6));
            assert_eq!(second.request(), request(2, 7, 11));
            assert_usage(&admission, request(3, 11, 17));
            assert_error(admission.try_acquire(request(1, 0, 0)), ErrorCode::BudgetExhausted);
            drop(first);
            assert_usage(&admission, second.request());
            let replacement = admission.try_acquire(request(1, 4, 6)).unwrap();
            drop(second);
            assert_usage(&admission, replacement.request());
            drop(replacement);
            assert_usage(&admission, ResourceRequest::default());
        }
    }

    #[test]
    fn each_failure_stage_rolls_back_without_touching_existing_leases() {
        let admission = Admission::new(limits(7, 13, 19)).unwrap();
        let held = admission.try_acquire(request(2, 3, 5)).unwrap();
        let failures = [
            request(6, 0, 0),
            request(1, 11, 0),
            request(1, 0, 15),
            request(5, 10, 15),
            request(usize::MAX, 0, 0),
            request(1, usize::MAX, 0),
            request(1, 10, usize::MAX),
        ];
        for failed in failures {
            for _ in 0..3 {
                assert_error(admission.try_acquire(failed), ErrorCode::BudgetExhausted);
                assert_error(admission.try_scratch(failed), ErrorCode::BudgetExhausted);
                assert_usage(&admission, held.request());
                assert_eq!(Arc::strong_count(&admission.state), 2);
                let remainder = admission.try_acquire(request(5, 10, 14)).unwrap();
                assert_usage(&admission, request(7, 13, 19));
                drop(remainder);
                assert_usage(&admission, held.request());
            }
        }
        drop(held);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn maximum_weights_are_accounting_only_and_addition_never_wraps() {
        let maximum = limits(usize::MAX, usize::MAX, usize::MAX);
        let admission = Admission::new(maximum).unwrap();
        let full = admission.try_scratch(request(usize::MAX, usize::MAX, usize::MAX)).unwrap();
        assert_eq!(full.budget_bytes(), usize::MAX);
        assert_empty_scratch(full.scratch());
        assert_eq!(full.scratch().decoded_buf.capacity(), 0);
        assert_eq!(full.scratch().stack.capacity(), 0);
        assert_eq!(full.scratch().seen_words.capacity(), 0);
        assert_error(admission.try_acquire(request(1, 0, 0)), ErrorCode::BudgetExhausted);
        drop(full);

        // Leave room for jobs so overflow in each byte counter is reached.
        let held = admission.try_acquire(request(1, usize::MAX - 1, usize::MAX - 1)).unwrap();
        for failed in [request(1, 2, 0), request(1, 0, 2), request(1, 1, 2)] {
            assert_error(admission.try_acquire(failed), ErrorCode::BudgetExhausted);
            assert_usage(&admission, held.request());
        }
        let edge = admission.try_acquire(request(usize::MAX - 1, 1, 1)).unwrap();
        assert_usage(&admission, request(usize::MAX, usize::MAX, usize::MAX));
        drop(held);
        assert_usage(&admission, edge.request());
        drop(edge);
        assert_usage(&admission, ResourceRequest::default());
        assert_eq!(admission.limits(), maximum);
    }

    #[test]
    fn exhaustive_small_requests_match_checked_capacity_arithmetic() {
        for max_jobs in 1..=3 {
            for max_bytes in 0..=2 {
                for max_scratch in 0..=2 {
                    let admission = Admission::new(limits(max_jobs, max_bytes, max_scratch)).unwrap();
                    for held_jobs in 1..=max_jobs {
                        for held_bytes in 0..=max_bytes {
                            for held_scratch in 0..=max_scratch {
                                let held_request = request(held_jobs, held_bytes, held_scratch);
                                let held = admission.try_acquire(held_request).unwrap();
                                for jobs in 0..=max_jobs + 1 {
                                    for bytes in 0..=max_bytes + 1 {
                                        for scratch in 0..=max_scratch + 1 {
                                            let candidate = request(jobs, bytes, scratch);
                                            let result = admission.try_acquire(candidate);
                                            if jobs == 0 {
                                                assert_error(result, ErrorCode::ConstraintViolation);
                                            } else if jobs > max_jobs - held_jobs
                                                || bytes > max_bytes - held_bytes
                                                || scratch > max_scratch - held_scratch
                                            {
                                                assert_error(result, ErrorCode::BudgetExhausted);
                                            } else {
                                                let lease = result.unwrap();
                                                assert_eq!(lease.request(), candidate);
                                                assert_usage(&admission, request(
                                                    held_jobs + jobs,
                                                    held_bytes + bytes,
                                                    held_scratch + scratch,
                                                ));
                                                drop(lease);
                                            }
                                            assert_usage(&admission, held_request);
                                        }
                                    }
                                }
                                drop(held);
                                assert_usage(&admission, ResourceRequest::default());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn clones_share_usage_but_separate_pools_are_independent() {
        let admission = Admission::new(limits(2, 3, 5)).unwrap();
        let clone = admission.clone();
        let independent = Admission::new(admission.limits()).unwrap();
        let held = clone.try_acquire(request(1, 3, 5)).unwrap();
        assert_usage(&admission, held.request());
        assert_usage(&clone, held.request());
        assert_usage(&independent, ResourceRequest::default());
        let other = independent.try_acquire(request(2, 3, 5)).unwrap();
        assert_error(admission.try_acquire(request(1, 1, 0)), ErrorCode::BudgetExhausted);
        drop(held);
        assert_usage(&clone, ResourceRequest::default());
        assert_usage(&independent, other.request());
        drop(other);
        assert_usage(&independent, ResourceRequest::default());
    }

    #[test]
    fn lease_keeps_state_alive_after_every_pool_handle_is_dropped() {
        let admission = Admission::new(limits(2, 7, 11)).unwrap();
        let weak = Arc::downgrade(&admission.state);
        let lease = admission.try_acquire(request(2, 7, 11)).unwrap();
        drop(admission);
        {
            let state = weak.upgrade().unwrap();
            assert_eq!(state.jobs.load(Ordering::Acquire), 2);
            assert_eq!(state.retained_bytes.load(Ordering::Acquire), 7);
            assert_eq!(state.scratch_bytes.load(Ordering::Acquire), 11);
        }
        drop(lease);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn scratch_is_fresh_and_clear_retains_capacities_and_reservation() {
        let admission = Admission::new(limits(2, 7, usize::MAX)).unwrap();
        let requested = request(1, 7, usize::MAX);
        let mut lease = admission.try_scratch(requested).unwrap();
        assert_eq!(lease.lease.request(), requested);
        assert_empty_scratch(lease.scratch());
        assert_eq!(lease.scratch().decoded_buf.capacity(), 0);
        assert_eq!(lease.scratch().stack.capacity(), 0);
        assert_eq!(lease.scratch().seen_words.capacity(), 0);
        let budget = lease.budget_bytes();
        let scratch = lease.scratch_mut();
        scratch.decoded_buf.try_reserve_exact(4).unwrap();
        scratch.stack.try_reserve_exact(2).unwrap();
        scratch.seen_words.try_reserve_exact(2).unwrap();
        let capacities = (
            scratch.decoded_buf.capacity(),
            scratch.stack.capacity(),
            scratch.seen_words.capacity(),
        );
        let logical_bytes = capacities.1.checked_mul(size_of::<FramePhase>())
            .and_then(|stack| capacities.0.checked_add(stack))
            .and_then(|bytes| bytes.checked_add(capacities.2.checked_mul(size_of::<u64>())?))
            .unwrap();
        assert!(logical_bytes <= budget);
        scratch.decoded_buf.extend_from_slice(b"json");
        scratch.stack.extend_from_slice(&[FramePhase::RootStart, FramePhase::ArrayStart]);
        scratch.seen_words.extend_from_slice(&[1, u64::MAX]);
        scratch.clear();
        assert_empty_scratch(scratch);
        assert_eq!((scratch.decoded_buf.capacity(), scratch.stack.capacity(),
            scratch.seen_words.capacity()), capacities);
        assert_usage(&admission, requested);
        assert_error(admission.try_scratch(request(1, 0, 1)), ErrorCode::BudgetExhausted);
        assert_usage(&admission, requested);
        drop(lease);
        assert_usage(&admission, ResourceRequest::default());
        let fresh = admission.try_scratch(requested).unwrap();
        assert_empty_scratch(fresh.scratch());
        assert_eq!(fresh.scratch().decoded_buf.capacity(), 0);
        assert_eq!(fresh.scratch().stack.capacity(), 0);
        assert_eq!(fresh.scratch().seen_words.capacity(), 0);
        drop(fresh);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn simultaneously_held_scratch_leases_have_independent_state() {
        let admission = Admission::new(limits(2, 0, 128)).unwrap();
        let mut first = admission.try_scratch(request(1, 0, 64)).unwrap();
        let second = admission.try_scratch(request(1, 0, 64)).unwrap();
        first.scratch_mut().decoded_buf.try_reserve_exact(1).unwrap();
        assert!(first.scratch().decoded_buf.capacity() <= first.budget_bytes());
        first.scratch_mut().decoded_buf.push(b'x');
        assert_empty_scratch(second.scratch());
        assert_eq!(second.scratch().decoded_buf.capacity(), 0);
        assert_eq!(first.scratch().decoded_buf.as_slice(), b"x");
        assert_usage(&admission, request(2, 0, 128));
        drop(second);
        assert_usage(&admission, request(1, 0, 64));
        drop(first);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn admitted_value_drops_before_its_reservation_is_released() {
        struct ObserveDrop {
            admission: Admission,
            expected: ResourceRequest,
            drops: Arc<AtomicUsize>,
        }

        impl Drop for ObserveDrop {
            fn drop(&mut self) {
                assert_usage(&self.admission, self.expected);
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        let admission = Admission::new(limits(1, 32, 8)).unwrap();
        let expected = request(1, 32, 8);
        let drops = Arc::new(AtomicUsize::new(0));
        let admitted = Admitted {
            value: ObserveDrop { admission: admission.clone(), expected, drops: drops.clone() },
            lease: admission.try_acquire(expected).unwrap(),
        };
        assert_eq!(admitted.reservation(), expected);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(admitted);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn moving_and_transferring_a_reservation_never_releases_it_early() {
        let admission = Admission::new(limits(2, 8, 16)).unwrap();
        let expected = request(2, 8, 16);
        let admitted = Admitted {
            value: 42u32,
            lease: admission.try_acquire(expected).unwrap(),
        };
        let (value, lease) = admitted.into_parts();
        assert_eq!(value, 42);
        assert_eq!(lease.request(), expected);
        assert_usage(&admission, expected);
        let mut owner = Some(lease);
        let transferred = owner.take().unwrap();
        drop(owner);
        assert_usage(&admission, expected);
        assert_error(admission.try_acquire(request(1, 0, 0)), ErrorCode::BudgetExhausted);
        drop(transferred);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn reservations_are_released_on_early_error_return() {
        fn fail(admission: &Admission) -> Result<()> {
            let _lease = admission.try_scratch(request(1, 2, 3))?;
            Err(runtime_error(ErrorCode::Cancelled, "test cancelled after admission"))
        }
        let admission = Admission::new(limits(1, 2, 3)).unwrap();
        assert_error(fail(&admission), ErrorCode::Cancelled);
        assert_usage(&admission, ResourceRequest::default());
        let lease = admission.try_acquire(request(1, 2, 3)).unwrap();
        drop(lease);
        assert_usage(&admission, ResourceRequest::default());
    }

    #[test]
    fn public_owners_support_the_required_send_boundary() {
        fn assert_send<T: Send + 'static>() {}
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Admission>();
        assert_send::<AdmissionLease>();
        assert_send::<ScratchLease>();
        assert_send::<Admitted<Vec<u8>>>();
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn unwinding_releases_plain_and_scratch_leases() {
        extern crate std;

        let admission = Admission::new(limits(2, 6, 10)).unwrap();
        let unwind = std::panic::catch_unwind(|| {
            let _plain = admission.try_acquire(request(1, 2, 3)).unwrap();
            let _scratch = admission.try_scratch(request(1, 4, 7)).unwrap();
            assert_usage(&admission, request(2, 6, 10));
            panic!("deliberate unwind with two live reservations");
        });
        assert!(unwind.is_err());
        assert_usage(&admission, ResourceRequest::default());
        let lease = admission.try_acquire(request(2, 6, 10)).unwrap();
        drop(lease);
        assert_usage(&admission, ResourceRequest::default());
    }
}
