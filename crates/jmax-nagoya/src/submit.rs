//! Owned CPU parse submission and checked publication of validator output.
//!
//! The host executor schedules CPU work; this module never executes a job on
//! behalf of the executor. Validators enforce parser limits and both supplied
//! memory budgets. Submission additionally checks the observable retained-size
//! lower bound, which cannot measure shared backing allocations or a decoded
//! type's private allocations.

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
struct Submission<V> {
    job: ParseJob,
    validator: Arc<V>,
    lease: ScratchLease,
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
    let submission = Submission { job, validator, lease };
    preflight(
        submission.job.payload.len(),
        submission.job.limits,
        submission.lease.lease.request(),
        &submission.job.cancellation,
    )?;
    executor.submit(Box::new(move || submission.run(finish)))
}

fn preflight(
    payload_len: usize,
    limits: Limits,
    request: ResourceRequest,
    cancellation: &CancellationToken,
) -> Result<()> {
    if limits.max_payload_bytes == 0 || limits.max_depth == 0 {
        return Err(runtime_error(
            ErrorCode::ConstraintViolation,
            "payload and depth limits must be nonzero",
        ));
    }
    let length = u32::try_from(payload_len).map_err(|_| {
        runtime_error(ErrorCode::PayloadTooLarge, "payload length exceeds ABI range")
    })?;
    if length > limits.max_payload_bytes {
        return Err(runtime_error(ErrorCode::PayloadTooLarge, "payload limit exceeded"));
    }
    if request.jobs == 0 || request.retained_bytes < payload_len {
        return Err(runtime_error(
            ErrorCode::BudgetExhausted,
            "parse reservation does not cover the job and payload",
        ));
    }
    cancellation.check()
}

impl<V: ParseValidator> Submission<V> {
    fn run<T>(mut self, finish: fn(ValidatedSlots) -> Result<T>) -> Result<Admitted<T>> {
        self.job.cancellation.check()?;
        self.lease.scratch.clear();
        let request = self.lease.lease.request();
        // Keep an immutable original owner for byte-for-byte comparison. Both
        // canonical Payload variants clone their shared owner, not their bytes.
        let validated = self.validator.validate(
            self.job.payload.clone(),
            self.job.limits,
            &mut self.lease.scratch,
            request.scratch_bytes,
            request.retained_bytes,
            &self.job.cancellation,
        )?;
        self.job.cancellation.check()?;
        check_output(&self.job.payload, &validated, request.retained_bytes)?;
        self.job.cancellation.check()?;
        let value = finish(validated)?;
        drop(self.lease.scratch);
        drop(self.validator);
        // Decoding and owner cleanup may observe or request cancellation too.
        // Check immediately before publication, while still owning the token.
        self.job.cancellation.check()?;
        drop(self.job);
        Ok(Admitted { value, lease: self.lease.lease })
    }
}

fn retained_lower_bound(
    payload_len: usize,
    decoded_len: usize,
    slot_capacity: usize,
) -> Result<usize> {
    slot_capacity.checked_mul(size_of::<SlotValue>())
        .and_then(|slots| payload_len.checked_add(decoded_len)?.checked_add(slots))
        .ok_or_else(|| runtime_error(
            ErrorCode::BudgetExhausted,
            "retained parse storage size overflow",
        ))
}

fn check_output(
    original: &Payload,
    validated: &ValidatedSlots,
    retained_budget_bytes: usize,
) -> Result<()> {
    if validated.payload.as_slice() != original.as_slice()
        || usize::try_from(validated.proof.payload_len()).ok() != Some(original.len())
        || usize::try_from(validated.proof.decoded_len()).ok() != Some(validated.decoded.len())
    {
        return Err(runtime_error(
            ErrorCode::InternalError,
            "validator returned inconsistent payload ownership or proof lengths",
        ));
    }
    let retained = retained_lower_bound(
        validated.payload.len(), validated.decoded.len(), validated.slots.capacity(),
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
    let ValidatedSlots { payload, decoded, slots, proof } = validated;
    drop(slots);
    // SAFETY: ParseValidator issued this proof for these exact immutable owners
    // and slots. check_output verified the original payload and proof lengths;
    // the fingerprint matches T. The trusted JsonDecode implementation produced
    // values from those slots and must preserve their reference invariants. No
    // owner or proof is replaced, and no proof is issued by submission code.
    Ok(unsafe { ParsedMessage::new_proven(payload, decoded.into_boxed_slice(), value, proof) })
}
