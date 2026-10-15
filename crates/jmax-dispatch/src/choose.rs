//! Pure backend selection from advisory capabilities and whole-input size hints.
//!
//! Selection neither detects hardware nor resolves a callable classifier. Even
//! synthetic capability combinations are valid inputs to this policy layer.

use super::{BackendId, CpuCapabilities, DispatchPolicy};

/// Returns the highest-priority eligible backend, falling back to scalar.
///
/// Priority is AVX-512, AVX2, NEON, then scalar. Each native backend requires
/// support and an input length at least its independent policy threshold;
/// AVX-512 additionally requires `allow_avx512`. Thresholds may be zero,
/// nonmonotonic, or `usize::MAX`. The input length is a whole-input hint, not
/// the length of a scanner block, and is never narrowed or used in arithmetic.
///
/// This function is infallible and performs no probing or native execution.
/// Its result is advisory: resolving a classifier still requires fresh checks
/// through [`super::portable::resolve`].
pub const fn choose(
    input_len: usize,
    capabilities: CpuCapabilities,
    policy: DispatchPolicy,
) -> BackendId {
    if policy.allow_avx512
        && capabilities.supports(BackendId::Avx512)
        && input_len >= policy.avx512_min_len
    {
        BackendId::Avx512
    } else if capabilities.supports(BackendId::Avx2) && input_len >= policy.avx2_min_len {
        BackendId::Avx2
    } else if capabilities.supports(BackendId::Neon) && input_len >= policy.neon_min_len {
        BackendId::Neon
    } else {
        BackendId::Scalar
    }
}
