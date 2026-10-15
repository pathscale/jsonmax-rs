//! Portable, checked bindings to the canonical safe scanner entry points.
//!
//! Scalar construction needs no detection. Native resolution checks current
//! availability and never substitutes a different backend for an explicit request.
//! The scanner entry points independently guard any native instructions.

#![forbid(unsafe_code)]

use super::{detect, unavailable_backend, BackendId, ClassifyFn, Dispatch, Result};

/// Returns the always-available scalar classifier without probing hardware.
///
/// The returned dispatch accepts blocks of at most 64 bytes and passes scanner
/// masks, carry updates, and errors through without modification.
pub fn scalar() -> Dispatch {
    Dispatch {
        backend: BackendId::Scalar,
        classifier: jmax_scan_scalar::classify::classify,
    }
}

/// Resolves exactly the requested backend to its safe ABI classifier.
///
/// Every non-scalar request performs fresh capability detection, including OS
/// state and scanner-build support. This function has no size policy or cache.
/// An empty block may subsequently produce masks tagged Scalar even when the
/// dispatch records a native backend: the masks identify work actually done.
///
/// # Errors
///
/// An unavailable backend returns `ConstraintViolation` from `Scanner` at
/// offset zero. Callers that need a fallback can construct [`scalar`] explicitly.
pub fn resolve(backend: BackendId) -> Result<Dispatch> {
    if backend == BackendId::Scalar {
        return Ok(scalar());
    }
    if !detect::is_available(backend) {
        return Err(unavailable_backend());
    }

    // These module entry points are safe on every architecture. Do not bind
    // directly to their private routines that require target features.
    let classifier: ClassifyFn = match backend {
        BackendId::Scalar => jmax_scan_scalar::classify::classify,
        BackendId::Neon => jmax_scan_neon::classify::classify,
        BackendId::Avx2 => jmax_scan_avx2::classify::classify,
        BackendId::Avx512 => jmax_scan_avx512::classify::classify,
    };
    Ok(Dispatch { backend, classifier })
}
