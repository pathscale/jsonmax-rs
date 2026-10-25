//! Bounded, wake-driven hosted handoff using nagoya's actual sync primitives.
//!
//! One worker owns a FIFO lane, preserving submission/result sequence order even
//! for failures and cancellation. Use independent lanes for parallelism. No
//! borrowed input or reactor-local handle enters either queue.

use super::*;
use alloc::collections::VecDeque;
use core::{future::poll_fn, pin::pin, task::Poll};
use nagoya::sync::{Notify, RwLock};

/// A bounded FIFO with asynchronous capacity/readiness waits. Closing rejects
/// producers and discards queued values immediately when the lock is available,
/// otherwise on the next receive (or final drop).
/// A pending send owns its value until it is accepted, rejected, or dropped.
pub struct ReactiveRing<T> {
    queue: RwLock<VecDeque<T>>,
    capacity: usize,
    closed: AtomicBool,
    readable: Notify,
    writable: Notify,
}

impl<T> ReactiveRing<T> {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(runtime_error(
                ErrorCode::ConstraintViolation,
                "ring capacity must be nonzero",
            ));
        }
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(capacity)
            .map_err(|_| runtime_error(ErrorCode::BudgetExhausted, "ring allocation failed"))?;
        Ok(Self {
            queue: RwLock::new(queue),
            capacity,
            closed: AtomicBool::new(false),
            readable: Notify::new(),
            writable: Notify::new(),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Idempotently wake every producer and consumer. Closing is terminal.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let discarded = self
            .queue
            .try_write()
            .map(|mut queue| core::mem::take(&mut *queue));
        self.readable.notify_waiters();
        self.writable.notify_waiters();
        // User destructors never execute while holding the queue lock.
        drop(discarded);
    }

    pub async fn send(&self, value: T) -> Result<()> {
        self.send_inner(value, None).await
    }

    /// Cancellation while waiting returns the unqueued owner to Drop. Once
    /// enqueued, the worker publishes a positional cancellation completion.
    pub async fn send_cancellable(&self, value: T, token: &CancellationToken) -> Result<()> {
        self.send_inner(value, Some(token)).await
    }

    async fn send_inner(&self, value: T, token: Option<&CancellationToken>) -> Result<()> {
        loop {
            // Register before observing capacity. Broadcasts wake all waiters,
            // including those not yet polled, through Notify's generation.
            let changed = self.writable.notified();
            let mut changed = pin!(changed);
            changed.as_mut().enable();
            if let Some(token) = token {
                token.check()?;
            }
            let mut queue = self.queue.write().await;
            if self.is_closed() {
                return Err(runtime_error(ErrorCode::Cancelled, "ring closed"));
            }
            if let Some(token) = token {
                token.check()?;
            }
            if queue.len() < self.capacity {
                queue.push_back(value);
                drop(queue);
                self.readable.notify_waiters();
                return Ok(());
            }
            drop(queue);
            if let Some(token) = token {
                let cancelled = token.cancelled();
                let mut cancelled = pin!(cancelled);
                poll_fn(|cx| {
                    if cancelled.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(());
                    }
                    changed.as_mut().poll(cx)
                })
                .await;
            } else {
                changed.await;
            }
        }
    }

    /// Waits without spin-polling. None denotes terminal closure, not emptiness.
    pub async fn recv(&self) -> Option<T> {
        loop {
            let changed = self.readable.notified();
            let mut changed = pin!(changed);
            changed.as_mut().enable();
            let mut queue = self.queue.write().await;
            if self.is_closed() {
                let discarded = core::mem::take(&mut *queue);
                drop(queue);
                drop(discarded);
                return None;
            }
            if let Some(value) = queue.pop_front() {
                drop(queue);
                self.writable.notify_waiters();
                return Some(value);
            }
            drop(queue);
            changed.await;
        }
    }
}

struct QueuedParse<V> {
    sequence: u64,
    submission: submit::Submission<V>,
}

/// Producer/completion endpoint for a single ordered CPU lane. Keep this
/// endpoint alive while consuming results; Drop closes both queues. Accepted
/// jobs retain their lease through successful result ownership. Errors release
/// it once when the worker unwinds its submission; shutdown discards work.
pub struct HostedPool<V> {
    jobs: Arc<ReactiveRing<QueuedParse<V>>>,
    completions: Arc<ReactiveRing<Completion<Admitted<ValidatedSlots>>>>,
    next: RwLock<Option<u64>>,
}

/// Unique worker endpoint. Run on an explicitly chosen nagoya runtime, or await
/// run on a host CPU executor. Dropping the worker closes both endpoints.
pub struct HostedWorker<V> {
    jobs: Arc<ReactiveRing<QueuedParse<V>>>,
    completions: Arc<ReactiveRing<Completion<Admitted<ValidatedSlots>>>>,
}

impl<V: ParseValidator> HostedPool<V> {
    pub fn new(
        job_capacity: usize,
        completion_capacity: usize,
        first_sequence: u64,
    ) -> Result<(Self, HostedWorker<V>)> {
        let jobs = Arc::new(ReactiveRing::new(job_capacity)?);
        let completions = Arc::new(ReactiveRing::new(completion_capacity)?);
        let worker = HostedWorker {
            jobs: Arc::clone(&jobs),
            completions: Arc::clone(&completions),
        };
        Ok((
            Self {
                jobs,
                completions,
                next: RwLock::new(Some(first_sequence)),
            },
            worker,
        ))
    }

    /// Moves an owned request, shared compiled-validator handle, cancellation
    /// token and unique admission lease together. No sequence is consumed on
    /// rejection or a dropped submission future. Concurrent producers serialize
    /// sequence assignment and enqueue, so there can be no completion gaps.
    pub async fn submit(
        &self,
        validator: Arc<V>,
        job: ParseJob,
        lease: ScratchLease,
    ) -> Result<u64> {
        let submission = submit::Submission {
            job,
            validator,
            lease,
        };
        submit::preflight(
            &submission.job.payload,
            submission.job.limits,
            submission.lease.lease.request(),
            &submission.job.cancellation,
        )?;
        let token = submission.job.cancellation.clone();
        let acquire = self.next.write();
        let mut acquire = pin!(acquire);
        let cancelled = token.cancelled();
        let mut cancelled = pin!(cancelled);
        let mut next = poll_fn(|cx| {
            if cancelled.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(runtime_error(
                    ErrorCode::Cancelled,
                    "submission cancelled",
                )));
            }
            acquire.as_mut().poll(cx).map(Ok)
        })
        .await?;
        let sequence = (*next).ok_or_else(|| {
            runtime_error(ErrorCode::ConstraintViolation, "hosted sequence exhausted")
        })?;
        self.jobs
            .send_cancellable(
                QueuedParse {
                    sequence,
                    submission,
                },
                &token,
            )
            .await?;
        *next = sequence.checked_add(1);
        Ok(sequence)
    }

    /// Every accepted sequence produces one FIFO success/error, unless the lane
    /// is explicitly closed or either endpoint/worker is dropped.
    pub async fn recv(&self) -> Option<Completion<Admitted<ValidatedSlots>>> {
        self.completions.recv().await
    }

    /// Terminal shutdown wakes backpressured workers/producers. Queued owners
    /// are discarded by the worker/receiver or when the last endpoint drops.
    pub fn close(&self) {
        self.jobs.close();
        self.completions.close();
    }
}

impl<V> Drop for HostedPool<V> {
    fn drop(&mut self) {
        self.jobs.close();
        self.completions.close();
    }
}

impl<V: ParseValidator> HostedWorker<V> {
    pub async fn run(self) {
        while let Some(queued) = self.jobs.recv().await {
            let token = queued.submission.job.cancellation.clone();
            let result = queued.submission.run(Ok);
            let retained_bytes = result
                .as_ref()
                .map_or(0, |value| value.reservation().retained_bytes);
            let succeeded = result.is_ok();
            let completion = Completion {
                sequence: queued.sequence,
                retained_bytes,
                result,
            };
            if succeeded {
                // A cancelled result blocked on completion capacity releases its
                // owner/lease now, but still occupies its original sequence.
                if let Err(error) = self.completions.send_cancellable(completion, &token).await {
                    if self.completions.is_closed() {
                        break;
                    }
                    let cancelled = Completion {
                        sequence: queued.sequence,
                        retained_bytes: 0,
                        result: Err(error),
                    };
                    if self.completions.send(cancelled).await.is_err() {
                        break;
                    }
                }
            } else if self.completions.send(completion).await.is_err() {
                break;
            }
            // Yield between CPU jobs; idle and capacity waits use notifications.
            nagoya::yield_now().await;
        }
    }

    /// Explicit runtime binding; this never starts a hidden global runtime.
    pub fn spawn_on(self, runtime: &nagoya::runtime::Runtime) -> nagoya::JoinHandle<()> {
        runtime.spawn(self.run())
    }
}

impl<V> Drop for HostedWorker<V> {
    fn drop(&mut self) {
        self.jobs.close();
        self.completions.close();
    }
}
