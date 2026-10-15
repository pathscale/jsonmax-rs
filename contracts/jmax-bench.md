# jmax-bench contract, version 1

Owner: W0-ROOTS-8, row 37. The root API below is frozen. Each assigned module imports shared definitions with `super::*` and owns its implementation. Only the listed manifest dependencies are permitted. ABI payloads, schema images, selections and errors are canonical. The library supports `std` and `alloc` without `std`; `SystemClock` is available only with `std`.

All benchmark runners perform real work and consume result-dependent digests. They never substitute input length for parse/validation correctness, publish invented timings, or silently skip unsupported modes. Clocks are injectable for deterministic callers. An outer `Result::Err` means the benchmark did not produce a complete usable report. Reports and corpora own their storage.

## Wave 1: corpus.rs

Exact public functions:

```rust
pub fn make_workload(kind: WorkloadKind, config: CorpusConfig) -> Result<Workload>;
pub fn make_suite(config: CorpusConfig) -> Result<Vec<Workload>>;
pub fn validate_workload(workload: &Workload, max_bytes: u64) -> Result<()>;
```

`make_workload` uses only `DeterministicRng::new(config.seed)` and deterministic integer formatting. Identical configuration produces identical names, bytes, schema and selection on all supported architectures. No wall clock, OS random source, hash-map iteration order or target SIMD width affects generation. `make_suite` uses the same supplied config independently for kinds in this fixed order: SmallObject, EscapedStrings, Unicode, NumberArray, DepthBook, AggTrade, Nested. Its byte cap covers the aggregate suite, not seven separate budgets.

Require positive documents, items_per_document and max_bytes, document count at most 1,000,000, items per document at most 1,000,000, and per-payload ABI length at most u32::MAX. Reject impossible minimum output sizes before allocation. Respect actual accumulated bytes while building, including JSON punctuation and escaping. Use checked size arithmetic and fallible reservations. Generation never truncates a JSON value to meet a limit.

Workloads are complete strict JSON values. SmallObject contains repeated named scalar fields; EscapedStrings includes quote, reverse solidus, ASCII controls and surrogate-pair escapes; Unicode contains multibyte BMP and non-BMP characters; NumberArray covers signed/unsigned bounds and finite exponent/decimal values; DepthBook and AggTrade use the respective exchange profiles; Nested uses alternating arrays/objects and caps nesting at 64. All names are fixed ASCII identifiers derived from kind and configuration. Each payload varies deterministically across the requested document count. Schema is present when the full generated root shape is representable by the supported schema IR; otherwise None is explicit. Do not attach a misleading partial schema to claim full validation. Selection field IDs are unique, in stable order, and refer to the attached schema. Factories set `strict_validate_unselected = true`.

`validate_workload` checks nonempty name/corpus, each complete JSON payload under strict default limits, byte budget, valid schema images when present, and selection field membership and uniqueness. A selection with no schema is rejected unless empty. Validation must work on caller-constructed workloads. Errors use canonical JSON errors for malformed input, `SchemaImageInvalid` for image/selection corruption, `ConstraintViolation` for invalid configuration, `PayloadTooLarge` for individual ABI overflow, and `BudgetExhausted` for allocation, aggregate bytes or checked arithmetic failure.

## Wave 2: parse.rs

Exact public functions:

```rust
pub fn run_parse(workload: &Workload, config: BenchmarkConfig, clock: &mut dyn Clock) -> Result<BenchmarkReport>;
pub fn run_validate(workload: &Workload, config: BenchmarkConfig, clock: &mut dyn Clock) -> Result<BenchmarkReport>;
pub fn run_extract(workload: &Workload, config: BenchmarkConfig, clock: &mut dyn Clock) -> Result<BenchmarkReport>;
pub fn compare(
    workload: &Workload,
    left: &mut dyn BenchmarkOperation,
    right: &mut dyn BenchmarkOperation,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<(BenchmarkReport, BenchmarkReport)>;
```

Native parse builds a fully validated owned jsonmax document with strict default limits/numeric policy, and derives its digest from parsed content. Validate requires `workload.schema`, executes complete schema validation and hashes accepted semantic slots or a fixed acceptance marker only after actual execution. Extract requires a schema, a nonempty selection, and `strict_validate_unselected == true`, validates all input and hashes selected slot types and resolved values. Missing schema/selection is `ConstraintViolation`. Each runner has the corresponding `BenchmarkMode` and a stable operation name (`jsonmax.parse`, `jsonmax.validate`, `jsonmax.extract`). Use the root `measure` loop; do not duplicate timing policy.

Compilation, corpus generation, workload validation, and reusable scratch preparation happen before timing. Parsing, schema execution, extraction, and ownership costs required by the mode remain inside each invocation. Every operation handles one complete payload and releases per-input results before returning its `Observation`; reusable scratch may survive but must be counted. `output_bytes` is zero for parse/validate; extraction counts resolved selected value bytes using a documented consistent encoding. `retained_bytes` includes input ownership, decoded storage, result storage, and live reusable scratch without double-counting shared backing storage. No result is retained across measured invocations unless explicitly counted and required by the mode.

`compare` accepts only matching Parse, Validate or Extract modes, validates the workload once, and runs left then right with identical iteration counts, payload order and input byte budgets. The clock must remain monotonic across both reports. Both operations must implement the same semantic policy and digest convention; compare warmup results for each input before measuring, including output byte counts. A mismatch is `ConstraintViolation`, not a relative speed score. The comparison returns both reports in argument order, not a claim of statistical significance. Compatibility/reference engines are supplied by the caller; no undeclared dependency is added.

## Wave 3: serialize.rs

Exact public functions:

```rust
pub fn run_serialize(workload: &Workload, config: BenchmarkConfig, clock: &mut dyn Clock) -> Result<BenchmarkReport>;
pub fn run_escape(workload: &Workload, config: BenchmarkConfig, clock: &mut dyn Clock) -> Result<BenchmarkReport>;
pub fn run_format(workload: &Workload, config: BenchmarkConfig, clock: &mut dyn Clock) -> Result<BenchmarkReport>;
pub fn compare(
    workload: &Workload,
    left: &mut dyn BenchmarkOperation,
    right: &mut dyn BenchmarkOperation,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<(BenchmarkReport, BenchmarkReport)>;
```

Names and modes are `jsonmax.serialize`/Serialize, `jsonmax.escape`/Escape, and `jsonmax.format`/Format. Parse input into immutable prepared data before timing. Serialization emits compact complete JSON from that data through a bounded sink; it cannot copy the original raw payload as a serialization benchmark. Escape traverses all prepared string values and member names in source order and emits their quoted JSON representation to the measured sink. Format traverses all numeric values in source order and emits their native numeric representations separated by one ASCII LF; fixed-point decimal formatting must not pass through f64. Missing string or numeric values for the selected microbenchmark is `ConstraintViolation`, not a zero-work success.

Prepared data is indexed consistently with the original payload corpus. Implementations may use private operations and a private common measurement adapter to look up prepared values, but must use root `measure` for iteration/timing policy. Preparation and validation are excluded equally for both sides of a comparison. Output allocation, escaping/formatting work, and digesting emitted bytes are included. Record exact output length and actual retained storage. Use `digest_bytes` over complete emitted bytes. Before measuring, validate native outputs, round-trip serialize output against the prepared value, and verify escape decoding and numeric round trips according to their original types. Reuse a reset bounded sink equally across all measured iterations, or include identical fresh allocation policy for both sides. Empty output is only valid for serialization of a legitimately empty container, which still emits delimiters.

`compare` requires matching Serialize, Escape or Format modes. It applies the same semantic/digest preflight and left-then-right report order as parse::compare. Errors from encoding retain `ErrorSource::Encode`; allocation and configured limits remain distinct. Public runners use `DEFAULT_MAX_OUTPUT_BYTES` from jsonmax for each output. They must report oversized output rather than overflow or truncate.

## Wave 4: service.rs

Exact public functions:

```rust
pub fn validate_service_config(config: ServiceConfig) -> Result<()>;
pub fn run_service(
    workload: &Workload,
    driver: &mut dyn ServiceDriver,
    config: ServiceConfig,
    clock: &mut dyn Clock,
) -> Result<ServiceReport>;
```

`validate_service_config` requires positive max_in_flight, max_retained_bytes, requests and timeout_ns. Requests are capped at 1,000,000, max_in_flight cannot exceed requests, and latency/queue allocations must be checked. `run_service` validates a nonempty strict workload, requires the driver initially retain zero bytes and have no pending completions, and ensures each payload fits the retained-byte budget before submission. Input IDs are exactly 0..requests and payloads cycle in corpus order. Successful `submit` transfers ownership; `Ok(false)` means no ID was admitted and no input was retained. Retrying a rejected submission uses the same ID. The caller must provide an adapter backed by the allowed Nagoya integration for Nagoya measurements; the harness never silently replaces it with synchronous parsing or a separate executor.

Track a bounded map of accepted IDs and their submission timestamps. Poll completions and refill admissions until all requested work is complete. Never exceed max_in_flight. Check `driver.retained_bytes()` before and after every driver operation, track its peak, and fail with `BudgetExhausted` on violation. Admission also accounts for the bytes of the next input; the driver remains responsible for actual backing-allocation/scratch accounting. The retained-byte metric includes driver-owned input, scratch, completions and retained results, but excludes the caller's corpus and harness latency vector. Duplicate, unknown or not-yet-admitted completion IDs are `InternalError`. Timestamps are captured just before the accepted submit attempt and immediately after completion observation. Retries do not include earlier backpressure delay in latency; throughput includes all admission and polling time.

Latency entries are in completion observation order; completion digests are folded in ascending request-ID order so scheduling does not change the report digest. `elapsed_ns` covers the complete submission/completion interval. Backward clock readings are `InternalError`; whole-run timeout is `BudgetExhausted`. Pending zero-progress polls must still check the clock and budget. Driver implementations own cooperative progress and return promptly; the harness never starts OS worker threads or sleeps. A clock supplied by tests must advance during pending work.

On every error after driver interaction, call `cancel_all` and preserve the original failure; cleanup errors never convert failure into success. On success call `cancel_all` to release remaining internal storage, check no completion is left queued, and require `final_retained_bytes == 0`; a leak is `ConstraintViolation`. Driver cancellation must be synchronous with respect to ownership release, idempotent and safe with no submitted work. A successful report has submitted == completed == requests and exactly requests latency entries. Report-only u64 arithmetic is checked.

## Shared measurement and errors

`measure` rejects an empty corpus, invalid iteration counts and byte budgets before warmup; warmup and measured passes both count against max_total_input_bytes. One Sample measures one whole corpus pass, with output byte sums, maximum per-invocation retention, and ordered result digest. Each invocation is guarded by `core::hint::black_box`. The sample vector is reserved before timing. Failed operations abort the report, and failures are never converted into fast successful samples.

Summary elapsed/documents/bytes are exact u128 sums; min, median, p95 and max are nearest-rank statistics over corpus-pass nanoseconds, not per-document latencies. Empty reports summarize to all zeros. Throughput is computed externally from input_bytes and elapsed_ns; zero elapsed time must be reported as clock-resolution limited, not infinite throughput. Report digests combine sample digests with `mix_digest`. `bench_error` uses ABI Decode as the harness boundary; engine errors retain their original source/offset. No statistical assertions belong in these production functions.

## Frozen shared root API

This exact source snapshot defines all shared structs/enums/traits, constants, methods, defaults, digest algorithms, and measurement behavior. Private helper state remains private.

```rust
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
```

## Follow-up: seven workloads and allocation regimes

The CLI uses `followup::make_suite`: NagoyaAuth, ExchangeOrder, RpcEnvelope,
LargeCatalog, VectorCoordinates, DeepNesting, and EscapedUnicode. Legacy
`corpus::make_suite` remains available. VectorCoordinates captures its coordinates
array and DeepNesting captures its root container, so every Extract operation
publishes selected output after validating the entire payload.

The CLI runs Cold-Alloc and Warm-Scratch (Zero-Alloc Reuse) for Parse, Validate,
Extract, and Serialize. Cold-Alloc constructs and drops engine scratch per message;
all schemas and immutable input owners are prepared beforehand. Warm entry points
prewarm the complete corpus even if `--warmup 0`, then retain cursor, VM, DOM,
selection, output and writer-frame allocations. Allocation regression checks live
in integration tests, without conditional unit-test modules. The external comparison runner
and its dependency manifest are built outside the workspace; the report archives
source, a lockfile, complete tables, and raw samples.
