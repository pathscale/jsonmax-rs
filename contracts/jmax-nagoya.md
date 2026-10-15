# jmax-nagoya frozen contract, version 1

The root freezes owned job/result types, executor and validator traits,
cancellation, resource leases, and ordered queue storage. Each wave module uses
`super::*`; the exact shared source is appended below. There is no built-in
executor, global pool, blocking wait, or platform worker creation. A host runtime
adapter implements CpuExecutor using its CPU scheduling facility. This trait is
a complete transport-independent boundary, not a fallback that runs CPU work on
an I/O task. All adapters must schedule accepted jobs on their CPU facility.

## Wave 1: submit.rs

```rust
pub fn submit_parse<E, V>(
    executor: &E, validator: Arc<V>, job: ParseJob, lease: ScratchLease,
) -> Result<E::Task<Admitted<ValidatedSlots>>>
where
    E: CpuExecutor,
    V: ParseValidator;
pub fn submit_typed<E, V, T>(
    executor: &E, validator: Arc<V>, job: ParseJob, lease: ScratchLease,
) -> Result<E::Task<Admitted<ParsedMessage<T>>>>>
where
    E: CpuExecutor,
    V: ParseValidator,
    T: JsonDecode + SchemaProvider + Send + 'static;
```

All captures are owned Send + 'static. Validate before submission: nonzero
max_payload_bytes and max_depth; payload must fit Limits.max_payload_bytes and
u32; lease.request.jobs >= 1; lease.request.retained_bytes >= payload.len();
cancellation not set. Invalid configuration is ConstraintViolation, insufficient
lease BudgetExhausted, oversized payload PayloadTooLarge. No executor call occurs
for these errors, and the supplied lease is dropped. Zero decoded budget is
legal when no decoding allocation is required. Validate all other relevant
Limits within the parser rather than changing its configuration silently.

Move payload, limits, cancellation, validator Arc, scratch and reservation into
one boxed job. Executor rejection drops the closure and releases the lease.
Inside the job check cancellation before validation; clear scratch before reuse;
call ParseValidator::validate with the exact scratch and retained-byte budgets
from the lease;
check cancellation after validation and before publication. Validators must
cooperatively check cancellation during long work. CPU cancellation cannot
preempt arbitrary code. Check returned payload bytes exactly equal original,
proof.payload_len equals payload length, proof.decoded_len equals arena length.
Bad proof/ownership is InternalError. Validate the checked lower bound
payload.len + decoded.len + slots.capacity * size_of::<SlotValue>() against
the retained-byte reservation; excess is BudgetExhausted. The validator must
preflight returned arena and slot storage against retained_budget_bytes, in
addition to respecting scratch_budget_bytes during validation. Shared allocation
backing storage can exceed visible slice lengths; callers declare that retained
weight conservatively. submit_typed additionally compares
T::schema_fingerprint against the issued proof, invokes T::decode_slots once,
and transfers the original owners and proof into ParsedMessage<T> using only the
canonical proven-owner constructor. A mismatch is SchemaImageInvalid. Proof is
issued only by actual validation, never fabricated by submission code.

On success, drop scratch and return Admitted with value plus the original
reservation. The reservation remains held while an executor future retains its
completed output and while a consumer holds the Admitted result. On any error,
all task-local owners and the lease are dropped. Scratch reservation may stay
conservatively reserved with a completed result even though scratch has been
freed. Admitted::into_parts transfers both ownership and accounting explicitly;
the caller must keep the lease alive for as long as its reserved value is held.
Dropping a task may cancel it or let it finish, according to the executor; in
either case the executor must eventually drop the closure/output and release
its lease. A job is never executed twice. Panics must not cause double execution;
normal Rust unwinding drops leases, and aborting runtimes cannot promise cleanup.
For typed decoding, the declared retained-byte reservation also covers any
additional allocations owned by T. JsonDecode implementations are trusted to
respect the caller's declared worst-case type budget; the canonical trait does
not expose allocator accounting, so generic submission cannot measure T's deep
size. No hard allocator-wide memory bound is claimed for arbitrary decoders.

## Wave 2: batch.rs

```rust
pub fn par_for<E, I, O, F>(
    executor: &E, items: Vec<OwnedBatchItem<I>>, options: BatchOptions,
    cancellation: CancellationToken, work: Arc<F>,
) -> Result<Vec<BatchTask<E::Task<O>>>>
where
    E: CpuExecutor,
    I: Send + 'static,
    O: Send + 'static,
    F: Fn(I, &CancellationToken) -> Result<O> + Send + Sync + 'static;
pub async fn collect_batch<F, T>(tasks: Vec<BatchTask<F>>) -> BatchResults<T>
where
    F: Future<Output = Result<T>>;
```

par_for preflights all inputs before any submission: max_items must be nonzero;
count must fit it; sum retained_bytes must fit max_retained_bytes without
arithmetic overflow. max_retained_bytes zero admits only zero-byte inputs.
An empty batch is legal. Retained weights are supplied by the owning caller and
must include nested allocations; weights are contractual accounting, not an
allocator measurement. Failure is ConstraintViolation for invalid options,
BudgetExhausted for count/byte/overflow limits, Cancelled for pre-cancelled work.
Reserve the returned vector before launching. If reservation fails, submit no
jobs. Every input gets an original position 0..n in the returned vector.

Each closure owns its input and clones Arc<F>/cancellation. Check cancellation
before work and after successful work, converting a late success to Cancelled.
Executor rejections and mid-launch cancellation occupy their original positions
as BatchTask.task Err; continue representing all positions without losing earlier
tasks. Accepted jobs are independently scheduled; no serial CPU execution loop
is substituted. collect_batch awaits all accepted tasks, preserves original
indices and vector order, and copies rejection errors into results. One failure
does not erase successful results. There is no fail-fast cancellation unless the
caller cancels the shared token. The task vector produced by par_for has unique
ascending indices; externally constructed vectors preserve their supplied order
and indices rather than being silently repaired. BatchResults' ascending-index
invariant applies to par_for output. Completed result sizes are the caller's
responsibility; use Admitted outputs and/or ordered queue budgets when retaining
large results. No task spawns another task implicitly.

## Wave 3: admission.rs

```rust
impl Admission {
    pub fn new(limits: AdmissionLimits) -> Result<Self>;
    pub fn limits(&self) -> AdmissionLimits;
    pub fn snapshot(&self) -> AdmissionSnapshot;
    pub fn try_acquire(&self, request: ResourceRequest) -> Result<AdmissionLease>;
    pub fn try_scratch(&self, request: ResourceRequest) -> Result<ScratchLease>;
}
```

max_jobs must be nonzero. Zero byte/scratch capacities are valid and deny
positive requests for that resource. Each acquisition requires jobs > 0;
zero jobs is ConstraintViolation. Reserve jobs, retained bytes, scratch bytes
in that order with checked compare/exchange loops; each counter must never
exceed its configured limit, including under contention. Any failed reservation
rolls back every prior reservation exactly once before returning BudgetExhausted.
Do not use wrapping addition, sleep, busy-wait for availability, or return a
partially admitted lease. Retrying CAS under contention is allowed, but an
insufficient current capacity fails immediately. Limits are immutable.
Snapshots use Acquire loads and are advisory per-counter observations, not an
atomic simultaneous snapshot; temporary reservations during rollback may appear.

try_scratch obtains one lease and creates a fresh empty Scratch. No scratch
allocation is made outside its declared logical budget. Scratch::clear resets
lengths for reuse without claiming freed capacity. Budget accounts logical
capacities of decoded_buf (bytes), stack (len/capacity times size_of FramePhase),
and seen_words (times size_of u64), excluding allocator bookkeeping. A validator
must use fallible, bounded scratch growth and return BudgetExhausted before
exceeding the supplied scratch_budget_bytes. ScratchLease::scratch_mut exposes
scratch to trusted callers, which inherit this rule. Admission cannot intercept
arbitrary caller allocations; declared weights and validator compliance are
part of the trait contract. Per-job decoded limits and payload limits apply in
addition to scratch admission. Leases are Send, not Clone, and Drop releases
scratch/bytes/jobs once using the root implementation. ScratchLease fields are
ordered so scratch drops before the reservation. Admitted fields likewise drop
the result before its lease. No explicit release method or leaked permits.

## Wave 4: ordered.rs

```rust
impl<T> OrderedCompletions<T> {
    pub fn new(options: OrderedOptions) -> Result<Self>;
    pub fn insert(
        &mut self, completion: Completion<T>,
    ) -> core::result::Result<Option<Completion<T>>, InsertError<T>>;
    pub fn pop_ready(&mut self) -> Option<Completion<T>>;
    pub fn next_sequence(&self) -> Option<u64>;
    pub fn pending_len(&self) -> usize;
    pub fn pending_bytes(&self) -> usize;
    pub fn is_cancelled(&self) -> bool;
    pub fn cancel(&mut self);
}
```

max_pending must be nonzero. Zero max_pending_bytes permits only zero-weight
buffered completions. next initially equals first_sequence. No sequence wrap:
after emitting u64::MAX, next is None and further insertions fail with
ConstraintViolation. A sequence below next or duplicate pending sequence is
DuplicateObjectKey. Insertion after cancellation is Cancelled. On any rejected
insertion return InsertError containing the canonical error and the unchanged
owned completion; do not discard its result or reservation.

When completion.sequence equals next, insert returns Some(completion) directly
and advances next with checked_add; it does not buffer it or charge queue bytes.
This direct delivery works even if out-of-order storage is full, avoiding a
head-of-line capacity deadlock. The caller must deliver this completion before
calling pop_ready. A future sequence is retained only if both pending count and
checked sum of retained_bytes fit options; success returns None. Exceeding a
queue limit is BudgetExhausted and leaves the queue unchanged. Weights cover
owned payloads and nested allocations; they are caller-supplied. Queue limits
bound buffered storage only, not a completion being directly returned. Fallible
preallocation should be used where supported; BTreeMap allocation follows the
standard alloc crate's allocation failure behavior (there is no stable fallible
BTreeMap insert). Do not claim recoverable OOM for that operation.

pop_ready removes only the exact next sequence, decrements pending bytes, and
advances next; otherwise returns None. Failed Results are ordinary completions
and preserve order just like successes. cancel is idempotent, marks cancelled,
clears all pending results/reservations, sets pending bytes to zero and next to
None. Later pop_ready returns None. is_cancelled distinguishes cancellation from
sequence exhaustion. No missing sequence is synthesized or skipped automatically.
Callers represent a cancelled individual job with a Cancelled Completion at its
original sequence if they want subsequent sequences delivered. This data
structure has no internal locks; move it to one coordinator or synchronize
externally. CancellationToken and Admission are independently shareable.

## Errors and invariants

Local runtime boundary errors use canonical ErrorSource::Decode, offset zero;
parser errors keep their original source, byte offset, schema and field IDs.
All task/result values are owned. No task captures stack borrows, mutable shared
Scratch, non-Send closures, or I/O task contexts. ResourceBudget is an ABI
reexport for hosts; its cpu_units and timeout_ms are not automatically enforced
by this CPU trait. Hosts control executor scheduling and deadlines. Cancellation
is monotonic; not-yet-published successful parse output is suppressed if cancelled.

Acyclic module structure: submit may use admission public methods, batch may use
submit public functions, admission and ordered use root only. No module imports
another's private state. Allowed dependencies: jmax-abi, jmax-buffer, jmax-syntax,
jmax-schema-vm. Optional runtime manifests do not authorize new runtime API
coupling in the frozen root. All source supports no_std + alloc on targets with
pointer-width atomics. No OS worker creation or unsupported async runtime.

## Exact shared root source

This source freezes all public layouts, trait signatures, root methods, and
private storage available to the wave implementations.

```rust
//! Runtime-neutral, owned CPU jobs with cancellation and admission accounting.
#![no_std]

extern crate alloc;

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{future::Future, sync::atomic::{AtomicBool, AtomicUsize, Ordering}};
pub use jmax_abi::buffer::{ParsedMessage, Payload};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::ValidatedSlots;
pub use jmax_abi::syntax::{Limits, Scratch};
pub use jmax_abi::traits::{JsonDecode, ResourceBudget, SchemaProvider};

pub mod submit;
pub mod batch;
pub mod admission;
pub mod ordered;

pub const CONTRACT_VERSION: u16 = 1;
pub const DEFAULT_BATCH_LIMIT: usize = 1024;
pub const DEFAULT_REORDER_LIMIT: usize = 1024;

/// A clone shares cancellation. Cancellation is monotonic and idempotent.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self { Self::default() }
    pub fn cancel(&self) { self.cancelled.store(true, Ordering::Release); }
    pub fn is_cancelled(&self) -> bool { self.cancelled.load(Ordering::Acquire) }
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(runtime_error(ErrorCode::Cancelled, "CPU operation cancelled"))
        } else {
            Ok(())
        }
    }
}

/// Runtime adapters accept owned Send jobs and return a nonblocking future.
/// Rejection must drop the job. Accepted jobs run at most once.
pub trait CpuExecutor {
    type Task<T>: Future<Output = Result<T>> + Send + 'static
    where T: Send + 'static;

    fn submit<T: Send + 'static>(
        &self,
        job: Box<dyn FnOnce() -> Result<T> + Send + 'static>,
    ) -> Result<Self::Task<T>>;
}

/// The validator owns no task-local borrows and issues canonical validation proof.
pub trait ParseValidator: Send + Sync + 'static {
    fn validate(
        &self,
        payload: Payload,
        limits: Limits,
        scratch: &mut Scratch,
        scratch_budget_bytes: usize,
        retained_budget_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<ValidatedSlots>;
}

#[derive(Clone, Debug)]
pub struct ParseJob {
    pub payload: Payload,
    pub limits: Limits,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchOptions {
    pub max_items: usize,
    pub max_retained_bytes: usize,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self { max_items: DEFAULT_BATCH_LIMIT, max_retained_bytes: 64 * 1024 * 1024 }
    }
}

#[derive(Debug)]
pub struct BatchItem<T> {
    pub index: usize,
    pub result: Result<T>,
}

/// `retained_bytes` is supplied by the owner and includes nested allocations.
#[derive(Debug)]
pub struct OwnedBatchItem<T> {
    pub value: T,
    pub retained_bytes: usize,
}

#[derive(Debug)]
pub struct BatchTask<F> {
    pub index: usize,
    /// An executor rejection occupies its original position in the batch.
    pub task: Result<F>,
}

#[derive(Debug)]
pub struct BatchResults<T> {
    /// One entry per supplied task; par_for inputs retain ascending input order.
    pub items: Vec<BatchItem<T>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceRequest {
    pub jobs: usize,
    pub retained_bytes: usize,
    pub scratch_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionLimits {
    pub max_jobs: usize,
    pub max_retained_bytes: usize,
    pub max_scratch_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub jobs: usize,
    pub retained_bytes: usize,
    pub scratch_bytes: usize,
}

#[derive(Debug)]
pub(crate) struct AdmissionState {
    pub(crate) limits: AdmissionLimits,
    pub(crate) jobs: AtomicUsize,
    pub(crate) retained_bytes: AtomicUsize,
    pub(crate) scratch_bytes: AtomicUsize,
}

#[derive(Clone, Debug)]
pub struct Admission {
    pub(crate) state: Arc<AdmissionState>,
}

/// Not Clone: each successful reservation is released exactly once by Drop.
#[derive(Debug)]
pub struct AdmissionLease {
    pub(crate) state: Arc<AdmissionState>,
    pub(crate) request: ResourceRequest,
}

impl AdmissionLease {
    pub fn request(&self) -> ResourceRequest { self.request }
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.state.scratch_bytes.fetch_sub(self.request.scratch_bytes, Ordering::AcqRel);
        self.state.retained_bytes.fetch_sub(self.request.retained_bytes, Ordering::AcqRel);
        self.state.jobs.fetch_sub(self.request.jobs, Ordering::AcqRel);
    }
}

/// Scratch is job-local. The reservation covers its configured logical budget;
/// validators must honor that budget, including stack and duplicate-key storage.
#[derive(Debug)]
pub struct ScratchLease {
    pub(crate) scratch: Scratch,
    pub(crate) lease: AdmissionLease,
}

impl ScratchLease {
    pub fn budget_bytes(&self) -> usize { self.lease.request.scratch_bytes }
    pub fn scratch(&self) -> &Scratch { &self.scratch }
    pub fn scratch_mut(&mut self) -> &mut Scratch { &mut self.scratch }
}

/// The value is dropped before its accounting lease. Completed jobs therefore
/// retain their reservation until their result is consumed or discarded.
#[derive(Debug)]
pub struct Admitted<T> {
    pub(crate) value: T,
    pub(crate) lease: AdmissionLease,
}

impl<T> Admitted<T> {
    pub fn value(&self) -> &T { &self.value }
    pub fn reservation(&self) -> ResourceRequest { self.lease.request() }
    /// Transfers accounting responsibility along with the owned value.
    pub fn into_parts(self) -> (T, AdmissionLease) { (self.value, self.lease) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderedOptions {
    pub first_sequence: u64,
    pub max_pending: usize,
    pub max_pending_bytes: usize,
}

impl Default for OrderedOptions {
    fn default() -> Self {
        Self { first_sequence: 0, max_pending: DEFAULT_REORDER_LIMIT, max_pending_bytes: 64 * 1024 * 1024 }
    }
}

#[derive(Debug)]
pub struct Completion<T> {
    pub sequence: u64,
    pub retained_bytes: usize,
    pub result: Result<T>,
}

/// Rejected insertion returns the completion to its owner without losing work.
#[derive(Debug)]
pub struct InsertError<T> {
    pub error: Error,
    pub completion: Completion<T>,
}

#[derive(Debug)]
pub struct OrderedCompletions<T> {
    pub(crate) options: OrderedOptions,
    pub(crate) next: Option<u64>,
    pub(crate) pending_bytes: usize,
    pub(crate) pending: BTreeMap<u64, Completion<T>>,
    pub(crate) cancelled: bool,
}

pub(crate) const fn runtime_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}
```
