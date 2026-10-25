use jmax_nagoya::*;
use std::{
    future::{Future, Ready, ready},
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

struct Inline;
impl CpuExecutor for Inline {
    type Task<T: Send + 'static> = Ready<Result<T>>;
    fn submit<T: Send + 'static>(
        &self,
        job: Box<dyn FnOnce() -> Result<T> + Send + 'static>,
    ) -> Result<Self::Task<T>> {
        Ok(ready(job()))
    }
}

struct Reject;
impl CpuExecutor for Reject {
    type Task<T: Send + 'static> = Ready<Result<T>>;
    fn submit<T: Send + 'static>(
        &self,
        _job: Box<dyn FnOnce() -> Result<T> + Send + 'static>,
    ) -> Result<Self::Task<T>> {
        Err(Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Decode,
            0,
            "executor full",
        ))
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Preserve,
    Copy,
    Replace,
    Fail,
}
struct Validator(Mode);
impl ParseValidator for Validator {
    fn validate(
        &self,
        payload: Payload,
        _: Limits,
        _: &mut Scratch,
        _: usize,
        _: usize,
        cancellation: &CancellationToken,
    ) -> Result<ValidatedSlots> {
        cancellation.check()?;
        assert_eq!(payload.as_slice(), b"null");
        let payload = match self.0 {
            Mode::Preserve => payload,
            Mode::Copy => Payload::from_slice(payload.as_slice()),
            Mode::Replace => Payload::from_slice(b"true"),
            Mode::Fail => {
                return Err(Error::new(
                    ErrorCode::ConstraintViolation,
                    ErrorSource::Decode,
                    0,
                    "test validation failure",
                ));
            }
        };
        // SAFETY: the fixed test schema accepts null and true, both checked
        // literal JSON values. There are no decoded bytes, slots or references.
        let proof = unsafe { jmax_abi::buffer::ValidationProof::issue_unchecked(7, 4, 0) };
        Ok(ValidatedSlots {
            payload,
            decoded: Vec::new(),
            slots: Vec::new(),
            proof,
        })
    }
}

fn admission() -> Result<Admission> {
    Admission::new(AdmissionLimits {
        max_jobs: 8,
        max_retained_bytes: 8192,
        max_scratch_bytes: 4096,
    })
}
fn job(payload: Payload) -> ParseJob {
    ParseJob {
        payload,
        limits: Limits::strict_default(),
        cancellation: CancellationToken::new(),
    }
}
fn immediate<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("inline executor unexpectedly suspended"),
    }
}

#[test]
fn owner_and_lease_survive_result_handoff() -> Result<()> {
    let admission = admission()?;
    let payload = Payload::from_arc_owner(Arc::new(b"null".to_vec()))?;
    let original = payload.clone();
    let lease = admission.try_parse(&payload, 0, 0)?;
    let result = immediate(submit::submit_parse(
        &Inline,
        Arc::new(Validator(Mode::Preserve)),
        job(payload),
        lease,
    )?)?;
    assert_eq!(admission.snapshot().jobs, 1);
    assert_eq!(admission.snapshot().retained_bytes, 4);
    assert!(result.value().payload.shares_backing_with(&original));
    assert_eq!(
        result.value().payload.as_slice().as_ptr(),
        original.as_slice().as_ptr()
    );
    drop(result);
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    Ok(())
}

#[test]
fn copied_external_output_uses_fallback_and_counts_coexisting_backings() -> Result<()> {
    let admission = admission()?;
    for (additional, succeeds) in [(0, false), (4, true)] {
        let payload = Payload::from_slice(b"null");
        let lease = admission.try_parse(&payload, additional, 0)?;
        let result = immediate(submit::submit_parse(
            &Inline,
            Arc::new(Validator(Mode::Copy)),
            job(payload),
            lease,
        )?);
        assert_eq!(result.is_ok(), succeeds);
        if let Err(error) = &result {
            assert_eq!(error.code, ErrorCode::BudgetExhausted);
        }
        drop(result);
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    }
    Ok(())
}

#[test]
fn tiny_subview_reserves_full_known_backing() -> Result<()> {
    let admission = admission()?;
    let mut backing = vec![b' '; 4096];
    backing[..4].copy_from_slice(b"null");
    let owner = Payload::from_arc_owner(Arc::new(backing))?;
    let view = owner.slice(0..4)?;
    let lease = admission.try_scratch(ResourceRequest {
        jobs: 1,
        retained_bytes: 4,
        scratch_bytes: 0,
    })?;
    let rejected = submit::submit_parse(
        &Inline,
        Arc::new(Validator(Mode::Preserve)),
        job(view.clone()),
        lease,
    );
    assert!(rejected.is_err());
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    let lease = admission.try_parse(&view, 0, 0)?;
    assert_eq!(lease.budget_bytes(), 0);
    assert_eq!(admission.snapshot().retained_bytes, 4096);
    let result = immediate(submit::submit_parse(
        &Inline,
        Arc::new(Validator(Mode::Preserve)),
        job(view),
        lease,
    )?)?;
    assert_eq!(result.reservation().retained_bytes, 4096);
    drop(result);
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    Ok(())
}

#[test]
fn every_failure_path_releases_once() -> Result<()> {
    let admission = admission()?;
    for (mode, expected) in [
        (Mode::Replace, ErrorCode::InternalError),
        (Mode::Fail, ErrorCode::ConstraintViolation),
    ] {
        let payload = Payload::from_slice(b"null");
        let lease = admission.try_parse(&payload, 16, 0)?;
        let result = immediate(submit::submit_parse(
            &Inline,
            Arc::new(Validator(mode)),
            job(payload),
            lease,
        )?);
        assert_eq!(result.err().map(|e| e.code), Some(expected));
        assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    }
    let payload = Payload::from_slice(b"null");
    let lease = admission.try_parse(&payload, 0, 0)?;
    assert!(
        submit::submit_parse(
            &Reject,
            Arc::new(Validator(Mode::Preserve)),
            job(payload),
            lease
        )
        .is_err()
    );
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    let payload = Payload::from_slice(b"null");
    let lease = admission.try_parse(&payload, 0, 0)?;
    let job = job(payload);
    job.cancellation.cancel();
    assert!(
        submit::submit_parse(&Inline, Arc::new(Validator(Mode::Preserve)), job, lease).is_err()
    );
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    Ok(())
}
