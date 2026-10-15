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
