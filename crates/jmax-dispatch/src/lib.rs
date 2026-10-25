//! Explicit, capability-checked scanner selection without global mutable state.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};

pub mod choose;
pub mod detect;
pub mod r#override;
pub mod portable;
pub mod short;

pub const DEFAULT_NEON_MIN_LEN: usize = 64;
pub const DEFAULT_AVX2_MIN_LEN: usize = 256;
pub const DEFAULT_AVX512_MIN_LEN: usize = 4096;

/// Usable feature bits and separately recorded OS register-state support.
///
/// User-constructed capabilities are advisory. Only portable::resolve may turn a
/// backend identifier into a cached native classifier, after fresh detection.
/// Detection intersects runtime support with each backend build's is_supported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuCapabilities {
    pub neon: bool,
    pub avx2: bool,
    pub avx512f: bool,
    pub avx512bw: bool,
    pub os_avx: bool,
    pub os_avx512: bool,
}

impl CpuCapabilities {
    pub const fn scalar_only() -> Self {
        Self {
            neon: false,
            avx2: false,
            avx512f: false,
            avx512bw: false,
            os_avx: false,
            os_avx512: false,
        }
    }

    pub const fn supports(self, backend: BackendId) -> bool {
        match backend {
            BackendId::Scalar => true,
            BackendId::Neon => self.neon,
            BackendId::Avx2 => self.avx2 && self.os_avx,
            BackendId::Avx512 => self.avx512f && self.avx512bw && self.os_avx && self.os_avx512,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DispatchPolicy {
    pub neon_min_len: usize,
    pub avx2_min_len: usize,
    pub avx512_min_len: usize,
    pub allow_avx512: bool,
}

impl DispatchPolicy {
    pub const fn conservative() -> Self {
        Self {
            neon_min_len: DEFAULT_NEON_MIN_LEN,
            avx2_min_len: DEFAULT_AVX2_MIN_LEN,
            avx512_min_len: DEFAULT_AVX512_MIN_LEN,
            allow_avx512: true,
        }
    }
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        Self::conservative()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendOverride {
    Auto,
    Require(BackendId),
    Prefer(BackendId),
}

impl Default for BackendOverride {
    fn default() -> Self {
        Self::Auto
    }
}

/// An already capability-checked safe ABI function pointer.
#[derive(Clone, Copy)]
pub struct Dispatch {
    pub(crate) backend: BackendId,
    pub(crate) classifier: ClassifyFn,
    pub(crate) block_classifier: ClassifyBlockFn,
}

/// Full-block classifier with no tail staging storage.
pub type ClassifyBlockFn = fn(&[u8; 64], &mut ScanCarry) -> Result<BlockMasks>;
/// A per-parse cached backend. Retains the existing Dispatch API name as well.
pub type Dispatcher = Dispatch;

impl Dispatch {
    /// Resolve a backend once for a complete input.
    pub fn new(input_len: usize, policy: DispatchPolicy) -> Result<Self> {
        dispatch(input_len, policy)
    }

    pub const fn block_classifier(&self) -> ClassifyBlockFn {
        self.block_classifier
    }

    pub fn classify_block(&self, input: &[u8; 64], carry: &mut ScanCarry) -> Result<BlockMasks> {
        let _t = jmax_trace::span!(@ block_site(self.backend));
        (self.block_classifier)(input, carry)
    }

    pub const fn backend(&self) -> BackendId {
        self.backend
    }
    pub const fn classifier(&self) -> ClassifyFn {
        self.classifier
    }

    /// `input` is one block, at most 64 bytes, regardless of the selection hint.
    pub fn classify(&self, input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
        let _t = jmax_trace::span!(SCAN_TAIL);
        (self.classifier)(input, carry)
    }
}

impl core::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dispatch")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DispatchReport {
    pub requested: BackendOverride,
    pub selected: BackendId,
    pub capabilities: CpuCapabilities,
    pub policy: DispatchPolicy,
    /// Whole-input size hint, not the length of any individual scanner block.
    pub input_len: usize,
    /// True only when the requested Prefer backend could not be resolved.
    pub used_fallback: bool,
}

/// Selects an automatic backend for this input-size hint.
///
/// The usable capabilities are probed once per process and reused: hardware
/// and OS state do not change under a running process, and the native
/// classifier bindings still check their own feature invariant when they are
/// resolved. An input below every native threshold needs no resolution.
pub fn dispatch(input_len: usize, policy: DispatchPolicy) -> Result<Dispatch> {
    let capabilities = cached_capabilities();
    let candidate = choose::choose(input_len, capabilities, policy);
    if candidate == BackendId::Scalar {
        return Ok(portable::scalar());
    }
    Ok(portable::resolve_supported(candidate).unwrap_or_else(|_| portable::scalar()))
}

fn cached_capabilities() -> CpuCapabilities {
    use core::sync::atomic::{AtomicU8, Ordering};
    const READY: u8 = 0x80;
    static CACHE: AtomicU8 = AtomicU8::new(0);
    let packed = CACHE.load(Ordering::Relaxed);
    if packed & READY != 0 {
        return CpuCapabilities {
            neon: packed & 1 != 0,
            avx2: packed & 2 != 0,
            avx512f: packed & 4 != 0,
            avx512bw: packed & 8 != 0,
            os_avx: packed & 16 != 0,
            os_avx512: packed & 32 != 0,
        };
    }
    let capabilities = detect::detect();
    let packed = READY
        | u8::from(capabilities.neon)
        | (u8::from(capabilities.avx2) << 1)
        | (u8::from(capabilities.avx512f) << 2)
        | (u8::from(capabilities.avx512bw) << 3)
        | (u8::from(capabilities.os_avx) << 4)
        | (u8::from(capabilities.os_avx512) << 5);
    CACHE.store(packed, Ordering::Relaxed);
    capabilities
}

pub const fn backend_name(backend: BackendId) -> &'static str {
    match backend {
        BackendId::Scalar => "scalar",
        BackendId::Neon => "neon",
        BackendId::Avx2 => "avx2",
        BackendId::Avx512 => "avx512",
    }
}

pub(crate) fn unavailable_backend() -> Error {
    Error::new(
        ErrorCode::ConstraintViolation,
        ErrorSource::Scanner,
        0,
        "requested scanner backend is unavailable",
    )
}

/// Trace site for a full-block classification by the given backend.
#[allow(dead_code)]
#[inline(always)]
fn block_site(backend: BackendId) -> jmax_trace::Site {
    match backend {
        BackendId::Scalar => jmax_trace::site::SCAN_BLOCK_SCALAR,
        BackendId::Neon => jmax_trace::site::SCAN_BLOCK_NEON,
        BackendId::Avx2 => jmax_trace::site::SCAN_BLOCK_AVX2,
        BackendId::Avx512 => jmax_trace::site::SCAN_BLOCK_AVX512,
    }
}
