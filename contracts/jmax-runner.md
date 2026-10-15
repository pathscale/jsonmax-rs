# jmax-runner contract, version 1

Owner: W0-ROOTS-8, row 36. This freezes the public root API and the four module APIs below. Implementations live only in their assigned wave files. Every module may use `super::*`. Root definitions do not depend on child definitions. Use only the manifest's allowed dependencies and the canonical ABI types; do not introduce alternative payload, span, proof, error-code, or endpoint-spec types.

The library supports `std` and `alloc` without `std`. Only the process management items explicitly marked below require `std`. The binary uses the standard library. Allocation failures that can be detected with fallible reservation are reported rather than deliberately panicking. No operation may log raw credentials or reply bodies on failure.

## Wave 1: scenario.rs

Exact public functions:

```rust
pub fn parse_scenario(input: &[u8]) -> Result<Scenario>;
pub fn validate_scenario(scenario: &Scenario) -> Result<()>;
```

`parse_scenario` performs complete strict JSON parsing, rejects trailing content and duplicate or unknown keys, constructs owned values, then calls `validate_scenario`. Its wire format is:

```json
{
  "version": 1,
  "name": "smoke",
  "timeout_ms": 30000,
  "steps": [
    {"connect": {"name": "main", "uri": "ws://localhost:8080", "role": "public"}},
    {"call": {"connection": "main", "endpoint": 1, "params": {}, "save_as": "reply"}},
    {"assert": {"response": "reply", "path": "status", "expected": {"json_equals": "ok"}}},
    {"close": {"connection": "main"}}
  ]
}
```

`version`, `name`, and `steps` are required. `timeout_ms` defaults to `DEFAULT_TIMEOUT_MS`. Each step is an object with exactly one variant key. Variant fields match the root enum names; `receive` has `connection` and `save_as`. An expectation is exactly one of `{"exists":true}`, `{"missing":true}`, `{"json_equals":<any JSON value>}`, or `{"error_code":<u32>}`. `params` and `json_equals` retain complete owned JSON fragments, never borrowed slices into the scenario input. Strings and keys are decoded correctly, including surrogate pairs. The root timeout is a positive per-I/O-operation deadline, not a whole-suite deadline.

Reject input larger than `MAX_SCENARIO_BYTES`, more than `MAX_STEPS`, unsupported versions, empty scenario or connection/response names, zero timeouts, malformed endpoint integers, invalid JSON fragments, malformed selector paths, calls/receives/closes against inactive connections, assertions against responses not yet saved, duplicate active connection names, and duplicate response names anywhere in a scenario. Connection names may be reused after closing; response names may not. Empty paths select the entire response. Empty roles denote anonymous access. URI and role are explicit strings; transports validate supported URI schemes. All active connections may be closed automatically at scenario end, so explicit final closes are optional. Empty step arrays are valid.

Errors: JSON lexical/syntax/limit failures use `RunnerError::Json` with original ABI offsets. Model and ordering violations use `InvalidScenario { step: Some(zero_based_index), detail }`, or `step: None` for top-level errors. Failure details identify fields and constraints, never their secret values. Validation must also work for scenarios constructed directly in Rust.

## Wave 2: drive.rs

Exact public functions:

```rust
pub fn validate_reply(input: &[u8]) -> Result<()>;
pub fn assert_reply(reply: &Reply, path: &str, expected: &Expectation) -> Result<()>;
pub fn run_scenario(scenario: &Scenario, transport: &mut dyn Transport) -> Result<RunReport>;
```

`validate_reply` strictly validates exactly one complete JSON value with `Limits::strict_default()`. `assert_reply` validates the complete reply before traversing a compiled native path. A missing path is distinct from a selected null. `Exists` and `Missing` test selection presence. `JsonEquals` requires exactly one selected value and compares decoded JSON structurally: object member order is ignored, array order is significant, string escapes are decoded, and numbers compare their exact JSON lexemes (`1` differs from `1.0`). Both sides reject duplicate keys. `ErrorCode` tests `Reply.error_code`, requires an empty path, and does not infer status from fields of `body`. A mismatch returns `Assertion { step: 0, detail }`; `run_scenario` replaces that index with the actual step index. Invalid paths are `InvalidScenario`; malformed reply/expectation bodies are `Json`.

`run_scenario` validates before I/O, executes steps sequentially, starts global call sequences at 1, and increments with checked arithmetic. A call reply must have `sequence == Some(request.sequence)`; receives may have any sequence. Every reply is validated before it is saved. Named responses retain their payload ownership until the run ends. No transport success is synthesized after failure. A successful call increments the `(connection role, endpoint)` counter, including application error replies. `steps_completed` increments only after a whole step succeeds; `assertions_passed` only after an assertion passes.

Track every successfully connected active name. On success and on every failure, attempt to close every remaining connection once in lexical name order, even if an earlier close failed. Explicit failed closes remain active for cleanup. Preserve the first execution error over cleanup errors; on otherwise successful execution return the first cleanup error. Drop response storage before return. Counter and sequence overflow return `InvalidScenario`. Transport errors pass through with sanitized context. A failed run returns no successful report.

Transport deadlines are relative milliseconds and strictly positive. The transport enforces deadlines for connect/call/receive, owns its reactor/scheduling, and treats `close` as idempotent resource cleanup where feasible. Implementations may return `Timeout`; runner code must not block a reactor with a sleep loop.

The binary exposes `validate SCENARIO`, `run SCENARIO --replies JSON_LINES`, help, and version. Replay is an explicitly offline transport. Each nonempty reply line is one JSON body, consumed globally in step order; call sequences are supplied by replay and application error status is absent. Replay therefore cannot simulate `ErrorCode` expectations. Exhausted or unused replies fail. Input reads are bounded. Exit codes: 0 success, 2 scenario/argument errors, 1 execution or I/O errors. Output contains counts, never bodies or scenario names that might contain secrets.

## Wave 3: coverage.rs

Exact public functions:

```rust
pub fn required_coverage(services: &[ServiceSpec]) -> Result<alloc::collections::BTreeSet<CoverageKey>>;
pub fn planned_coverage(scenarios: &[Scenario]) -> Result<alloc::collections::BTreeSet<CoverageKey>>;
pub fn observed_coverage(reports: &[RunReport]) -> alloc::collections::BTreeSet<CoverageKey>;
pub fn compare_coverage(
    required: &alloc::collections::BTreeSet<CoverageKey>,
    observed: &alloc::collections::BTreeSet<CoverageKey>,
) -> CoverageReport;
```

`required_coverage` contains every endpoint/declared-role pair; an endpoint with no declared roles contributes exactly the anonymous empty-string role. Reject duplicate global endpoint codes, duplicate service IDs or names, and repeated roles on one endpoint as `InvalidScenario { step: None, .. }`. Distinct roles and endpoint codes are never case-folded. `planned_coverage` first validates all scenarios, follows connection open/close/reuse state, and collects calls under the role current at that step. Plans do not count as observed execution. `observed_coverage` includes only positive report counters, deduplicates suite membership, and never changes reports. Comparison fills `covered = required intersection observed`, `missing = required minus observed`, and `unexpected = observed minus required`. `required` is copied exactly. Set iteration and diagnostics are deterministic. `is_complete` means no missing pairs; unexpected pairs remain visible without changing that predicate.

## Wave 4: process.rs

Exact public functions and additional module types:

```rust
pub fn validate_boot(spec: &BootSpec) -> Result<()>;
pub fn redact(input: &str, policy: &RedactionPolicy) -> String;

#[cfg(feature = "std")]
pub trait ReadinessProbe {
    fn is_ready(&mut self) -> Result<bool>;
}
#[cfg(feature = "std")]
pub struct ManagedProcess { /* private child, deadline, state, and redaction fields */ }
#[cfg(feature = "std")]
impl ManagedProcess {
    pub fn spawn(spec: &BootSpec, policy: RedactionPolicy) -> Result<Self>;
    pub fn id(&self) -> u32;
    pub fn poll(&mut self, probe: &mut dyn ReadinessProbe) -> Result<ProcessState>;
    pub fn terminate(&mut self) -> Result<ProcessState>;
}
#[cfg(feature = "std")]
impl Drop for ManagedProcess { fn drop(&mut self); }
```

Private representation is not frozen; the public fields of all root types are frozen. `validate_boot` rejects empty program names, zero startup deadlines, NUL in program/arguments/cwd/environment, empty environment keys, keys containing `=`, and an empty explicit cwd. Use `Process` errors with safe fixed diagnostics. `redact` delegates exactly to `RedactionPolicy::redact`: matching is case-sensitive and literal, empty secrets are ignored, longest matches win at each original-input character boundary, replacements are not rescanned, and Unicode stays valid.

`spawn` validates, invokes the program directly without a shell, applies `clear_env`, explicit environment and cwd, and routes stdin/stdout/stderr to null to avoid inherited credential-bearing streams and full-pipe deadlocks. It tracks a checked monotonic startup deadline. Spawn failures are sanitized through the policy before becoming `Process` errors. The child is owned uniquely and never detached. `id` remains the spawned PID after exit. `poll` performs one nonblocking child status check, then at most one readiness probe while Starting. Child exit wins over readiness, successful readiness transitions to Ready, and timeout terminates/reaps the child and returns `Timeout`. A probe error terminates/reaps and preserves the probe error. Once Ready, subsequent polls only check liveness; once Exited, they return the saved state without probing. Probe implementations must return promptly. Callers schedule future polls; this API does not busy-wait or sleep.

`terminate` checks for an already exited child, otherwise kills and waits to reap it, caches `Exited`, and is idempotent. A child-exit race is resolved by rechecking status before treating a kill failure as fatal. Drop performs best-effort kill/reap without panicking. Every early error after successful spawn must retain cleanup ownership. No global environment mutation or signal handler installation is allowed. The implementation must not include environment values, raw arguments, or probe body contents in its own diagnostics.

## Frozen shared root API

The following source snapshot specifies exact root public definitions, derives, constants, method signatures, and implemented helper semantics. `Result` is runner-specific; embedded JSON errors remain canonical ABI errors. Private fields cannot be constructed or mutated by downstream users.

```rust
//! Scenario execution contracts shared by the runner's four implementation waves.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::fmt;
pub use jmax_abi::{
    buffer::Payload,
    endpoint::{EndpointSchemaSpec, ServiceSpec},
    error::{Error, ErrorCode, ErrorSource},
    syntax::Limits,
};

pub mod scenario;
pub mod drive;
pub mod coverage;
pub mod process;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;
pub const MAX_SCENARIO_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_STEPS: usize = 100_000;
pub const REDACTED: &str = "[REDACTED]";
pub type Result<T> = core::result::Result<T, RunnerError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunnerError {
    InvalidScenario { step: Option<usize>, detail: String },
    UnknownConnection(String),
    UnknownResponse(String),
    Transport { connection: String, detail: String },
    Assertion { step: usize, detail: String },
    Timeout { operation: String },
    Process { detail: String },
    Io { operation: &'static str, detail: String },
    Json(Error),
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScenario { step, detail } => write!(f, "invalid scenario at {step:?}: {detail}"),
            Self::UnknownConnection(name) => write!(f, "unknown connection: {name}"),
            Self::UnknownResponse(name) => write!(f, "unknown response: {name}"),
            Self::Transport { connection, detail } => write!(f, "transport {connection}: {detail}"),
            Self::Assertion { step, detail } => write!(f, "assertion at step {step}: {detail}"),
            Self::Timeout { operation } => write!(f, "timeout: {operation}"),
            Self::Process { detail } => write!(f, "process: {detail}"),
            Self::Io { operation, detail } => write!(f, "I/O {operation}: {detail}"),
            Self::Json(error) => fmt::Display::fmt(error, f),
        }
    }
}
impl core::error::Error for RunnerError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Json(error) => Some(error), _ => None }
    }
}
impl From<Error> for RunnerError {
    fn from(error: Error) -> Self { Self::Json(error) }
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub timeout_ms: u32,
    pub steps: Vec<Step>,
}
#[derive(Clone, Debug)]
pub enum Step {
    Connect { name: String, uri: String, role: String },
    Call { connection: String, endpoint: u32, params: Payload, save_as: String },
    Receive { connection: String, save_as: String },
    Assert { response: String, path: String, expected: Expectation },
    Close { connection: String },
}
#[derive(Clone, Debug)]
pub enum Expectation {
    Exists,
    Missing,
    JsonEquals(Payload),
    ErrorCode(u32),
}
#[derive(Clone, Debug)]
pub struct CallRequest {
    pub endpoint: u32,
    pub sequence: u64,
    pub params: Payload,
}
#[derive(Clone, Debug)]
pub struct Reply {
    pub sequence: Option<u64>,
    pub error_code: Option<u32>,
    pub body: Payload,
}

/// Implementations own their I/O and must enforce the supplied relative deadline.
/// Errors must not contain credentials or unsanitized payload contents.
pub trait Transport {
    fn connect(&mut self, name: &str, uri: &str, role: &str, timeout_ms: u32) -> Result<()>;
    fn call(&mut self, name: &str, request: &CallRequest, timeout_ms: u32) -> Result<Reply>;
    fn receive(&mut self, name: &str, timeout_ms: u32) -> Result<Reply>;
    fn close(&mut self, name: &str) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoverageKey {
    pub role: String,
    pub endpoint: u32,
}
#[derive(Clone, Debug, Default)]
pub struct RunReport {
    pub scenario: String,
    pub steps_completed: usize,
    pub assertions_passed: usize,
    pub calls: BTreeMap<CoverageKey, u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoverageReport {
    pub required: alloc::collections::BTreeSet<CoverageKey>,
    pub covered: alloc::collections::BTreeSet<CoverageKey>,
    pub missing: alloc::collections::BTreeSet<CoverageKey>,
    pub unexpected: alloc::collections::BTreeSet<CoverageKey>,
}
impl CoverageReport {
    pub fn is_complete(&self) -> bool { self.missing.is_empty() }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub clear_env: bool,
    pub startup_timeout_ms: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState { Starting, Ready, Exited { code: Option<i32> } }

/// Redacts exact secret strings, longest first. Empty secrets are discarded.
#[derive(Clone, Debug, Default)]
pub struct RedactionPolicy { secrets: Vec<String> }
impl RedactionPolicy {
    pub fn new(mut secrets: Vec<String>) -> Self {
        secrets.retain(|value| !value.is_empty());
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Self { secrets }
    }
    pub fn redact(&self, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut offset = 0;
        while offset < input.len() {
            let remaining = &input[offset..];
            if let Some(secret) = self.secrets.iter().find(|secret| remaining.starts_with(secret.as_str())) {
                result.push_str(REDACTED);
                offset += secret.len();
            } else if let Some(ch) = remaining.chars().next() {
                result.push(ch);
                offset += ch.len_utf8();
            }
        }
        result
    }
}
```
