# jmax-dispatch frozen contract

Owner: W0-ROOTS-1. Allowed crate dependencies: jmax-abi, jmax-scan-scalar,
jmax-scan-neon, jmax-scan-avx2, jmax-scan-avx512. No allocations, mutable global
selection, environment reads, external detection library, or initialization
ordering requirements. Support both std and no_std.

## Root vocabulary and implementations

The root declares `pub mod detect; pub mod choose; pub mod portable;
pub mod r#override;`. The last module's file is src/override.rs. All children may
`use super::*`. Canonical ABI types retain their identity and layout:

```rust
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};
pub const DEFAULT_NEON_MIN_LEN: usize = 64;
pub const DEFAULT_AVX2_MIN_LEN: usize = 256;
pub const DEFAULT_AVX512_MIN_LEN: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuCapabilities {
    pub neon: bool,
    pub avx2: bool,
    pub avx512f: bool,
    pub avx512bw: bool,
    pub os_avx: bool,
    pub os_avx512: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DispatchPolicy {
    pub neon_min_len: usize,
    pub avx2_min_len: usize,
    pub avx512_min_len: usize,
    pub allow_avx512: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendOverride { Auto, Require(BackendId), Prefer(BackendId) }
#[derive(Clone, Copy)]
pub struct Dispatch {
    pub(crate) backend: BackendId,
    pub(crate) classifier: ClassifyFn,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DispatchReport {
    pub requested: BackendOverride,
    pub selected: BackendId,
    pub capabilities: CpuCapabilities,
    pub policy: DispatchPolicy,
    pub input_len: usize,
    pub used_fallback: bool,
}
```

Root-implemented signatures:

```rust
impl CpuCapabilities {
    pub const fn scalar_only() -> Self;
    pub const fn supports(self, backend: BackendId) -> bool;
}
impl DispatchPolicy { pub const fn conservative() -> Self; }
impl Default for DispatchPolicy { fn default() -> Self; }
impl Default for BackendOverride { fn default() -> Self; }
impl Dispatch {
    pub const fn backend(&self) -> BackendId;
    pub const fn classifier(&self) -> ClassifyFn;
    pub fn classify(&self, input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks>;
}
impl core::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}
pub fn dispatch(input_len: usize, policy: DispatchPolicy) -> Result<Dispatch>;
pub const fn backend_name(backend: BackendId) -> &'static str;
```

Scalar_only and CpuCapabilities::default set all fields false. Supports(Scalar)
is always true; Neon requires neon; Avx2 requires avx2 && os_avx; Avx512 requires
avx512f && avx512bw && os_avx && os_avx512. This is a pure advisory predicate on
possibly synthetic caller data, not permission to execute native instructions.

DispatchPolicy::default is conservative(): root threshold constants and
allow_avx512=true. BackendOverride::default is Auto. Thresholds are independent,
can be zero or usize::MAX, and need not be monotonic. Dispatch Debug includes
only backend, not a pointer address. Backend_name returns exactly `scalar`,
`neon`, `avx2`, `avx512`. Dispatch::classify simply calls its checked safe ABI
pointer; input is still at most 64 bytes. Root dispatch calls override::select
with Auto and returns the dispatch, discarding the report.

Root-private `unavailable_backend() -> Error` returns
ConstraintViolation/Scanner, offset 0, detail
`requested scanner backend is unavailable`.

## Wave 1: src/detect.rs

```rust
pub fn detect() -> CpuCapabilities;
pub fn is_available(backend: BackendId) -> bool;
```

Detection returns capabilities usable by this build and target, not merely CPU
advertising bits. Intersect CPU/OS availability with each scanner crate's public
is_supported(). This is necessary because dependencies may be built without
std even when this crate has std enabled. Do not assume feature propagation
that is absent from Cargo.toml, and do not change that manifest.

With std on x86/x86_64 use std::is_x86_feature_detected for avx, avx2, avx512f,
avx512bw. These checks incorporate OS-enabled extended register state; CPUID
feature bits alone are insufficient. Set os_avx from the AVX usable-state
check. Set os_avx512 from the AVX-512F usable-state check. Set avx2 to the AVX2
usable-state check AND jmax_scan_avx2::is_supported(). Set avx512f and avx512bw
to their corresponding usable-state checks AND jmax_scan_avx512::is_supported().
Set neon=false. The AVX-512 requirement is F plus BW, with no VL requirement.

Without std on x86/x86_64, use only target_feature guarantees: os_avx is true
when avx, avx2 or avx512f is statically enabled; os_avx512 requires avx512f;
the avx2 and AVX-512 feature fields additionally require their exact statically
enabled features and the corresponding scanner is_supported(). A build's
explicit target features are a deployment contract with the CPU and OS. Do not
speculate that unknown hardware or OS state is available.

On aarch64, neon is jmax_scan_neon::is_supported() (runtime detection with std,
compile-time NEON target feature without std). All x86 fields are false. On all
other targets every field is false. This initial NEON contract deliberately
uses only aarch64, so arm32 falls back. Scalar is always available.

Is_available uses fresh detect().supports(backend), with an explicit scalar
fast path allowed. Detection must not call choose, portable, or override and
must never execute an instruction merely to see if it traps. No error path,
cache, architecture-specific public type, allocation or global mutation.

## Wave 2: src/choose.rs

```rust
pub const fn choose(
    input_len: usize, capabilities: CpuCapabilities, policy: DispatchPolicy,
) -> BackendId;
```

This is a pure policy function, not a function-pointer resolver. Return the first
eligible backend in this priority order:

1. Avx512 when allow_avx512, capabilities.supports(Avx512), and input_len >=
   avx512_min_len.
2. Avx2 when supported and input_len >= avx2_min_len.
3. Neon when supported and input_len >= neon_min_len.
4. Scalar otherwise.

Input_len is a whole-input size hint, can exceed 64, and is never added,
multiplied or narrowed. Exact thresholds qualify. No errors, probing, allocation,
native execution, or mutable state. Synthetic capabilities are valid policy
inputs, including combinations impossible on real hardware.

## Wave 3: src/portable.rs

```rust
pub fn scalar() -> Dispatch;
pub fn resolve(backend: BackendId) -> Result<Dispatch>;
```

Scalar returns Dispatch with BackendId::Scalar and the safe ABI pointer
`jmax_scan_scalar::classify::classify`. Resolve checks fresh detect::is_available
before selecting a non-scalar function pointer. Unsupported requests return
unavailable_backend(); they do not silently become scalar. The exact pointers
are `jmax_scan_neon::classify::classify`,
`jmax_scan_avx2::classify::classify`, and
`jmax_scan_avx512::classify::classify`. These safe module entry points are
portable on every architecture; never bind to a private target_feature routine
or transmute an unsafe pointer. Module paths are intentional: the AVX roots do
not promise root-level classify reexports. The backend crates independently
check safety before any SIMD execution, even when reached through a dispatcher.

An empty classification may report the Scalar tag even through a native
Dispatch; Dispatch::backend records the selected backend, while BlockMasks's
tag records the work actually done on that block. Scalar can be constructed
without detection and cannot fail. No dependency on choose or override.

## Wave 4: src/override.rs

```rust
pub fn parse(value: &str) -> Result<BackendOverride>;
pub fn select(
    requested: BackendOverride, input_len: usize, policy: DispatchPolicy,
) -> Result<(Dispatch, DispatchReport)>;
```

Parse accepts exactly lowercase `auto`, `scalar`, `neon`, `avx2`, `avx512`, and
`prefer:scalar`, `prefer:neon`, `prefer:avx2`, `prefer:avx512`. A bare backend is
Require; a prefer prefix is Prefer. No whitespace trimming, case folding, empty
string default, `require:` alias, or environment lookup. Any other spelling is
InvalidToken/Scanner at offset 0, with static payload-free detail. Parsing does
not detect hardware, so syntactically valid unavailable requests still parse.

Select takes a fresh effective capability snapshot and applies:

- Auto: choose(input_len, capabilities, policy), then portable::resolve.
- Require(id): resolve id if supported, else unavailable_backend(). Required
  requests bypass size thresholds and allow_avx512, but never safety checks.
- Prefer(id): resolve id if supported, bypassing thresholds and allow_avx512;
  otherwise use automatic policy and mark used_fallback=true.

If fresh resolution rejects a non-scalar backend after the snapshot, Require
returns the error; Auto and Prefer must resolve scalar and return a truthful
report. For such a resolution-time failure, refresh the report's capability
snapshot and set used_fallback=true only for Prefer. With this rule Auto always
has a scalar path even when successive detection results differ. Report selected
must equal Dispatch::backend; record the original request, policy and input hint.
Used_fallback is false for Auto and Require, and for a supported Prefer request;
it is true for Prefer only when the requested backend could not be resolved.
No process-global override, environment variable, logging, or side effects.

## Cross-crate invariants

Never treat caller-supplied CpuCapabilities as authority to execute SIMD. Every
resolved pointer is a safe canonical ClassifyFn; ABI scan carry and masks are
neither wrapped nor changed. All scanner backends use the same low-bit-first
64-byte mask semantics and block-relative errors. Unsupported hardware is not a
schema error: the canonical constraint error is used because there is no
dedicated hardware error code. Every generated detail is static and contains no
override input bytes. Classification errors pass through unchanged.

Dependency graph: detect -> backend capability APIs; choose -> root only;
portable -> detect and backend classifiers; override -> detect, choose, portable;
root dispatch -> override. No reverse dependency and no cycle among wave modules.
The crate can be linked before invoking detection and requires no heap or locks.
