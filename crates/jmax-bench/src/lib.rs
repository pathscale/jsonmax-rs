//! Reproducible workload and measurement contracts. Timings are integer nanoseconds.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::Payload,
    error::{Error, ErrorCode, ErrorSource, Result},
    schema::{SchemaImage, Selection},
    syntax::Limits,
};

pub mod corpus;
pub mod parse;
pub mod serialize;
pub mod service;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_SEED: u64 = 0x4a4d_4158_4245_4e43;
pub const DEFAULT_MAX_CORPUS_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MEASURED_ITERATIONS: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadKind { SmallObject, EscapedStrings, Unicode, NumberArray, DepthBook, AggTrade, Nested }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorpusConfig {
    pub seed: u64,
    pub documents: u32,
    pub items_per_document: u32,
    pub max_bytes: u64,
}
impl Default for CorpusConfig {
    fn default() -> Self {
        Self { seed: DEFAULT_SEED, documents: 128, items_per_document: 32, max_bytes: DEFAULT_MAX_CORPUS_BYTES }
    }
}
#[derive(Clone, Debug)]
pub struct Workload {
    pub name: String,
    pub kind: WorkloadKind,
    pub payloads: Vec<Payload>,
    pub schema: Option<SchemaImage>,
    pub selection: Selection,
}
impl Workload {
    pub fn total_bytes(&self) -> Result<u64> {
        self.payloads.iter().try_fold(0u64, |sum, payload| {
            let len = u64::try_from(payload.len()).map_err(|_| bench_error(ErrorCode::PayloadTooLarge, "payload length exceeds u64"))?;
            sum.checked_add(len).ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "corpus byte count overflow"))
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchmarkMode { Parse, Validate, Extract, Serialize, Escape, Format, Service }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BenchmarkConfig {
    pub warmup_iterations: u32,
    pub measured_iterations: u32,
    pub max_total_input_bytes: u64,
}
impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self { warmup_iterations: 3, measured_iterations: 20, max_total_input_bytes: 4 * 1024 * 1024 * 1024 }
    }
}
impl BenchmarkConfig {
    pub fn validate(&self) -> Result<()> {
        if self.measured_iterations == 0 || self.measured_iterations > MAX_MEASURED_ITERATIONS
            || self.warmup_iterations > MAX_MEASURED_ITERATIONS || self.max_total_input_bytes == 0 {
            return Err(bench_error(ErrorCode::ConstraintViolation, "invalid benchmark iteration or byte budget"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub digest: u64,
    pub output_bytes: u64,
    /// Peak live input, result, and reusable scratch bytes for this invocation.
    pub retained_bytes: u64,
}
/// Each invocation performs exactly the named mode on the complete input.
/// The returned digest depends on the result, preventing dead result elimination.
pub trait BenchmarkOperation {
    fn name(&self) -> &str;
    fn mode(&self) -> BenchmarkMode;
    fn execute(&mut self, input: &Payload) -> Result<Observation>;
}
/// Monotonic time from an arbitrary origin, expressed in nanoseconds.
pub trait Clock { fn now_ns(&mut self) -> u64; }

#[cfg(feature = "std")]
pub struct SystemClock { origin: std::time::Instant }
#[cfg(feature = "std")]
impl SystemClock { pub fn new() -> Self { Self { origin: std::time::Instant::now() } } }
#[cfg(feature = "std")]
impl Default for SystemClock { fn default() -> Self { Self::new() } }
#[cfg(feature = "std")]
impl Clock for SystemClock {
    fn now_ns(&mut self) -> u64 { self.origin.elapsed().as_nanos().min(u64::MAX as u128) as u64 }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub elapsed_ns: u64,
    pub documents: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub peak_retained_bytes: u64,
    pub digest: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchmarkReport {
    pub operation: String,
    pub workload: String,
    pub mode: BenchmarkMode,
    pub samples: Vec<Sample>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub elapsed_ns: u128,
    pub documents: u128,
    pub input_bytes: u128,
    pub output_bytes: u128,
    pub min_ns: u64,
    pub median_ns: u64,
    pub p95_ns: u64,
    pub max_ns: u64,
    pub peak_retained_bytes: u64,
    pub digest: u64,
}
impl BenchmarkReport {
    /// Percentiles use the nearest rank rule over complete corpus passes.
    pub fn summarize(&self) -> Summary {
        if self.samples.is_empty() { return Summary::default(); }
        let mut times: Vec<u64> = self.samples.iter().map(|sample| sample.elapsed_ns).collect();
        times.sort_unstable();
        let mut result = Summary {
            min_ns: times[0], median_ns: times[(times.len() - 1) / 2],
            p95_ns: times[times.len() - 1 - times.len() / 20],
            max_ns: times[times.len() - 1], ..Summary::default()
        };
        for sample in &self.samples {
            result.elapsed_ns += u128::from(sample.elapsed_ns);
            result.documents += u128::from(sample.documents);
            result.input_bytes += u128::from(sample.input_bytes);
            result.output_bytes += u128::from(sample.output_bytes);
            result.peak_retained_bytes = result.peak_retained_bytes.max(sample.peak_retained_bytes);
            result.digest = mix_digest(result.digest, sample.digest);
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceConfig {
    pub max_in_flight: u32,
    pub max_retained_bytes: u64,
    pub requests: u32,
    pub timeout_ns: u64,
}
impl Default for ServiceConfig {
    fn default() -> Self {
        Self { max_in_flight: 32, max_retained_bytes: DEFAULT_MAX_CORPUS_BYTES, requests: 10_000, timeout_ns: 30_000_000_000 }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    pub id: u64,
    pub observation: Observation,
}
/// An adapter owns scheduling, admission credits, and completion queues.
/// Ok(false) means temporary backpressure, and must not retain the input.
pub trait ServiceDriver {
    fn submit(&mut self, id: u64, input: Payload) -> Result<bool>;
    fn poll_completion(&mut self) -> Result<Option<Completion>>;
    fn retained_bytes(&self) -> u64;
    fn cancel_all(&mut self) -> Result<()>;
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceReport {
    pub submitted: u64,
    pub completed: u64,
    pub elapsed_ns: u64,
    pub latencies_ns: Vec<u64>,
    pub peak_retained_bytes: u64,
    pub final_retained_bytes: u64,
    pub digest: u64,
}

/// SplitMix64, frozen so every target produces identical workload bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeterministicRng { state: u64 }
impl DeterministicRng {
    pub const fn new(seed: u64) -> Self { Self { state: seed } }
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}
pub const fn mix_digest(state: u64, value: u64) -> u64 {
    state.rotate_left(7) ^ value.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}
pub fn digest_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3))
}
pub fn bench_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}

/// Shared measurement loop. Workload construction and report allocation are untimed.
pub fn measure(
    workload: &Workload,
    operation: &mut dyn BenchmarkOperation,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    if workload.payloads.is_empty() {
        return Err(bench_error(ErrorCode::ConstraintViolation, "empty benchmark corpus"));
    }
    let bytes = workload.total_bytes()?;
    let passes = u64::from(config.warmup_iterations) + u64::from(config.measured_iterations);
    if bytes.checked_mul(passes).filter(|total| *total <= config.max_total_input_bytes).is_none() {
        return Err(bench_error(ErrorCode::BudgetExhausted, "benchmark input budget exceeded"));
    }
    let operation_name: String = operation.name().into();
    let operation_mode = operation.mode();
    let mut samples = Vec::new();
    samples.try_reserve_exact(config.measured_iterations as usize)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "sample allocation failed"))?;
    for _ in 0..config.warmup_iterations {
        for input in &workload.payloads {
            core::hint::black_box(operation.execute(core::hint::black_box(input))?);
        }
    }
    let mut previous_end = None;
    for _ in 0..config.measured_iterations {
        let mut sample = Sample { input_bytes: bytes, documents: workload.payloads.len() as u64, ..Sample::default() };
        let start = clock.now_ns();
        if previous_end.is_some_and(|end| start < end) {
            return Err(bench_error(ErrorCode::InternalError, "clock moved backwards"));
        }
        for input in &workload.payloads {
            let observation = core::hint::black_box(operation.execute(core::hint::black_box(input))?);
            sample.output_bytes = sample.output_bytes.checked_add(observation.output_bytes)
                .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "output byte count overflow"))?;
            sample.peak_retained_bytes = sample.peak_retained_bytes.max(observation.retained_bytes);
            sample.digest = mix_digest(sample.digest, observation.digest);
        }
        let end = clock.now_ns();
        sample.elapsed_ns = end.checked_sub(start)
            .ok_or_else(|| bench_error(ErrorCode::InternalError, "clock moved backwards"))?;
        previous_end = Some(end);
        samples.push(sample);
    }
    Ok(BenchmarkReport { operation: operation_name, workload: workload.name.clone(), mode: operation_mode, samples })
}
