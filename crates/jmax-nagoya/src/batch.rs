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
