//! Owned, bounded batch submission through the host's CPU executor.
//!
//! Input weights are caller-declared accounting, including nested allocations.
//! They neither measure allocator usage nor bound the size of completed outputs.
//! Use admitted outputs or an independently budgeted completion queue when
//! results retain significant storage.

use super::*;

/// Schedule one independent CPU job for each owned input.
///
/// Invalid options, excessive input count or retained weight, pre-existing
/// cancellation, and task-vector reservation failure reject the entire batch
/// before submission. Once launching starts, executor rejections and observed
/// cancellation become per-item errors, preserving every original position.
///
/// Work must cooperate with cancellation during long operations. Each job also
/// checks the shared token before work and before publishing a successful value.
/// Work errors and executor errors retain their original diagnostic context.
/// Dropping returned tasks delegates eventual job/output cleanup to the host.
pub fn par_for<E, I, O, F>(
    executor: &E,
    items: Vec<OwnedBatchItem<I>>,
    options: BatchOptions,
    cancellation: CancellationToken,
    work: Arc<F>,
) -> Result<Vec<BatchTask<E::Task<O>>>>
where
    E: CpuExecutor,
    I: Send + 'static,
    O: Send + 'static,
    F: Fn(I, &CancellationToken) -> Result<O> + Send + Sync + 'static,
{
    if options.max_items == 0 {
        return Err(runtime_error(
            ErrorCode::ConstraintViolation,
            "batch item limit must be nonzero",
        ));
    }
    if items.len() > options.max_items {
        return Err(runtime_error(ErrorCode::BudgetExhausted, "batch item limit exceeded"));
    }
    let mut retained_bytes = 0usize;
    for item in &items {
        retained_bytes = retained_bytes.checked_add(item.retained_bytes).ok_or_else(|| {
            runtime_error(ErrorCode::BudgetExhausted, "batch retained-byte sum overflow")
        })?;
        if retained_bytes > options.max_retained_bytes {
            return Err(runtime_error(
                ErrorCode::BudgetExhausted,
                "batch retained-byte limit exceeded",
            ));
        }
    }
    cancellation.check()?;
    let mut tasks = reserve_tasks::<E::Task<O>>(items.len())?;

    for (index, item) in items.into_iter().enumerate() {
        let task = match cancellation.check() {
            Err(error) => {
                // Release this unsubmitted input now, without discarding any
                // previously accepted tasks or later positional error entries.
                drop(item);
                Err(error)
            }
            Ok(()) => {
                let token = cancellation.clone();
                let work = Arc::clone(&work);
                let input = item.value;
                executor.submit(Box::new(move || {
                    token.check()?;
                    let output = work(input, &token)?;
                    // Captured owners can request cancellation during cleanup.
                    // Observe that cleanup before the final publication check.
                    drop(work);
                    token.check()?;
                    Ok(output)
                }))
            }
        };
        tasks.push(BatchTask { index, task });
    }
    Ok(tasks)
}

fn reserve_tasks<F>(count: usize) -> Result<Vec<BatchTask<F>>> {
    let mut tasks = Vec::new();
    tasks.try_reserve_exact(count).map_err(|_| {
        runtime_error(ErrorCode::BudgetExhausted, "batch task storage reservation failed")
    })?;
    Ok(tasks)
}

/// Await every accepted task and preserve all successes and failures in order.
///
/// Awaiting handles in vector order does not serialize CPU execution: the host
/// has already scheduled all accepted jobs independently. Supplied indices are
/// retained verbatim, including duplicates or unordered indices in manually
/// constructed vectors. No error implicitly cancels other tasks.
///
/// This non-fallible collection API uses the allocator's normal allocation
/// failure behavior for result storage. It accepts non-`Unpin` futures and does
/// not impose `Send` or `'static` requirements beyond the future's own bounds.
pub async fn collect_batch<F, T>(tasks: Vec<BatchTask<F>>) -> BatchResults<T>
where
    F: Future<Output = Result<T>>,
{
    let mut items = Vec::with_capacity(tasks.len());
    for BatchTask { index, task } in tasks {
        let result = match task {
            Ok(task) => task.await,
            Err(error) => Err(error),
        };
        items.push(BatchItem { index, result });
    }
    BatchResults { items }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::{rc::Rc, task::Wake, vec};
    use core::{
        cell::Cell,
        future::{pending, ready, Ready},
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    // CPU work runs only when the test driver explicitly requests it. Polling
    // never executes CPU work, and taking the closure enforces at-most-once use.
    struct ManualTask<T> {
        job: Option<Box<dyn FnOnce() -> Result<T> + Send + 'static>>,
        output: Option<Result<T>>,
    }

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
        reject_at: Vec<usize>,
        cancel_at: Option<(usize, CancellationToken)>,
    }

    impl CpuExecutor for ManualExecutor {
        type Task<T> = ManualTask<T> where T: Send + 'static;

        fn submit<T: Send + 'static>(
            &self,
            job: Box<dyn FnOnce() -> Result<T> + Send + 'static>,
        ) -> Result<Self::Task<T>> {
            let position = self.submissions.fetch_add(1, Ordering::Relaxed);
            if let Some((at, token)) = &self.cancel_at {
                if position == *at {
                    token.cancel();
                }
            }
            if self.reject_at.contains(&position) {
                drop(job);
                return Err(rejection_error());
            }
            Ok(ManualTask { job: Some(job), output: None })
        }
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = Waker::from(Arc::new(WakeCount::default()));
        future.poll(&mut Context::from_waker(&waker))
    }

    fn ready_output<F: Future>(future: F) -> F::Output {
        let mut future = core::pin::pin!(future);
        match poll_once(future.as_mut()) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test expected an already completed future"),
        }
    }

    fn run_all<T>(tasks: &mut [BatchTask<ManualTask<T>>]) {
        for entry in tasks {
            if let Ok(task) = &mut entry.task {
                assert!(task.run_cpu());
                assert!(!task.run_cpu());
            }
        }
    }

    fn options(max_items: usize, max_retained_bytes: usize) -> BatchOptions {
        BatchOptions { max_items, max_retained_bytes }
    }

    fn inputs(weights: &[usize]) -> Vec<OwnedBatchItem<usize>> {
        weights.iter().enumerate().map(|(value, &retained_bytes)| {
            OwnedBatchItem { value, retained_bytes }
        }).collect()
    }

    fn identity(value: usize, _: &CancellationToken) -> Result<usize> {
        Ok(value)
    }

    fn rejection_error() -> Error {
        Error::new(ErrorCode::BudgetExhausted, ErrorSource::Transport, 31,
            "host CPU queue rejected job").with_schema(7, 11)
    }

    fn work_error() -> Error {
        Error::new(ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 23,
            "work rejected input").with_schema(13, 17)
    }

    fn error_of<T>(result: Result<T>) -> Error {
        match result {
            Err(error) => error,
            Ok(_) => panic!("test expected an error"),
        }
    }

    fn assert_runtime_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    #[derive(Debug)]
    struct Tracked(Arc<AtomicUsize>);

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn whole_batch_preflight_rejects_limits_and_overflow_before_submission() {
        let cases = [
            (vec![], options(0, 0), ErrorCode::ConstraintViolation),
            (vec![0], options(0, 0), ErrorCode::ConstraintViolation),
            (vec![0, 0], options(1, 0), ErrorCode::BudgetExhausted),
            (vec![0, 1], options(2, 0), ErrorCode::BudgetExhausted),
            (vec![2, 3, 4], options(3, 8), ErrorCode::BudgetExhausted),
            (vec![usize::MAX, 1], options(2, usize::MAX), ErrorCode::BudgetExhausted),
            (vec![usize::MAX - 1, 1, 1], options(3, usize::MAX), ErrorCode::BudgetExhausted),
        ];
        for (weights, limits, expected) in cases {
            let executor = ManualExecutor::default();
            let work = Arc::new(identity);
            let weak = Arc::downgrade(&work);
            let result = par_for(&executor, inputs(&weights), limits,
                CancellationToken::new(), work);
            assert_runtime_error(error_of(result), expected);
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn empty_zero_weight_and_exact_limits_are_accepted() {
        let cases = [
            (vec![], options(1, 0)),
            (vec![0], options(1, 0)),
            (vec![0, 0, 0], options(3, 0)),
            (vec![2, 3, 4], options(3, 9)),
            (vec![usize::MAX], options(1, usize::MAX)),
            (vec![usize::MAX - 1, 1, 0], options(3, usize::MAX)),
        ];
        for (weights, limits) in cases {
            let executor = ManualExecutor::default();
            let mut tasks = par_for(&executor, inputs(&weights), limits,
                CancellationToken::new(), Arc::new(identity)).unwrap();
            assert_eq!(tasks.len(), weights.len());
            assert_eq!(executor.submissions.load(Ordering::Relaxed), weights.len());
            run_all(&mut tasks);
            let results = ready_output(collect_batch(tasks));
            assert_eq!(results.items.len(), weights.len());
            for (expected, item) in results.items.into_iter().enumerate() {
                assert_eq!(item.index, expected);
                assert_eq!(item.result, Ok(expected));
            }
        }
    }

    #[test]
    fn pre_cancelled_batches_including_empty_never_submit() {
        for weights in [vec![], vec![0, 0]] {
            let executor = ManualExecutor::default();
            let token = CancellationToken::new();
            token.cancel();
            let result = par_for(&executor, inputs(&weights), options(2, 0),
                token, Arc::new(identity));
            assert_runtime_error(error_of(result), ErrorCode::Cancelled);
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn failed_preflight_drops_every_owned_input_exactly_once() {
        for cancelled in [false, true] {
            let executor = ManualExecutor::default();
            let drops = Arc::new(AtomicUsize::new(0));
            let items = (0..3).map(|_| OwnedBatchItem {
                value: Tracked(drops.clone()), retained_bytes: 1,
            }).collect();
            let token = CancellationToken::new();
            if cancelled { token.cancel(); }
            let result = par_for(&executor, items, options(3, if cancelled { 3 } else { 2 }),
                token, Arc::new(|value: Tracked, _: &CancellationToken| Ok(value)));
            assert_runtime_error(error_of(result), if cancelled {
                ErrorCode::Cancelled
            } else {
                ErrorCode::BudgetExhausted
            });
            assert_eq!(drops.load(Ordering::Relaxed), 3);
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn task_storage_capacity_overflow_is_a_recoverable_budget_error() {
        assert!(reserve_tasks::<Ready<Result<()>>>(0).unwrap().is_empty());
        let tasks = reserve_tasks::<Ready<Result<()>>>(3).unwrap();
        assert!(tasks.is_empty());
        assert!(tasks.capacity() >= 3);
        // Nonzero-sized entries make this a deterministic capacity overflow,
        // without attempting an actual enormous allocation or exhausting memory.
        assert_runtime_error(error_of(reserve_tasks::<Ready<Result<()>>>(usize::MAX)),
            ErrorCode::BudgetExhausted);
    }

    #[test]
    fn every_job_is_submitted_before_work_and_can_finish_out_of_order() {
        let executor = ManualExecutor::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let work = Arc::new(move |value: usize, _: &CancellationToken| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(value * value + 1)
        });
        let mut tasks = par_for(&executor, inputs(&[1, 2, 3, 4]), options(4, 10),
            CancellationToken::new(), work).unwrap();
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 4);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        for entry in &mut tasks {
            assert!(poll_once(Pin::new(entry.task.as_mut().unwrap())).is_pending());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        for index in [3, 1, 2, 0] {
            let task = tasks[index].task.as_mut().unwrap();
            assert!(task.run_cpu());
            assert!(!task.run_cpu());
        }
        let results = ready_output(collect_batch(tasks));
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        for (index, (item, expected)) in results.items.into_iter().zip([1, 2, 5, 10]).enumerate() {
            assert_eq!(item.index, index);
            assert_eq!(item.result, Ok(expected));
        }
    }

    #[test]
    fn rejections_and_work_errors_preserve_all_positions_and_error_context() {
        let executor = ManualExecutor { reject_at: vec![0, 2, 5], ..Default::default() };
        let token = CancellationToken::new();
        let mut tasks = par_for(&executor, inputs(&[0; 6]), options(6, 0), token.clone(),
            Arc::new(|value: usize, _: &CancellationToken| {
                if value == 3 { Err(work_error()) } else { Ok(value + 10) }
            })).unwrap();
        assert_eq!(tasks.len(), 6);
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 6);
        run_all(&mut tasks);
        let results = ready_output(collect_batch(tasks));
        assert!(!token.is_cancelled());
        assert_eq!(results.items.len(), 6);
        for (index, item) in results.items.into_iter().enumerate() {
            assert_eq!(item.index, index);
            let expected = match index {
                0 | 2 | 5 => Err(rejection_error()),
                3 => Err(work_error()),
                _ => Ok(index + 10),
            };
            assert_eq!(item.result, expected);
        }
    }

    #[test]
    fn all_rejections_release_inputs_and_shared_work() {
        let executor = ManualExecutor { reject_at: vec![0, 1, 2], ..Default::default() };
        let drops = Arc::new(AtomicUsize::new(0));
        let work = Arc::new(|value: Tracked, _: &CancellationToken| Ok(value));
        let weak = Arc::downgrade(&work);
        let items = (0..3).map(|_| OwnedBatchItem {
            value: Tracked(drops.clone()), retained_bytes: 0,
        }).collect();
        let tasks = par_for(&executor, items, options(3, 0),
            CancellationToken::new(), work).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 3);
        assert!(weak.upgrade().is_none());
        let results = ready_output(collect_batch(tasks));
        for item in results.items {
            assert_eq!(error_of(item.result), rejection_error());
        }
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn mid_launch_cancellation_keeps_accepted_tasks_and_all_later_positions() {
        for reject_trigger in [false, true] {
            let token = CancellationToken::new();
            let executor = ManualExecutor {
                reject_at: if reject_trigger { vec![1] } else { vec![] },
                cancel_at: Some((1, token.clone())),
                ..Default::default()
            };
            let mut tasks = par_for(&executor, inputs(&[0; 5]), options(5, 0), token,
                Arc::new(identity)).unwrap();
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 2);
            assert_eq!(tasks.len(), 5);
            assert!(tasks[0].task.is_ok());
            assert_eq!(tasks[1].task.is_err(), reject_trigger);
            for entry in &tasks[2..] {
                assert_eq!(entry.task.as_ref().err().unwrap().code, ErrorCode::Cancelled);
            }
            run_all(&mut tasks);
            for (index, item) in ready_output(collect_batch(tasks)).items.into_iter().enumerate() {
                assert_eq!(item.index, index);
                if index == 1 && reject_trigger {
                    assert_eq!(error_of(item.result), rejection_error());
                } else {
                    assert_runtime_error(error_of(item.result), ErrorCode::Cancelled);
                }
            }
        }
    }

    #[test]
    fn queued_cancellation_skips_work_and_drops_inputs() {
        let executor = ManualExecutor::default();
        let token = CancellationToken::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let items = vec![OwnedBatchItem { value: Tracked(drops.clone()), retained_bytes: 0 }];
        let mut tasks = par_for(&executor, items, options(1, 0), token.clone(),
            Arc::new(|_: Tracked, _: &CancellationToken| -> Result<()> {
                panic!("cancelled work must not execute")
            })).unwrap();
        token.cancel();
        run_all(&mut tasks);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let item = ready_output(collect_batch(tasks)).items.pop().unwrap();
        assert_runtime_error(error_of(item.result), ErrorCode::Cancelled);
    }

    #[test]
    fn late_cancellation_drops_success_but_preserves_work_errors() {
        for fail in [false, true] {
            let executor = ManualExecutor::default();
            let token = CancellationToken::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let output_drops = drops.clone();
            let work = Arc::new(move |_: usize, token: &CancellationToken| {
                let value = Tracked(output_drops.clone());
                token.cancel();
                if fail { Err(work_error()) } else { Ok(value) }
            });
            let mut tasks = par_for(&executor, inputs(&[0]), options(1, 0), token.clone(),
                work).unwrap();
            run_all(&mut tasks);
            assert!(token.is_cancelled());
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            let item = ready_output(collect_batch(tasks)).items.pop().unwrap();
            if fail {
                assert_eq!(error_of(item.result), work_error());
            } else {
                assert_runtime_error(error_of(item.result), ErrorCode::Cancelled);
            }
        }
    }

    #[test]
    fn cancellation_during_work_owner_cleanup_is_observed() {
        struct CancelOnDrop(CancellationToken);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) { self.0.cancel(); }
        }
        impl CancelOnDrop {
            fn use_owner(&self) { assert!(!self.0.is_cancelled()); }
        }
        let token = CancellationToken::new();
        let owner = CancelOnDrop(token.clone());
        let work = Arc::new(move |value: usize, _: &CancellationToken| {
            owner.use_owner();
            Ok(value)
        });
        let mut tasks = par_for(&ManualExecutor::default(), inputs(&[0]), options(1, 0),
            token.clone(), work).unwrap();
        run_all(&mut tasks);
        assert!(token.is_cancelled());
        let item = ready_output(collect_batch(tasks)).items.pop().unwrap();
        assert_runtime_error(error_of(item.result), ErrorCode::Cancelled);
    }

    #[test]
    fn already_published_success_survives_later_shared_cancellation() {
        let executor = ManualExecutor::default();
        let token = CancellationToken::new();
        let mut tasks = par_for(&executor, inputs(&[0, 0]), options(2, 0), token.clone(),
            Arc::new(identity)).unwrap();
        assert!(tasks[0].task.as_mut().unwrap().run_cpu());
        token.cancel();
        assert!(tasks[1].task.as_mut().unwrap().run_cpu());
        let mut results = ready_output(collect_batch(tasks)).items.into_iter();
        assert_eq!(results.next().unwrap().result, Ok(0));
        assert_runtime_error(error_of(results.next().unwrap().result), ErrorCode::Cancelled);
        assert!(results.next().is_none());
    }

    #[test]
    fn dropping_queued_or_completed_tasks_releases_owned_values() {
        for complete in [false, true] {
            let executor = ManualExecutor::default();
            let drops = Arc::new(AtomicUsize::new(0));
            let items = (0..3).map(|_| OwnedBatchItem {
                value: Tracked(drops.clone()), retained_bytes: 0,
            }).collect();
            let work = Arc::new(|value: Tracked, _: &CancellationToken| Ok(value));
            let weak = Arc::downgrade(&work);
            let mut tasks = par_for(&executor, items, options(3, 0),
                CancellationToken::new(), work).unwrap();
            if complete { run_all(&mut tasks); }
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            drop(tasks);
            assert_eq!(drops.load(Ordering::Relaxed), 3);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn collection_transfers_nonclone_outputs_without_early_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let executor = ManualExecutor::default();
        let items = vec![OwnedBatchItem { value: Tracked(drops.clone()), retained_bytes: 0 }];
        let mut tasks = par_for(&executor, items, options(1, 0), CancellationToken::new(),
            Arc::new(|value: Tracked, _: &CancellationToken| Ok(value))).unwrap();
        run_all(&mut tasks);
        let results = ready_output(collect_batch(tasks));
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(results.items.len(), 1);
        assert!(results.items[0].result.is_ok());
        drop(results);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn manually_supplied_indices_and_rejections_are_preserved_verbatim() {
        let tasks = vec![
            BatchTask { index: usize::MAX, task: Ok(ready(Ok(4))) },
            BatchTask { index: 3, task: Err(rejection_error()) },
            BatchTask { index: 3, task: Ok(ready(Err(work_error()))) },
            BatchTask { index: 0, task: Ok(ready(Ok(9))) },
        ];
        let results = ready_output(collect_batch(tasks));
        assert_eq!(results.items.len(), 4);
        for (item, (index, expected)) in results.items.into_iter().zip([
            (usize::MAX, Ok(4)), (3, Err(rejection_error())),
            (3, Err(work_error())), (0, Ok(9)),
        ]) {
            assert_eq!(item.index, index);
            assert_eq!(item.result, expected);
        }
        let empty: Vec<BatchTask<Ready<Result<()>>>> = Vec::new();
        assert!(ready_output(collect_batch(empty)).items.is_empty());
    }

    // A single cooperative suspension verifies that collection passes through
    // the caller's waker and resumes without a blocking wait or CPU driver.
    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn collection_supports_pending_non_send_borrowed_async_futures() {
        let calls = Rc::new(Cell::new(0));
        let borrowed = &calls;
        let task = async move {
            YieldOnce(false).await;
            borrowed.set(borrowed.get() + 1);
            Ok(borrowed)
        };
        let mut collection = core::pin::pin!(collect_batch(vec![BatchTask {
            index: 41, task: Ok(task),
        }]));
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(collection.as_mut().poll(&mut cx).is_pending());
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        assert_eq!(calls.get(), 0);
        let results = match collection.as_mut().poll(&mut cx) {
            Poll::Ready(results) => results,
            Poll::Pending => panic!("yielded future should now be ready"),
        };
        assert_eq!(calls.get(), 1);
        assert_eq!(results.items[0].index, 41);
        assert!(Rc::ptr_eq(results.items[0].result.as_ref().unwrap(), &calls));
    }

    #[test]
    fn dropping_pending_collection_releases_results_current_and_remaining_tasks() {
        let drops = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..3).map(|index| {
            let value = Tracked(drops.clone());
            BatchTask {
                index,
                task: Ok(async move {
                    if index == 1 { pending::<()>().await; }
                    Ok(value)
                }),
            }
        }).collect();
        let mut collection = Box::pin(collect_batch(tasks));
        assert!(poll_once(collection.as_mut()).is_pending());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(collection);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn owned_submission_and_collection_futures_are_send() {
        fn assert_send_static<T: Send + 'static>(_: &T) {}
        let executor = ManualExecutor::default();
        let tasks = par_for(&executor, inputs(&[0]), options(1, 0),
            CancellationToken::new(), Arc::new(identity)).unwrap();
        assert_send_static(&tasks);
        let collection = collect_batch(tasks);
        assert_send_static(&collection);
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn panicking_work_drops_input_and_capture_without_reexecution() {
        let executor = ManualExecutor::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Tracked(drops.clone());
        let work = Arc::new(move |value: Tracked, _: &CancellationToken| -> Result<()> {
            let captured_owner = &capture;
            assert!(Arc::ptr_eq(&captured_owner.0, &value.0));
            panic!("deliberate work panic")
        });
        let weak = Arc::downgrade(&work);
        let items = vec![OwnedBatchItem { value: Tracked(drops.clone()), retained_bytes: 0 }];
        let mut tasks = par_for(&executor, items, options(1, 0),
            CancellationToken::new(), work).unwrap();
        let task = tasks[0].task.as_mut().unwrap();
        let unwind = self::std::panic::catch_unwind(self::std::panic::AssertUnwindSafe(|| {
            task.run_cpu();
        }));
        assert!(unwind.is_err());
        assert!(!task.run_cpu());
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert!(weak.upgrade().is_none());
        drop(tasks);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}
