//! CPU and OS capabilities intersected with the available scanner builds.
//!
//! Detection has no dispatch policy, allocation, environment access, or local
//! cache. With `std`, x86 feature checks include OS extended-register support.
//! Without `std`, explicit target features are the CPU and OS deployment
//! contract. No instructions are executed speculatively to test for a trap.

use super::{BackendId, CpuCapabilities};

/// Returns the capabilities usable by this target and its scanner dependencies.
///
/// A scanner can have fewer features enabled than this crate. Its public
/// `is_supported()` check is therefore required even after CPU/OS detection.
/// OS state fields describe the target independently of scanner availability.
/// Unsupported architectures, including 32-bit ARM, report scalar-only support.
pub fn detect() -> CpuCapabilities {
    detect_target()
}

/// Reports whether a backend is usable according to fresh capability detection.
///
/// Scalar is always available. This function does not resolve a classifier or
/// execute scanner instructions, and does not retain a capability snapshot.
pub fn is_available(backend: BackendId) -> bool {
    match backend {
        BackendId::Scalar => true,
        _ => detect().supports(backend),
    }
}

#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
fn detect_target() -> CpuCapabilities {
    // These are usable-state checks, not raw CPUID advertising bits. In
    // particular, the AVX-512F check includes the required OS register state.
    x86_capabilities(
        std::is_x86_feature_detected!("avx"),
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("avx512f"),
        std::is_x86_feature_detected!("avx512bw"),
        jmax_scan_avx2::is_supported(),
        jmax_scan_avx512::is_supported(),
    )
}

#[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
fn detect_target() -> CpuCapabilities {
    // Stronger explicitly enabled features also guarantee AVX OS state. BW
    // alone is deliberately not used to infer that guarantee.
    x86_capabilities(
        cfg!(any(
            target_feature = "avx",
            target_feature = "avx2",
            target_feature = "avx512f",
        )),
        cfg!(target_feature = "avx2"),
        cfg!(target_feature = "avx512f"),
        cfg!(target_feature = "avx512bw"),
        jmax_scan_avx2::is_supported(),
        jmax_scan_avx512::is_supported(),
    )
}

#[cfg(target_arch = "aarch64")]
fn detect_target() -> CpuCapabilities {
    // The scanner owns its runtime versus compile-time NEON detection choice.
    CpuCapabilities {
        neon: jmax_scan_neon::is_supported(),
        ..CpuCapabilities::scalar_only()
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn detect_target() -> CpuCapabilities {
    CpuCapabilities::scalar_only()
}

// Kept pure so every combination of usable-state and scanner-build gates can
// be exercised without requiring specific hardware or executing a classifier.
#[cfg(any(target_arch = "x86", target_arch = "x86_64", test))]
const fn x86_capabilities(
    avx: bool,
    avx2: bool,
    avx512f: bool,
    avx512bw: bool,
    scanner_avx2: bool,
    scanner_avx512: bool,
) -> CpuCapabilities {
    CpuCapabilities {
        neon: false,
        avx2: avx2 && scanner_avx2,
        avx512f: avx512f && scanner_avx512,
        avx512bw: avx512bw && scanner_avx512,
        os_avx: avx,
        os_avx512: avx512f,
    }
}
