//! Owned CPU parse submission and checked publication of validator output.
//!
//! The host executor schedules CPU work; this module never executes a job on
//! behalf of the executor. Validators enforce parser limits and both supplied
//! memory budgets. Submission additionally checks the observable retained-size
//! lower bound, including known shared backings. Bytes hidden capacity and a
//! decoded type's private allocations remain the validator's responsibility.

use super::*;
use core::mem::size_of;
use jmax_abi::schema::SlotValue;

/// Submit one owned parse, retaining its reservation with the validated output.
///
/// Invalid limits, oversized input, insufficient admission, and pre-existing
/// cancellation are rejected before contacting the executor. Dropping a task
/// delegates cancellation and eventual cleanup to the executor's contract.
pub fn submit_parse<E, V>(
    executor: &E,
    validator: Arc<V>,
    job: ParseJob,
    lease: ScratchLease,
) -> Result<E::Task<Admitted<ValidatedSlots>>>
where
    E: CpuExecutor,
    V: ParseValidator,
{
    submit_owned(executor, validator, job, lease, Ok)
}

/// Submit validation followed by exactly one typed decode on the CPU executor.
///
/// The issued proof must match `T`'s schema. The original validated buffers and
/// proof accompany the decoded value. The caller's retained-byte reservation
/// must also cover any additional allocations owned by `T`; `JsonDecode` is
/// trusted to respect that declared worst-case budget and preserve references.
pub fn submit_typed<E, V, T>(
    executor: &E,
    validator: Arc<V>,
    job: ParseJob,
    lease: ScratchLease,
) -> Result<E::Task<Admitted<ParsedMessage<T>>>>
where
    E: CpuExecutor,
    V: ParseValidator,
    T: JsonDecode + SchemaProvider + Send + 'static,
{
    submit_owned(executor, validator, job, lease, decode_typed::<T>)
}

// Keep the pending owners together, with their reservation last. In particular,
// executor rejection or dropping an unexecuted closure releases owners first.
// Capturing this aggregate through `run` also avoids disjoint field captures.
pub(crate) struct Submission<V> {
    pub(crate) job: ParseJob,
    pub(crate) validator: Arc<V>,
    pub(crate) lease: ScratchLease,
}

fn submit_owned<E, V, T>(
    executor: &E,
    validator: Arc<V>,
    job: ParseJob,
    lease: ScratchLease,
    finish: fn(ValidatedSlots) -> Result<T>,
) -> Result<E::Task<Admitted<T>>>
where
    E: CpuExecutor,
    V: ParseValidator,
    T: Send + 'static,
{
    let submission = Submission {
        job,
        validator,
        lease,
    };
    preflight(
        &submission.job.payload,
        submission.job.limits,
        submission.lease.lease.request(),
        &submission.job.cancellation,
    )?;
    executor.submit(Box::new(move || submission.run(finish)))
}

pub(crate) fn preflight(
    payload: &Payload,
    limits: Limits,
    request: ResourceRequest,
    cancellation: &CancellationToken,
) -> Result<()> {
    let payload_len = payload.len();
    if limits.max_payload_bytes == 0 || limits.max_depth == 0 {
        return Err(runtime_error(
            ErrorCode::ConstraintViolation,
            "payload and depth limits must be nonzero",
        ));
    }
    let length = u32::try_from(payload_len).map_err(|_| {
        runtime_error(
            ErrorCode::PayloadTooLarge,
            "payload length exceeds ABI range",
        )
    })?;
    if length > limits.max_payload_bytes {
        return Err(runtime_error(
            ErrorCode::PayloadTooLarge,
            "payload limit exceeded",
        ));
    }
    if request.jobs == 0 || request.retained_bytes < admission::payload_retained_bytes(payload) {
        return Err(runtime_error(
            ErrorCode::BudgetExhausted,
            "parse reservation does not cover the job and payload",
        ));
    }
    cancellation.check()
}

impl<V: ParseValidator> Submission<V> {
    pub(crate) fn run<T>(mut self, finish: fn(ValidatedSlots) -> Result<T>) -> Result<Admitted<T>> {
        self.job.cancellation.check()?;
        self.lease.scratch.clear();
        let request = self.lease.lease.request();
        // Keep the original owner alive for identity checks, falling back to
        // byte comparison only when an external validator changes backing.
        let output = self.validator.validate_output(
            self.job.payload.clone(),
            self.job.limits,
            &mut self.lease.scratch,
            request.scratch_bytes,
            request.retained_bytes,
            &self.job.cancellation,
        )?;
        self.job.cancellation.check()?;
        check_output(&self.job.payload, &output, request.retained_bytes)?;
        let validated = output.into_slots();
        self.job.cancellation.check()?;
        let value = finish(validated)?;
        drop(self.lease.scratch);
        drop(self.validator);
        // Decoding and owner cleanup may observe or request cancellation too.
        // Check immediately before publication, while still owning the token.
        self.job.cancellation.check()?;
        drop(self.job);
        Ok(Admitted {
            value,
            lease: self.lease.lease,
        })
    }
}

fn retained_lower_bound(
    payload_len: usize,
    decoded_len: usize,
    slot_capacity: usize,
) -> Result<usize> {
    slot_capacity
        .checked_mul(size_of::<SlotValue>())
        .and_then(|slots| payload_len.checked_add(decoded_len)?.checked_add(slots))
        .ok_or_else(|| {
            runtime_error(
                ErrorCode::BudgetExhausted,
                "retained parse storage size overflow",
            )
        })
}

// The result owns the exact immutable payload while this check runs. A copied
// naked pointer capability cannot enter this path through an external validator.
fn capability_matches(original: &Payload, result: &ValidationOutput) -> bool {
    let Some(capability) = result.capability else {
        return false;
    };
    let validated = result.slots();
    let matches = |payload: &Payload| {
        capability.matches_payload(
            payload,
            validated.decoded.len(),
            validated.proof.schema_fingerprint(),
            capability.policy_fingerprint(),
        )
    };
    original.shares_backing_with(&validated.payload)
        && matches(original)
        && matches(&validated.payload)
}

pub(crate) fn check_output(
    original: &Payload,
    result: &ValidationOutput,
    retained_budget_bytes: usize,
) -> Result<()> {
    let validated = result.slots();
    let output = &validated.payload;
    let trusted_owner = capability_matches(original, result);
    if (!trusted_owner && output.as_slice() != original.as_slice())
        || usize::try_from(validated.proof.payload_len()).ok() != Some(original.len())
        || usize::try_from(validated.proof.decoded_len()).ok() != Some(validated.decoded.len())
    {
        return Err(runtime_error(
            ErrorCode::InternalError,
            "validator returned inconsistent payload ownership or proof lengths",
        ));
    }
    let retained = retained_lower_bound(
        admission::handoff_backing_bytes(original, output)?,
        validated.decoded.capacity(),
        validated.slots.capacity(),
    )?;
    if retained > retained_budget_bytes {
        return Err(runtime_error(
            ErrorCode::BudgetExhausted,
            "validated storage exceeds the retained-byte reservation",
        ));
    }
    Ok(())
}

fn decode_typed<T>(validated: ValidatedSlots) -> Result<ParsedMessage<T>>
where
    T: JsonDecode + SchemaProvider,
{
    if T::schema_fingerprint() != validated.proof.schema_fingerprint() {
        return Err(runtime_error(
            ErrorCode::SchemaImageInvalid,
            "typed decoder schema does not match the validation proof",
        ));
    }
    let value = T::decode_slots(&validated)?;
    let ValidatedSlots {
        payload,
        decoded,
        slots,
        proof,
    } = validated;
    drop(slots);
    // SAFETY: ParseValidator issued this proof for these exact immutable owners
    // and slots. check_output verified the original payload and proof lengths;
    // the fingerprint matches T. The trusted JsonDecode implementation produced
    // values from those slots and must preserve their reference invariants. No
    // owner or proof is replaced, and no proof is issued by submission code.
    Ok(unsafe { ParsedMessage::new_proven(payload, decoded.into_boxed_slice(), value, proof) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jmax_abi::buffer::ValidationProof;

    fn canonical(payload: Payload) -> ValidationOutput {
        assert_eq!(payload.as_slice(), b"null");
        // SAFETY: this test's schema accepts only the checked null literal.
        let proof = unsafe { ValidationProof::issue_unchecked(7, 4, 0) };
        let capability = unsafe { ValidationCapability::issue_for_payload(&payload, 0, 7, 11) };
        ValidationOutput {
            slots: ValidatedSlots {
                payload,
                decoded: Vec::new(),
                slots: Vec::new(),
                proof,
            },
            capability: Some(capability),
        }
    }

    #[test]
    fn canonical_capability_requires_exact_live_owner_range_and_proof() -> Result<()> {
        let backing = Payload::from_arc_owner(Arc::new(b" null ".to_vec()))?;
        let original = backing.slice(1..5)?;
        let result = canonical(original.clone());
        assert!(capability_matches(&original, &result));
        check_output(&original, &result, 6)?;
        assert!(!capability_matches(&backing, &result));
        let copied = Payload::from_slice(b"null");
        assert!(!capability_matches(&copied, &result));
        check_output(&copied, &result, 10)?; // defensive equality fallback
        let mut changed = canonical(original.clone());
        changed.slots.proof = unsafe { ValidationProof::issue_unchecked(8, 4, 0) };
        assert!(!capability_matches(&original, &changed));
        let mut changed = canonical(original.clone());
        changed.slots.decoded.push(b'x');
        assert!(!capability_matches(&original, &changed));
        assert!(check_output(&original, &changed, 128).is_err());
        let external = ValidationOutput::external(result.into_slots());
        assert!(!capability_matches(&original, &external));
        check_output(&original, &external, 6)?;
        Ok(())
    }
}
