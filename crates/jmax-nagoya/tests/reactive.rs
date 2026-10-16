#![cfg(feature = "nagoya-rt")]

use jmax_nagoya::{
    reactive::{HostedPool, ReactiveRing},
    *,
};
use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn poll<F: Future>(future: Pin<&mut F>, wakes: &Arc<Wakes>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(&Waker::from(Arc::clone(wakes))))
}
fn immediate<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match poll(future.as_mut(), &Arc::new(Wakes::default())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("operation unexpectedly suspended"),
    }
}

struct Validator;
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
        // SAFETY: fixed test schema accepts this checked null literal. No
        // decoded bytes, slots or references exist in this test output.
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
        max_retained_bytes: 1024,
        max_scratch_bytes: 1024,
    })
}
fn job() -> ParseJob {
    ParseJob {
        payload: Payload::from_slice(b"null"),
        limits: Limits::strict_default(),
        cancellation: CancellationToken::new(),
    }
}

#[test]
fn full_ring_suspends_then_wakes_producer_and_reader() -> Result<()> {
    let ring = ReactiveRing::new(1)?;
    immediate(ring.send(1))?;
    let wakes = Arc::new(Wakes::default());
    let mut second = Box::pin(ring.send(2));
    assert!(poll(second.as_mut(), &wakes).is_pending());
    assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    assert_eq!(immediate(ring.recv()), Some(1));
    assert!(wakes.0.load(Ordering::Relaxed) > 0);
    assert!(matches!(poll(second.as_mut(), &wakes), Poll::Ready(Ok(()))));
    drop(second);
    assert_eq!(immediate(ring.recv()), Some(2));
    let mut reader = Box::pin(ring.recv());
    assert!(poll(reader.as_mut(), &wakes).is_pending());
    let before = wakes.0.load(Ordering::Relaxed);
    ring.close();
    assert!(wakes.0.load(Ordering::Relaxed) > before);
    assert_eq!(poll(reader.as_mut(), &wakes), Poll::Ready(None));
    assert!(immediate(ring.send(3)).is_err());
    Ok(())
}

#[test]
fn cancelled_waiting_submission_has_no_sequence_gap_and_releases_once() -> Result<()> {
    let admission = admission()?;
    let (pool, worker) = HostedPool::<Validator>::new(1, 1, 10)?;
    let first = job();
    let lease = admission.try_parse(&first.payload, 0, 0)?;
    assert_eq!(
        immediate(pool.submit(Arc::new(Validator), first, lease))?,
        10
    );
    let waiting = job();
    let cancellation = waiting.cancellation.clone();
    let lease = admission.try_parse(&waiting.payload, 0, 0)?;
    let wakes = Arc::new(Wakes::default());
    let mut waiting = Box::pin(pool.submit(Arc::new(Validator), waiting, lease));
    assert!(poll(waiting.as_mut(), &wakes).is_pending());
    cancellation.cancel();
    assert!(wakes.0.load(Ordering::Relaxed) > 0);
    assert!(
        matches!(poll(waiting.as_mut(), &wakes), Poll::Ready(Err(e)) if e.code == ErrorCode::Cancelled)
    );
    drop(waiting);
    assert_eq!(admission.snapshot().jobs, 1);
    let mut running = Box::pin(worker.run());
    assert!(poll(running.as_mut(), &wakes).is_pending());
    let first = immediate(pool.recv()).ok_or_else(|| {
        Error::new(
            ErrorCode::InternalError,
            ErrorSource::Decode,
            0,
            "missing completion",
        )
    })?;
    assert_eq!(first.sequence, 10);
    assert!(first.result.is_ok());
    drop(first);
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    let next = job();
    let cancellation = next.cancellation.clone();
    let lease = admission.try_parse(&next.payload, 0, 0)?;
    assert_eq!(
        immediate(pool.submit(Arc::new(Validator), next, lease))?,
        11
    );
    cancellation.cancel();
    assert!(poll(running.as_mut(), &wakes).is_pending());
    let next = immediate(pool.recv()).ok_or_else(|| {
        Error::new(
            ErrorCode::InternalError,
            ErrorSource::Decode,
            0,
            "missing completion",
        )
    })?;
    assert_eq!(next.sequence, 11);
    assert!(matches!(next.result, Err(e) if e.code == ErrorCode::Cancelled));
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    pool.close();
    assert!(poll(running.as_mut(), &wakes).is_ready());
    Ok(())
}

#[test]
fn completion_backpressure_cancellation_and_shutdown_release_all_leases() -> Result<()> {
    let admission = admission()?;
    let (pool, worker) = HostedPool::<Validator>::new(2, 1, 0)?;
    let mut cancellation = None;
    for index in 0..2 {
        let job = job();
        if index == 1 {
            cancellation = Some(job.cancellation.clone());
        }
        let lease = admission.try_parse(&job.payload, 0, 0)?;
        assert_eq!(
            immediate(pool.submit(Arc::new(Validator), job, lease))?,
            index
        );
    }
    let wakes = Arc::new(Wakes::default());
    let mut running = Box::pin(worker.run());
    assert!(poll(running.as_mut(), &wakes).is_pending()); // result 0 fills completion ring
    assert!(poll(running.as_mut(), &wakes).is_pending()); // result 1 waits on capacity
    assert_eq!(admission.snapshot().jobs, 2);
    let before = wakes.0.load(Ordering::Relaxed);
    if let Some(token) = cancellation {
        token.cancel();
    }
    assert!(wakes.0.load(Ordering::Relaxed) > before);
    assert!(poll(running.as_mut(), &wakes).is_pending()); // positional error now waits
    assert_eq!(admission.snapshot().jobs, 1);
    let first = immediate(pool.recv());
    assert_eq!(first.as_ref().map(|c| c.sequence), Some(0));
    drop(first);
    assert!(poll(running.as_mut(), &wakes).is_pending());
    let second = immediate(pool.recv());
    assert_eq!(second.as_ref().map(|c| c.sequence), Some(1));
    assert!(
        matches!(second, Some(Completion { result: Err(e), .. }) if e.code == ErrorCode::Cancelled)
    );
    let queued = job();
    let lease = admission.try_parse(&queued.payload, 0, 0)?;
    immediate(pool.submit(Arc::new(Validator), queued, lease))?;
    pool.close();
    assert!(poll(running.as_mut(), &wakes).is_ready());
    drop(running);
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    assert!(immediate(pool.recv()).is_none());
    Ok(())
}

#[test]
fn worker_drop_and_rejected_submission_release_pending_owners() -> Result<()> {
    let admission = admission()?;
    let (pool, worker) = HostedPool::<Validator>::new(1, 1, 0)?;
    let request = job();
    let lease = admission.try_parse(&request.payload, 0, 0)?;
    immediate(pool.submit(Arc::new(Validator), request, lease))?;
    drop(worker);
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    let request = job();
    let lease = admission.try_parse(&request.payload, 0, 0)?;
    assert!(immediate(pool.submit(Arc::new(Validator), request, lease)).is_err());
    drop(pool);
    assert_eq!(admission.snapshot(), AdmissionSnapshot::default());
    Ok(())
}
