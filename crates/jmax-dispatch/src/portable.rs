//! Portable, checked bindings to the canonical safe scanner entry points.
//!
//! Scalar construction needs no detection. Native resolution checks current
//! availability and never substitutes a different backend for an explicit request.
//! Resolved bindings retain feature checks for all subsequent blocks.

#![forbid(unsafe_code)]

use super::{detect, unavailable_backend, BackendId, Dispatch, Result};

/// Returns the always-available scalar classifier without probing hardware.
///
/// The returned dispatch accepts blocks of at most 64 bytes and passes scanner
/// masks, carry updates, and errors through without modification.
pub fn scalar() -> Dispatch {
    Dispatch {
        backend: BackendId::Scalar,
        classifier: jmax_scan_scalar::classify::classify,
        block_classifier: jmax_scan_scalar::classify::classify_block,
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

    // Each scanner resolves private guarded bindings. Subsequent blocks keep
    // this CPU/OS capability invariant and avoid general-entry detection.
    let (classifier, block_classifier) = match backend {
        BackendId::Scalar => return Ok(scalar()),
        BackendId::Neon => jmax_scan_neon::classify::cached_classifiers(),
        BackendId::Avx2 => jmax_scan_avx2::classify::cached_classifiers(),
        BackendId::Avx512 => jmax_scan_avx512::classify::cached_classifiers(),
    }
    .ok_or_else(unavailable_backend)?;
    Ok(Dispatch {
        backend,
        classifier,
        block_classifier,
    })
}
