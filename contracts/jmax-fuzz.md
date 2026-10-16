# jmax-fuzz contract, version 1

Owner: W0-ROOTS-8, row 38. The root source snapshot freezes exact shared public types, traits, constants and helpers. Modules may use `super::*`; they must not create ABI lookalikes or introduce dependencies outside the allowed manifest list. The library supports `std` and `alloc` without `std`. Randomness is always the root SplitMix64 generator, never host entropy. No harness catches a target panic and reports agreement: panics remain visible to an external fuzzing engine.

Every harness distinguishes a rejected tested input (`Outcome::Rejected` or `NumberOutcome::Rejected`) from an infrastructure/configuration failure (outer ABI `Result::Err`). Reports contain concrete replayable failing bytes and case index/seed, never just a hash. Reports may contain sensitive input and must not be automatically logged. All byte offsets are relative to the tested slice, not padded backing storage.

## Wave 1: syntax.rs

Exact public functions:

```rust
pub fn seed_corpus(max_input_bytes: usize) -> Result<Vec<Payload>>;
pub fn mutate(input: &[u8], rng: &mut DeterministicRng, max_input_bytes: usize) -> Result<Vec<u8>>;
pub fn run_syntax(
    inputs: &[Payload],
    left: &mut dyn SyntaxOracle,
    right: &mut dyn SyntaxOracle,
    limits: Limits,
    config: FuzzConfig,
) -> Result<FuzzReport>;
```

`seed_corpus` returns a deterministic ordered mix of valid and invalid strict JSON: all scalar roots; empty and nested containers; object delimiter/colon/comma violations; escaped quotes and reverse solidus runs; controls; UTF-8 truncation and invalid sequences; valid surrogate pairs and lone surrogates; duplicate decoded keys; invalid numbers; trailing whitespace and non-whitespace; and truncated strings/containers/literals. Include lengths near 0, 1, 15, 16, 31, 32, 63, 64, 65, 127 and 128 where the byte cap permits. Entries exceeding the cap are omitted, never silently truncated into a different seed. Zero or greater-than-u32 max_input_bytes is `ConstraintViolation`.

`mutate` uses one RNG draw to choose among byte insertion, deletion, replacement, duplication, and truncation, then draws positions/bytes only for the chosen operation. Use byte operations so invalid UTF-8 is representable. Empty inputs and a full byte budget must still permit progress without out-of-bounds indexing; an impossible insertion/duplication becomes one-byte replacement (or insertion for empty input if capacity permits). Oversized original input is `PayloadTooLarge`. RNG evolution is deterministic and has no rejection loop with unbounded work. Output length never exceeds the cap. All allocations are bounded by the cap and use fallible reservation when possible.

`run_syntax` validates config and requires a nonempty seed list with no oversized input. Cases 0 through inputs.len()-1 use seeds unchanged, capped by max_cases. Subsequent cases choose a seed via RNG.index and mutate it with the same RNG seeded by config.seed. Both oracles receive identical bytes and Limits, with no changed dispatch state between observations unless the caller intentionally supplied backend-specific adapters. Per-case state must reset in adapters. Call left then right; outer failures abort without fabricating a successful report. Accepted results use the same canonical convention: compact JSON, object keys ordered by decoded UTF-8 byte order, source array order, decoded strings escaped minimally, exact original number lexemes, and strict duplicate-key rejection. This exposes value corruption beyond acceptance differences. The oracles must implement the same duplicate-key, numeric and limits policy before comparison.

Use `compare_outcomes` to select the mismatch kind. Increment attempted exactly once after both observations succeed, agreed only if they compare equal. Record each mismatch with original input, zero-based case index and the starting config seed. Fixed diagnostic text may identify the two oracle names but must not require Debug-printing unbounded trees. `max_failures` caps stored records; after reaching the cap, stop before generating further cases and set truncated if any configured case remains. A completed run with a mismatch is not success; a zero-attempt run is not success.

## Wave 2: schema.rs

Exact public functions:

```rust
pub fn constraint_corpus(config: FuzzConfig) -> Result<Vec<SchemaCase>>;
pub fn run_schema(
    cases: &[SchemaCase],
    aot: &mut dyn SchemaOracle,
    vm: &mut dyn SchemaOracle,
    limits: Limits,
    config: FuzzConfig,
) -> Result<FuzzReport>;
```

`constraint_corpus` generates deterministic schema/payload/selection cases covering required and optional fields, duplicate and unknown keys, scalar type mismatches, integer lower/upper bounds, string scalar-length bounds, supported formats, and strict validation of unselected invalid fields. Include exact-boundary and one-past-boundary inputs. Construct real valid ABI schema images through the allowed compiler facade or AOT machinery, with matching fingerprints; do not hardcode a pretend accepted result or use unchecked proof issuance. Unsupported constraints must be visible as `UnsupportedSchemaFeature`, not omitted while claiming full coverage. Images are validated before returning them. Limit case count and payload length by config. Corpus generation errors are outer errors.

`run_schema` first validates every supplied image and selection, rejects empty cases and oversized payloads, and executes at most max_cases in original order. It does not cycle cases or generate hidden schema mutations. AOT and VM receive the exact same immutable image, owner-bearing payload, selection, and limits. AOT adapters must verify their generated table/image fingerprint matches the case before execution; an unsupported case is an outer failure, not acceptance. Successful canonical bytes encode selected fields in Selection.field_ids order with explicit type tags and length prefixes; strings resolve to decoded UTF-8, raw spans to their bytes, integers and decimals retain exact type/coefficient/scale, f64 retains all bits, and Empty is distinct from null or an empty string. When selection is empty, both adapters return an empty canonical sequence only after complete schema validation. Rejection comparisons and accounting match run_syntax.

A public selection with strict_validate_unselected false is not upgraded into full validation; differential adapters must agree on the requested mode and cannot publish a complete validation proof for unchecked data. Invalid schema images and out-of-range/duplicate selection IDs are infrastructure `SchemaImageInvalid` errors. Payload schema failures appear as `Outcome::Rejected`. Source offsets are original payload byte offsets.

## Wave 3: ownership.rs

Exact public functions:

```rust
pub fn validate_snapshot(snapshot: &OwnershipSnapshot, max_retained_bytes: u64) -> Result<()>;
pub fn run_ownership(
    inputs: &[Payload],
    probe: &mut dyn OwnershipProbe,
    ownership: OwnershipConfig,
    config: FuzzConfig,
) -> Result<FuzzReport>;
```

`validate_snapshot` verifies raw spans using Span32::slice, source-specific string spans, UTF-8 and scalar_len, decoded length representability, and checked accounting. RawPayload references resolve only against payload, DecodedArena only against decoded. String scalar count is Unicode scalar values, not bytes or UTF-16 units. Every reported string and raw span must fit its owning buffer including checked offset+len overflow. At minimum retained_bytes must cover logical payload length plus decoded Vec capacity, without arithmetic overflow; implementations must count larger shared backing allocations and other retained storage when known. It must not exceed max_retained_bytes. It is legal to report no references for a scalar without strings, but a probe cannot drop actual string references merely to hide corruption. Range failures use `InternalError`, bad decoded UTF-8 `InvalidUtf8`, scalar count mismatch `ConstraintViolation`, and retention failures `BudgetExhausted`.

`run_ownership` requires nonempty valid strict input values, positive retained-byte cap, alignments in 1..=64, valid FuzzConfig, and inputs within config.max_input_bytes. Invalid caller inputs are outer JSON errors. For each input, create a fresh backing Vec with each byte offset in 0..alignments and append sentinel tail bytes; pass only the exact input subslice to the probe. Use at least 64 tail bytes and vary sentinels deterministically, including invalid UTF-8 and structural characters. The probe has no access to sentinels outside the borrowed slice. Inspect the snapshot after dropping the temporary backing Vec and verify its owned payload bytes equal the original input. All offset/length and retention checks then run against the owned snapshot. Different alignments must yield identical resolved string/raw-span contents, ignoring offsets into distinct decoded packing layouts. Never construct out-of-bounds slices or inaccessible guard-page references in safe Rust.

One alignment is one attempted case and max_cases caps total probe calls. A probe error on previously validated input is recorded as Ownership (or Retention for BudgetExhausted), not converted into agreement. Invalid snapshots similarly become bounded Failure records. Allocation/configuration failures in the harness remain outer errors. Preserve the exact tested JSON, case index and seed. Failure budget/truncated behavior matches run_syntax. Probes own admission/release accounting; the harness never forges ValidationProof, derives a static lifetime from input, or dereferences a naked span without its owner.

## Wave 4: numbers.rs

Exact public functions:

```rust
pub fn adversarial_corpus(config: NumberConfig, fuzz: FuzzConfig) -> Result<Vec<NumberCase>>;
pub fn run_numbers(
    cases: &[NumberCase],
    left: &mut dyn NumberOracle,
    right: &mut dyn NumberOracle,
    number: NumberConfig,
    fuzz: FuzzConfig,
) -> Result<FuzzReport>;
```

Validate numeric policy exponent bound is nonnegative and decimal precision in 1..=38, scale at most 38, tick_step positive, with scale at most precision. Invalid policy is `ConstraintViolation`. `adversarial_corpus` deterministically covers u64/i64 extrema and overflow neighbors, leading zero/sign/fraction/exponent grammar, negative zero, maximum exponent boundaries, halfway f64 round-to-even values, exact powers of two, subnormal/normal boundary, smallest subnormals, maximal finite f64 and overflowing exponents. Decimal cases cover i128 accumulation, precision, scale extension/reduction, inexact rescale, exponent policy, quoted decimal grammar and exact tick multiples. Quoted decimal inputs include their JSON quote bytes. Include zero, negative coefficients, and overlong digit runs, within the input cap.

Set expected to Some for cases whose exact expected outcome is established independently by constants or checked integer arithmetic; use known IEEE bit patterns for rounding anchors. A field value of None means differential-only and must never be interpreted as an expected rejection. Do not compute expected by calling either tested oracle. Cases whose expected result depends on configured policies must be recomputed with independent checked rules or marked None. Do not use floating-point arithmetic as a decimal reference. At least one accepted and one rejected known-answer case are required when the byte budget can represent them; otherwise return ConstraintViolation. Corpus output is bounded by fuzz.max_cases and max_input_bytes.

`run_numbers` validates configs, requires nonempty cases, enforces byte/case budgets, then calls both oracles in order. Compare exact ConvertedNumber variants and contents; F64Bits compare all 64 bits including signed zero; Decimal compares coefficient and scale, not a lossy f64 conversion. Nonfinite outputs under a rejecting policy are a mismatch even if both oracles agree. Check Some(expected) against both observations as well as comparing left/right, so equal wrong answers still fail. Accepted number variants must correspond to NumberMode. Rejected codes follow the configured RejectionComparison: AcceptanceOnly compares rejection status; Code and CodeAndOffset both compare codes because number outcomes carry no offset. Known-answer expectations always compare exact rejection codes. A mismatch is FailureKind::Numeric with one record per case. Outer oracle errors abort. Failure accounting and stopping follow the syntax harness. Number parsing errors belong inside NumberOutcome::Rejected rather than the outer Result.

## Shared invariants

FuzzConfig::validate rejects zero case/byte/failure budgets, input caps above the ABI's u32 range and stored failure caps above MAX_RECORDED_FAILURES. Every function validates configuration before oracle interaction. Root record_failure only appends bounded records; module loops own attempted/agreed counters and set truncated when early termination leaves work. Therefore attempted == agreed + failures.len() for completed non-truncated runs. Failures do not cause panics, assertions or success-only reports. All shared state is caller-owned, permitting repeated deterministic runs without global mutation.

## Frozen shared root API

Exact shared definitions, defaults, method signatures and comparison/RNG semantics follow. ConvertedNumber is reused directly and only guarantees PartialEq, so numeric records deliberately do not promise Eq.

```rust
//! Deterministic differential and ownership harness contracts.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::Payload,
    error::{Error, ErrorCode, ErrorSource, Result},
    number::{ConvertedNumber, Decimal128, DecimalPolicy, NumericPolicy},
    scan::BackendId,
    schema::{SchemaImage, Selection},
    span::{Span32, SpanSource, StrRef},
    syntax::Limits,
};

pub mod syntax;
pub mod schema;
pub mod ownership;
pub mod numbers;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_SEED: u64 = 0x4a4d_4158_4655_5a5a;
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_CASES: u32 = 10_000;
pub const MAX_RECORDED_FAILURES: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionComparison { AcceptanceOnly, Code, CodeAndOffset }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuzzConfig {
    pub seed: u64,
    pub max_cases: u32,
    pub max_input_bytes: usize,
    pub max_failures: u32,
    pub rejection_comparison: RejectionComparison,
}
impl Default for FuzzConfig {
    fn default() -> Self {
        Self { seed: DEFAULT_SEED, max_cases: DEFAULT_MAX_CASES, max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_failures: 32, rejection_comparison: RejectionComparison::AcceptanceOnly }
    }
}
impl FuzzConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_cases == 0 || self.max_input_bytes == 0 || self.max_input_bytes > u32::MAX as usize
            || self.max_failures == 0 || self.max_failures > MAX_RECORDED_FAILURES {
            return Err(harness_error(ErrorCode::ConstraintViolation, "invalid fuzz configuration"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted { canonical: Vec<u8> },
    Rejected { code: ErrorCode, byte_offset: u32 },
}
/// Canonical bytes encode the complete result under a common, documented policy.
/// An outer Err is harness failure. Invalid tested input is Outcome::Rejected.
pub trait SyntaxOracle {
    fn name(&self) -> &str;
    fn observe(&mut self, input: &[u8], limits: Limits) -> Result<Outcome>;
}
pub trait SchemaOracle {
    fn name(&self) -> &str;
    fn observe(&mut self, input: &Payload, schema: &SchemaImage, selection: &Selection, limits: Limits) -> Result<Outcome>;
}
#[derive(Clone, Debug)]
pub struct SchemaCase {
    pub name: String,
    pub payload: Payload,
    pub schema: SchemaImage,
    pub selection: Selection,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind { Acceptance, Value, ErrorCode, ErrorOffset, Ownership, Retention, Numeric }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub case_index: u64,
    pub seed: u64,
    pub kind: FailureKind,
    pub input: Vec<u8>,
    pub detail: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FuzzReport {
    pub attempted: u64,
    pub agreed: u64,
    pub failures: Vec<Failure>,
    pub truncated: bool,
}
impl FuzzReport {
    pub fn is_success(&self) -> bool { self.attempted > 0 && self.attempted == self.agreed && self.failures.is_empty() && !self.truncated }
    /// Returns false once the configured failure storage budget is exhausted.
    pub fn record_failure(&mut self, failure: Failure, max_failures: u32) -> bool {
        if self.failures.len() >= max_failures.min(MAX_RECORDED_FAILURES) as usize {
            self.truncated = true;
            return false;
        }
        self.failures.push(failure);
        true
    }
}

#[derive(Clone, Debug)]
pub struct OwnershipSnapshot {
    pub payload: Payload,
    pub decoded: Vec<u8>,
    pub strings: Vec<StrRef>,
    pub raw_spans: Vec<Span32>,
    pub retained_bytes: u64,
}
pub trait OwnershipProbe {
    /// The snapshot owns all storage after this borrowed input has expired.
    fn inspect(&mut self, input: &[u8], limits: Limits) -> Result<OwnershipSnapshot>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnershipConfig {
    pub alignments: u8,
    pub max_retained_bytes: u64,
    pub limits: Limits,
}
impl Default for OwnershipConfig {
    fn default() -> Self {
        Self { alignments: 64, max_retained_bytes: 4 * DEFAULT_MAX_INPUT_BYTES as u64, limits: Limits::strict_default() }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberMode { Unsigned, Signed, Float, Decimal }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumberConfig {
    pub numeric: NumericPolicy,
    pub decimal: DecimalPolicy,
}
impl Default for NumberConfig {
    fn default() -> Self { Self { numeric: NumericPolicy::strict_default(), decimal: DecimalPolicy::spot_8() } }
}
#[derive(Clone, Debug, PartialEq)]
pub enum NumberOutcome { Value(ConvertedNumber), Rejected(ErrorCode) }
#[derive(Clone, Debug, PartialEq)]
pub struct NumberCase {
    pub name: String,
    pub lexeme: Vec<u8>,
    pub mode: NumberMode,
    pub expected: Option<NumberOutcome>,
}
pub trait NumberOracle {
    fn name(&self) -> &str;
    fn observe(&mut self, input: &[u8], mode: NumberMode, config: NumberConfig) -> Result<NumberOutcome>;
}

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
    pub fn index(&mut self, upper_exclusive: usize) -> Option<usize> {
        if upper_exclusive == 0 { None } else { Some((self.next_u64() % upper_exclusive as u64) as usize) }
    }
}

/// Returns the first observable mismatch, in acceptance/value/code/offset order.
pub fn compare_outcomes(left: &Outcome, right: &Outcome, policy: RejectionComparison) -> Option<FailureKind> {
    match (left, right) {
        (Outcome::Accepted { canonical: left }, Outcome::Accepted { canonical: right }) => {
            if left == right { None } else { Some(FailureKind::Value) }
        }
        (Outcome::Rejected { code: left, byte_offset: lo }, Outcome::Rejected { code: right, byte_offset: ro }) => {
            match policy {
                RejectionComparison::AcceptanceOnly => None,
                _ if left != right => Some(FailureKind::ErrorCode),
                RejectionComparison::CodeAndOffset if lo != ro => Some(FailureKind::ErrorOffset),
                _ => None,
            }
        }
        _ => Some(FailureKind::Acceptance),
    }
}
pub fn harness_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}
```

## Wave 5: native.rs

```rust
pub fn native_seed_corpus(max_input_bytes: usize) -> Result<Vec<Payload>>;
pub fn run_native_vs_vm(inputs: &[Payload], native: &mut dyn SyntaxOracle, vm: &mut dyn SyntaxOracle, limits: Limits, config: FuzzConfig) -> Result<FuzzReport>;
```

native_seed_corpus returns deterministic Binance aggTrade, depthUpdate (0, 1, 20 and 1000
levels), snapshot and RPC request documents plus their mutations by syntax.rs mutate. The
harness is run_syntax with RejectionComparison::CodeAndOffset fixed: native is the public
jmax-native decode_into outcome and vm the schema VM outcome, so any difference in
acceptance, code or offset is a failure. Accepted outcomes compare canonical typed values.
