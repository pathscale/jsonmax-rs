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
    Ok(unsafe { ParsedMessage::new_proven(payload, decoded, value, proof) })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::{task::Wake, vec};
    use core::{pin::Pin, task::{Context, Poll, Waker}};
    use jmax_abi::{ValidationProof, schema::SchemaIr, span::{Span32, SpanSource, StrRef}};
    use jmax_abi::syntax::FramePhase;
    use self::std::sync::Mutex;

    const FINGERPRINT: u64 = 0x5731_0029;

    // A deterministic host simulation. CPU work is run only by the explicit
    // driver, never by Future::poll. No platform runtime or worker is needed.
    struct ManualTask<T> {
        job: Option<Box<dyn FnOnce() -> Result<T> + Send + 'static>>,
        output: Option<Result<T>>,
    }

    // No field is structurally pinned or exposed through a pinned projection.
    impl<T> Unpin for ManualTask<T> {}

    impl<T> ManualTask<T> {
        fn run_cpu(&mut self) -> bool {
            match self.job.take() {
                Some(job) => {
                    self.output = Some(job());
                    true
                }
                None => false,
            }
        }
    }

    impl<T> Future for ManualTask<T> {
        type Output = Result<T>;

        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            match self.output.take() {
                Some(output) => Poll::Ready(output),
                None => Poll::Pending,
            }
        }
    }

    #[derive(Default)]
    struct ManualExecutor {
        submissions: AtomicUsize,
        rejection: Option<Error>,
    }

    impl CpuExecutor for ManualExecutor {
        type Task<T> = ManualTask<T> where T: Send + 'static;

        fn submit<T: Send + 'static>(
            &self,
            job: Box<dyn FnOnce() -> Result<T> + Send + 'static>,
        ) -> Result<Self::Task<T>> {
            self.submissions.fetch_add(1, Ordering::Relaxed);
            if let Some(error) = &self.rejection {
                drop(job);
                return Err(error.clone());
            }
            Ok(ManualTask { job: Some(job), output: None })
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn poll_task<T>(task: &mut ManualTask<T>) -> Poll<Result<T>> {
        let waker = Waker::from(Arc::new(NoopWake));
        Pin::new(task).poll(&mut Context::from_waker(&waker))
    }

    fn complete<T>(mut task: ManualTask<T>) -> Result<T> {
        assert!(task.run_cpu());
        assert!(!task.run_cpu(), "a CPU job must never execute twice");
        match poll_task(&mut task) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("explicit CPU execution must complete the job"),
        }
    }

    struct TestValidator<F>(F);

    impl<F> ParseValidator for TestValidator<F>
    where
        F: Fn(Payload, Limits, &mut Scratch, usize, usize, &CancellationToken)
            -> Result<ValidatedSlots> + Send + Sync + 'static,
    {
        fn validate(
            &self,
            payload: Payload,
            limits: Limits,
            scratch: &mut Scratch,
            scratch_budget_bytes: usize,
            retained_budget_bytes: usize,
            cancellation: &CancellationToken,
        ) -> Result<ValidatedSlots> {
            (self.0)(payload, limits, scratch, scratch_budget_bytes,
                retained_budget_bytes, cancellation)
        }
    }

    fn validator<F>(validate: F) -> Arc<TestValidator<F>>
    where
        F: Fn(Payload, Limits, &mut Scratch, usize, usize, &CancellationToken)
            -> Result<ValidatedSlots> + Send + Sync + 'static,
    {
        Arc::new(TestValidator(validate))
    }

    // A complete validator for this test schema's finite language. It accepts
    // exactly these five JSON literals, computes their values and references,
    // and only then issues evidence. It is not a general-purpose JSON parser.
    fn literal_slots(payload: Payload) -> Result<ValidatedSlots> {
        let (decoded, slots) = match payload.as_slice() {
            b"null" => (Box::<[u8]>::default(), Vec::new()),
            b"true" => (Box::<[u8]>::default(), vec![SlotValue::Bool(true)]),
            b"false" => (Box::<[u8]>::default(), vec![SlotValue::Bool(false)]),
            b"\"a\"" => (Box::<[u8]>::default(), vec![SlotValue::Str(StrRef {
                span: Span32::new(1, 1), source: SpanSource::RawPayload, scalar_len: 1,
            })]),
            b"\"a\\n\"" => (b"a\n".to_vec().into_boxed_slice(), vec![SlotValue::Str(StrRef {
                span: Span32::new(0, 2), source: SpanSource::DecodedArena, scalar_len: 2,
            })]),
            _ => return Err(parser_error()),
        };
        // SAFETY: The exhaustive byte comparisons above validate the complete
        // JSON input against this finite schema. Each slot is its exact value;
        // string spans and scalar counts match the immutable owners. Checked
        // length conversions below cannot truncate either owner.
        let proof = unsafe { ValidationProof::issue_unchecked(
            FINGERPRINT,
            u32::try_from(payload.len()).unwrap(),
            u32::try_from(decoded.len()).unwrap(),
        ) };
        Ok(ValidatedSlots { payload, decoded, slots, proof })
    }

    fn parser_error() -> Error {
        Error::new(ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 3,
            "input is outside the literal test schema").with_schema(17, 29)
    }

    fn decode_error() -> Error {
        Error::new(ErrorCode::ConstraintViolation, ErrorSource::Decode, 2,
            "typed test decoder rejected the value").with_schema(31, 7)
    }

    fn job(bytes: &[u8]) -> ParseJob {
        ParseJob {
            payload: Payload::from_slice(bytes),
            limits: Limits::strict_default(),
            cancellation: CancellationToken::new(),
        }
    }

    fn request(retained_bytes: usize) -> ResourceRequest {
        ResourceRequest { jobs: 1, retained_bytes, scratch_bytes: 0 }
    }

    // Seed the frozen root's counters directly, so Wave 1 tests do not require
    // the later admission implementation. Every seeded reservation has exactly
    // one owning lease, including deliberately invalid requests for preflight.
    fn lease(request: ResourceRequest) -> (Arc<AdmissionState>, ScratchLease) {
        let state = Arc::new(AdmissionState {
            limits: AdmissionLimits {
                max_jobs: request.jobs.max(1),
                max_retained_bytes: request.retained_bytes,
                max_scratch_bytes: request.scratch_bytes,
            },
            jobs: AtomicUsize::new(request.jobs),
            retained_bytes: AtomicUsize::new(request.retained_bytes),
            scratch_bytes: AtomicUsize::new(request.scratch_bytes),
        });
        let lease = ScratchLease {
            scratch: Scratch::default(),
            lease: AdmissionLease { state: state.clone(), request },
        };
        (state, lease)
    }

    fn assert_accounting(state: &AdmissionState, expected: ResourceRequest) {
        assert_eq!(state.jobs.load(Ordering::Acquire), expected.jobs);
        assert_eq!(state.retained_bytes.load(Ordering::Acquire), expected.retained_bytes);
        assert_eq!(state.scratch_bytes.load(Ordering::Acquire), expected.scratch_bytes);
    }

    fn assert_runtime_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
    }

    fn error_of<T>(result: Result<T>) -> Error {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected failure"),
        }
    }

    #[test]
    fn invalid_submissions_never_reach_the_executor_and_release_owners() {
        for case in 0..6 {
            let executor = ManualExecutor::default();
            let mut input = job(b"null");
            let mut reservation = request(4);
            let expected = match case {
                0 => {
                    input.limits.max_payload_bytes = 0;
                    ErrorCode::ConstraintViolation
                }
                1 => {
                    input.limits.max_depth = 0;
                    ErrorCode::ConstraintViolation
                }
                2 => {
                    input.limits.max_payload_bytes = 3;
                    ErrorCode::PayloadTooLarge
                }
                3 => {
                    reservation.jobs = 0;
                    ErrorCode::BudgetExhausted
                }
                4 => {
                    reservation.retained_bytes = 3;
                    ErrorCode::BudgetExhausted
                }
                _ => {
                    input.cancellation.cancel();
                    ErrorCode::Cancelled
                }
            };
            let validate = validator(|_, _, _, _, _, _| panic!("preflight must reject"));
            let weak = Arc::downgrade(&validate);
            let (state, lease) = lease(reservation);
            let result = submit_parse(&executor, validate, input, lease);
            assert_runtime_error(error_of(result), expected);
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
            assert!(weak.upgrade().is_none());
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn preflight_checks_inclusive_payload_and_u32_boundaries_without_allocating() {
        let limits = Limits::permissive_default();
        let cancellation = CancellationToken::new();
        let largest = u32::MAX as usize;
        assert!(preflight(0, limits, request(0), &cancellation).is_ok());
        assert!(preflight(largest, limits, request(largest), &cancellation).is_ok());
        if let Some(too_large) = largest.checked_add(1) {
            assert_runtime_error(error_of(preflight(
                too_large, limits, request(too_large), &cancellation,
            )), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn exact_budgets_and_limits_reach_validator_and_scratch_is_cleared() {
        let executor = ManualExecutor::default();
        let mut input = job(b"null");
        input.limits = Limits {
            max_payload_bytes: 4, max_decoded_bytes: 0, max_depth: 1,
            max_fields_per_object: 0, max_string_bytes: 0, max_number_digits: 0,
            max_array_items: 0, reject_duplicate_keys: false, reject_unknown_keys: false,
        };
        let expected_limits = input.limits;
        let mut scratch = Scratch::default();
        scratch.decoded_buf.push(91);
        scratch.stack.push(FramePhase::ObjectValue);
        scratch.seen_words.push(u64::MAX);
        let capacities = (scratch.decoded_buf.capacity(), scratch.stack.capacity(),
            scratch.seen_words.capacity());
        let scratch_bytes = capacities.0 + capacities.1 * size_of::<FramePhase>()
            + capacities.2 * size_of::<u64>();
        let reservation = ResourceRequest { jobs: 2, retained_bytes: 4, scratch_bytes };
        let (state, mut lease) = lease(reservation);
        lease.scratch = scratch;
        let validate = validator(move |payload, limits, scratch, scratch_budget, retained, token| {
            assert_eq!(limits, expected_limits);
            assert_eq!((scratch_budget, retained), (scratch_bytes, 4));
            assert!(!token.is_cancelled());
            assert!(scratch.decoded_buf.is_empty());
            assert!(scratch.stack.is_empty());
            assert!(scratch.seen_words.is_empty());
            assert_eq!((scratch.decoded_buf.capacity(), scratch.stack.capacity(),
                scratch.seen_words.capacity()), capacities);
            literal_slots(payload)
        });
        let admitted = complete(submit_parse(&executor, validate, input, lease).unwrap()).unwrap();
        assert_eq!(admitted.value().payload.as_slice(), b"null");
        assert!(admitted.value().decoded.is_empty());
        assert_eq!(admitted.reservation(), reservation);
        assert_accounting(&state, reservation);
        drop(admitted);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn executor_rejection_preserves_error_and_drops_unexecuted_job() {
        let rejection = Error::new(ErrorCode::BudgetExhausted, ErrorSource::Transport,
            19, "host CPU queue full").with_schema(5, 6);
        let executor = ManualExecutor {
            submissions: AtomicUsize::new(0), rejection: Some(rejection.clone()),
        };
        let validate = validator(|_, _, _, _, _, _| panic!("rejected job cannot run"));
        let weak = Arc::downgrade(&validate);
        let (state, lease) = lease(request(4));
        assert_eq!(error_of(submit_parse(&executor, validate, job(b"null"), lease)), rejection);
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 1);
        assert!(weak.upgrade().is_none());
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn queued_cancellation_skips_validation_and_releases_reservation() {
        let executor = ManualExecutor::default();
        let input = job(b"null");
        let cancellation = input.cancellation.clone();
        let validate = validator(|_, _, _, _, _, _| panic!("cancelled job cannot validate"));
        let (state, lease) = lease(request(4));
        let task = submit_parse(&executor, validate, input, lease).unwrap();
        cancellation.cancel();
        assert_runtime_error(error_of(complete(task)), ErrorCode::Cancelled);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn cancellation_during_validation_suppresses_success() {
        let executor = ManualExecutor::default();
        let validate = validator(|payload, _, _, _, _, cancellation| {
            let validated = literal_slots(payload)?;
            cancellation.cancel();
            Ok(validated)
        });
        let (state, lease) = lease(request(4));
        let task = submit_parse(&executor, validate, job(b"null"), lease).unwrap();
        assert_runtime_error(error_of(complete(task)), ErrorCode::Cancelled);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn parser_errors_keep_their_complete_context_even_with_cancellation() {
        for cancelled in [false, true] {
            let executor = ManualExecutor::default();
            let validate = validator(move |_, _, _, _, _, cancellation| {
                if cancelled { cancellation.cancel(); }
                Err(parser_error())
            });
            let (state, lease) = lease(request(4));
            let task = submit_parse(&executor, validate, job(b"null"), lease).unwrap();
            assert_eq!(error_of(complete(task)), parser_error());
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn empty_input_is_validated_and_parser_rejection_is_not_preflight_failure() {
        let executor = ManualExecutor::default();
        let validate = validator(|payload, _, _, _, _, _| literal_slots(payload));
        let (state, lease) = lease(request(0));
        let task = submit_parse(&executor, validate, job(b""), lease).unwrap();
        assert_eq!(error_of(complete(task)), parser_error());
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 1);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn wrong_payload_bytes_and_corrupted_proof_lengths_are_rejected() {
        for case in 0..3 {
            let executor = ManualExecutor::default();
            let input = if case == 1 { job(b"false") } else { job(b"null") };
            let validate = validator(move |_, _, _, _, _, _| {
                let mut output = literal_slots(Payload::from_slice(b"null"))?;
                match case {
                    // A genuinely proven output, but for different input bytes
                    // of exactly the same length as the original payload.
                    0 => output = literal_slots(Payload::from_slice(b"true"))?,
                    // Fault injection after valid proof issuance. These safe
                    // field changes must be rejected before any trusted decode.
                    1 => output.payload = Payload::from_slice(b"false"),
                    _ => output.decoded = vec![b'x'].into_boxed_slice(),
                }
                Ok(output)
            });
            let (state, lease) = lease(request(1024));
            let task = submit_parse(&executor, validate, input, lease).unwrap();
            assert_runtime_error(error_of(complete(task)), ErrorCode::InternalError);
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn independently_owned_exact_payload_copy_is_accepted() {
        let executor = ManualExecutor::default();
        let validate = validator(|payload, _, _, _, _, _| {
            let copy = Payload::from_slice(payload.as_slice());
            assert_ne!(copy.as_slice().as_ptr(), payload.as_slice().as_ptr());
            literal_slots(copy)
        });
        let (state, lease) = lease(request(4));
        let output = complete(submit_parse(&executor, validate, job(b"null"), lease).unwrap())
            .unwrap();
        assert_eq!(output.value().payload.as_slice(), b"null");
        drop(output);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn retained_bound_uses_slot_capacity_and_includes_decoded_arena() {
        for bytes in [&b"null"[..], &b"\"a\\n\""[..]] {
            let mut fixture = literal_slots(Payload::from_slice(bytes)).unwrap();
            fixture.slots.reserve_exact(8);
            let capacity = fixture.slots.capacity();
            assert!(capacity > fixture.slots.len());
            let exact = bytes.len() + fixture.decoded.len() + capacity * size_of::<SlotValue>();
            assert!(check_output(&fixture.payload, &fixture, exact).is_ok());
            assert_runtime_error(error_of(check_output(
                &fixture.payload, &fixture, exact - 1,
            )), ErrorCode::BudgetExhausted);
            drop(fixture);

            let executor = ManualExecutor::default();
            let validate = validator(|payload, _, _, _, _, _| {
                let mut output = literal_slots(payload)?;
                // Deliberate validator budget violation exercises the defensive
                // publication check, including empty slots with spare capacity.
                output.slots.reserve_exact(8);
                Ok(output)
            });
            let (state, lease) = lease(request(bytes.len()));
            let task = submit_parse(&executor, validate, job(bytes), lease).unwrap();
            assert_runtime_error(error_of(complete(task)), ErrorCode::BudgetExhausted);
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn retained_bound_detects_each_arithmetic_overflow() {
        assert_eq!(retained_lower_bound(0, 0, 0).unwrap(), 0);
        assert_eq!(retained_lower_bound(usize::MAX, 0, 0).unwrap(), usize::MAX);
        assert_eq!(retained_lower_bound(1, 2, 3).unwrap(), 3 + 3 * size_of::<SlotValue>());
        for (payload, decoded, capacity) in [
            (usize::MAX, 1, 0),
            (usize::MAX, 0, 1),
            (0, 0, usize::MAX / size_of::<SlotValue>() + 1),
        ] {
            assert_runtime_error(error_of(retained_lower_bound(payload, decoded, capacity)),
                ErrorCode::BudgetExhausted);
        }
    }

    #[test]
    fn queued_and_completed_task_drops_release_owners_and_reservations() {
        for run in [false, true] {
            let executor = ManualExecutor::default();
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let validate = validator(move |payload, _, _, _, _, _| {
                seen.fetch_add(1, Ordering::Relaxed);
                literal_slots(payload)
            });
            let weak = Arc::downgrade(&validate);
            let payload = Arc::<[u8]>::from(&b"null"[..]);
            let payload_weak = Arc::downgrade(&payload);
            let mut input = job(b"null");
            input.payload = Payload::Shared(payload);
            let reservation = request(4);
            let (state, lease) = lease(reservation);
            let mut task = submit_parse(&executor, validate, input, lease).unwrap();
            assert!(poll_task(&mut task).is_pending());
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            if run {
                assert!(task.run_cpu());
                assert!(!task.run_cpu());
                assert!(weak.upgrade().is_none());
            }
            assert_accounting(&state, reservation);
            assert!(payload_weak.upgrade().is_some());
            drop(task);
            assert!(payload_weak.upgrade().is_none());
            assert!(weak.upgrade().is_none());
            assert_eq!(calls.load(Ordering::Relaxed), usize::from(run));
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn into_parts_explicitly_transfers_the_live_reservation() {
        let executor = ManualExecutor::default();
        let validate = validator(|payload, _, _, _, _, _| literal_slots(payload));
        let reservation = request(4);
        let (state, lease) = lease(reservation);
        let output = complete(submit_parse(&executor, validate, job(b"null"), lease).unwrap())
            .unwrap();
        let (value, accounting) = output.into_parts();
        assert_accounting(&state, reservation);
        assert_eq!(value.payload.as_slice(), b"null");
        assert_eq!(accounting.request(), reservation);
        drop(value);
        assert_accounting(&state, reservation);
        drop(accounting);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[derive(Debug)]
    struct TextValue(StrRef);

    impl SchemaProvider for TextValue {
        fn schema_ir() -> SchemaIr {
            panic!("submission must use the fingerprint without recompiling the schema")
        }
        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl JsonDecode for TextValue {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            match slots.slots.as_slice() {
                [SlotValue::Str(reference)] => Ok(Self(*reference)),
                _ => Err(decode_error()),
            }
        }
    }

    struct WrongSchema;

    impl SchemaProvider for WrongSchema {
        fn schema_ir() -> SchemaIr {
            panic!("submission must use the fingerprint without recompiling the schema")
        }
        fn schema_fingerprint() -> u64 { FINGERPRINT + 1 }
    }

    impl JsonDecode for WrongSchema {
        fn decode_slots(_: &ValidatedSlots) -> Result<Self> {
            panic!("schema mismatch must be rejected before decoding")
        }
    }

    #[test]
    fn typed_results_preserve_original_owners_proof_and_resolvable_references() {
        for bytes in [&b"\"a\""[..], &b"\"a\\n\""[..]] {
            for shared in [false, true] {
                let executor = ManualExecutor::default();
                let mut input = job(bytes);
                if !shared { input.payload = Payload::from_vec(bytes.to_vec()); }
                let payload_owner = input.payload.clone();
                let decoded_pointer = Arc::new(AtomicUsize::new(0));
                let pointer = decoded_pointer.clone();
                let validate = validator(move |payload, _, _, _, _, _| {
                    let output = literal_slots(payload)?;
                    pointer.store(output.decoded.as_ptr() as usize, Ordering::Relaxed);
                    Ok(output)
                });
                let reservation = request(1024);
                let (state, lease) = lease(reservation);
                let task = submit_typed::<_, _, TextValue>(&executor, validate, input, lease)
                    .unwrap();
                let admitted = complete(task).unwrap();
                let message = admitted.value();
                assert_eq!(message.raw_bytes(), bytes);
                assert_eq!(message.raw_bytes().as_ptr(), payload_owner.as_slice().as_ptr());
                assert_eq!(message.decoded_bytes().as_ptr() as usize,
                    decoded_pointer.load(Ordering::Relaxed));
                assert_eq!(message.proof().schema_fingerprint(), FINGERPRINT);
                assert_eq!(message.proof().payload_len() as usize, bytes.len());
                assert_eq!(message.proof().decoded_len() as usize, message.decoded_bytes().len());
                assert_eq!(message.resolve_str(message.values().0),
                    Some(if bytes.len() == 3 { "a" } else { "a\n" }));
                assert_accounting(&state, reservation);
                drop(admitted);
                assert_accounting(&state, ResourceRequest::default());
            }
        }
    }

    #[test]
    fn typed_schema_mismatch_and_decoder_failure_release_the_reservation() {
        let executor = ManualExecutor::default();
        let validate = validator(|payload, _, _, _, _, _| literal_slots(payload));
        let (state, scratch) = lease(request(4));
        let task = submit_typed::<_, _, WrongSchema>(
            &executor, validate.clone(), job(b"null"), scratch,
        ).unwrap();
        assert_runtime_error(error_of(complete(task)), ErrorCode::SchemaImageInvalid);
        assert_accounting(&state, ResourceRequest::default());

        let (state, scratch) = lease(request(4));
        let task = submit_typed::<_, _, TextValue>(
            &executor, validate, job(b"null"), scratch,
        ).unwrap();
        assert_eq!(error_of(complete(task)), decode_error());
        assert_accounting(&state, ResourceRequest::default());
    }

    struct DecodeControl {
        cancellation: CancellationToken,
        state: Arc<AdmissionState>,
        reservation: ResourceRequest,
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        cancel: bool,
        panic: bool,
    }

    // Used by exactly one test. The canonical static decoder cannot accept
    // runtime context, so that test installs and consumes its own control value.
    static DECODE_CONTROL: Mutex<Option<DecodeControl>> = Mutex::new(None);

    struct ControlledValue {
        state: Arc<AdmissionState>,
        reservation: ResourceRequest,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for ControlledValue {
        fn drop(&mut self) {
            // Value destruction must precede reservation release on success,
            // cancellation, dropping completed tasks, and panic unwinding.
            assert_accounting(&self.state, self.reservation);
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl SchemaProvider for ControlledValue {
        fn schema_ir() -> SchemaIr {
            panic!("submission must use the fingerprint without recompiling the schema")
        }

        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl JsonDecode for ControlledValue {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            let control = DECODE_CONTROL.lock().unwrap().take()
                .expect("the decoder must be called exactly once");
            control.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(slots.payload.as_slice(), b"null");
            assert!(slots.slots.is_empty());
            let value = Self {
                state: control.state,
                reservation: control.reservation,
                drops: control.drops,
            };
            if control.cancel { control.cancellation.cancel(); }
            assert!(!control.panic, "deliberate typed decoder panic");
            Ok(value)
        }
    }

    #[test]
    fn typed_decode_runs_once_and_cancellation_or_unwinding_drops_value_before_lease() {
        // Success, cancellation during decode, and (on unwinding targets) a
        // panic after constructing the decoded value all exercise its Drop.
        let cases = if cfg!(panic = "unwind") { 3 } else { 2 };
        for case in 0..cases {
            let executor = ManualExecutor::default();
            let input = job(b"null");
            let reservation = request(1024);
            let (state, scratch) = lease(reservation);
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let control = DecodeControl {
                cancellation: input.cancellation.clone(),
                state: state.clone(), reservation,
                calls: calls.clone(), drops: drops.clone(),
                cancel: case == 1, panic: case == 2,
            };
            assert!(DECODE_CONTROL.lock().unwrap().replace(control).is_none());
            let validate = validator(|payload, _, _, _, _, _| literal_slots(payload));
            let task = submit_typed::<_, _, ControlledValue>(
                &executor, validate, input, scratch,
            ).unwrap();
            if case == 2 {
                #[cfg(panic = "unwind")]
                {
                    let panic = self::std::panic::catch_unwind(
                        self::std::panic::AssertUnwindSafe(move || complete(task)),
                    );
                    assert!(panic.is_err());
                }
            } else {
                let output = complete(task);
                if case == 1 {
                    assert_runtime_error(error_of(output), ErrorCode::Cancelled);
                } else {
                    let admitted = match output {
                        Ok(admitted) => admitted,
                        Err(error) => panic!("unexpected typed failure: {error}"),
                    };
                    assert_accounting(&state, reservation);
                    assert_eq!(drops.load(Ordering::Relaxed), 0);
                    drop(admitted);
                }
            }
            assert!(DECODE_CONTROL.lock().unwrap().is_none());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn typed_submission_uses_the_same_preflight_as_untyped_submission() {
        let executor = ManualExecutor::default();
        let mut input = job(b"null");
        input.limits.max_depth = 0;
        let validate = validator(|_, _, _, _, _, _| panic!("preflight must reject"));
        let (state, scratch) = lease(request(4));
        let result = submit_typed::<_, _, WrongSchema>(&executor, validate, input, scratch);
        assert_runtime_error(error_of(result), ErrorCode::ConstraintViolation);
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    fn typed_publication_checks_precede_schema_dispatch() {
        for case in 0..3 {
            let executor = ManualExecutor::default();
            let validate = validator(move |payload, _, _, _, _, cancellation| {
                let mut output = literal_slots(payload)?;
                match case {
                    0 => output.decoded = vec![b'x'].into_boxed_slice(),
                    1 => output.slots.reserve_exact(1),
                    _ => cancellation.cancel(),
                }
                Ok(output)
            });
            let (state, scratch) = lease(request(4));
            let task = submit_typed::<_, _, WrongSchema>(
                &executor, validate, job(b"null"), scratch,
            ).unwrap();
            // Reaching schema dispatch would yield SchemaImageInvalid instead.
            let expected = match case {
                0 => ErrorCode::InternalError,
                1 => ErrorCode::BudgetExhausted,
                _ => ErrorCode::Cancelled,
            };
            assert_runtime_error(error_of(complete(task)), expected);
            assert_accounting(&state, ResourceRequest::default());
        }
    }

    #[test]
    fn cleanup_cancellation_is_observed_before_publication() {
        struct CancelOnDrop(CancellationToken);

        impl Drop for CancelOnDrop {
            fn drop(&mut self) { self.0.cancel(); }
        }

        impl ParseValidator for CancelOnDrop {
            fn validate(
                &self,
                payload: Payload,
                _: Limits,
                _: &mut Scratch,
                _: usize,
                _: usize,
                _: &CancellationToken,
            ) -> Result<ValidatedSlots> {
                literal_slots(payload)
            }
        }

        let executor = ManualExecutor::default();
        let input = job(b"null");
        let validate = Arc::new(CancelOnDrop(input.cancellation.clone()));
        let (state, scratch) = lease(request(4));
        let task = submit_parse(&executor, validate, input, scratch).unwrap();
        assert_runtime_error(error_of(complete(task)), ErrorCode::Cancelled);
        assert_accounting(&state, ResourceRequest::default());
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn validator_panic_releases_captured_owners_and_all_counters() {
        let executor = ManualExecutor::default();
        let validate = validator(|_, _, _, _, _, _| panic!("deliberate validator panic"));
        let weak = Arc::downgrade(&validate);
        let payload = Arc::<[u8]>::from(&b"null"[..]);
        let payload_weak = Arc::downgrade(&payload);
        let mut input = job(b"null");
        input.payload = Payload::Shared(payload);
        let reservation = ResourceRequest { jobs: 3, retained_bytes: 128, scratch_bytes: 64 };
        let (state, scratch) = lease(reservation);
        let task = submit_parse(&executor, validate, input, scratch).unwrap();
        let panic = self::std::panic::catch_unwind(
            self::std::panic::AssertUnwindSafe(move || complete(task)),
        );
        assert!(panic.is_err());
        assert!(weak.upgrade().is_none());
        assert!(payload_weak.upgrade().is_none());
        assert_accounting(&state, ResourceRequest::default());
    }
}
