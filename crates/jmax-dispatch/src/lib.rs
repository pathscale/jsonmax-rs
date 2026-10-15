//! Explicit, capability-checked scanner selection without global mutable state.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};

pub mod detect;
pub mod choose;
pub mod portable;
pub mod r#override;

pub const DEFAULT_NEON_MIN_LEN: usize = 64;
pub const DEFAULT_AVX2_MIN_LEN: usize = 256;
pub const DEFAULT_AVX512_MIN_LEN: usize = 4096;

/// Usable feature bits and separately recorded OS register-state support.
///
/// User-constructed capabilities are advisory. Only portable::resolve may turn a
/// backend identifier into a callable native classifier, after fresh detection.
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
            neon: false, avx2: false, avx512f: false, avx512bw: false,
            os_avx: false, os_avx512: false,
        }
    }

    pub const fn supports(self, backend: BackendId) -> bool {
        match backend {
            BackendId::Scalar => true,
            BackendId::Neon => self.neon,
            BackendId::Avx2 => self.avx2 && self.os_avx,
            BackendId::Avx512 => {
                self.avx512f && self.avx512bw && self.os_avx && self.os_avx512
            }
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
    fn default() -> Self { Self::conservative() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendOverride {
    Auto,
    Require(BackendId),
    Prefer(BackendId),
}

impl Default for BackendOverride {
    fn default() -> Self { Self::Auto }
}

/// An already capability-checked safe ABI function pointer.
#[derive(Clone, Copy)]
pub struct Dispatch {
    pub(crate) backend: BackendId,
    pub(crate) classifier: ClassifyFn,
}

impl Dispatch {
    pub const fn backend(&self) -> BackendId { self.backend }
    pub const fn classifier(&self) -> ClassifyFn { self.classifier }

    /// `input` is one block, at most 64 bytes, regardless of the selection hint.
    pub fn classify(&self, input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
        (self.classifier)(input, carry)
    }
}

impl core::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dispatch").field("backend", &self.backend).finish_non_exhaustive()
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
pub fn dispatch(input_len: usize, policy: DispatchPolicy) -> Result<Dispatch> {
    let (selected, _) = r#override::select(BackendOverride::Auto, input_len, policy)?;
    Ok(selected)
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
        ErrorCode::ConstraintViolation, ErrorSource::Scanner, 0,
        "requested scanner backend is unavailable",
    )
}
