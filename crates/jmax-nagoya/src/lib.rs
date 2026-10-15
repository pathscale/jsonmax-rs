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
