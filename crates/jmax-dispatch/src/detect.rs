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

#[cfg(test)]
mod tests {
    use super::*;

    const BACKENDS: [BackendId; 4] = [
        BackendId::Scalar,
        BackendId::Neon,
        BackendId::Avx2,
        BackendId::Avx512,
    ];

    #[test]
    fn all_x86_feature_and_scanner_gate_combinations() {
        // Bits: AVX usable state, AVX2, F, BW, AVX2 scanner, AVX-512 scanner.
        // Includes all-false/all-true boundaries and deliberately inconsistent
        // inputs to ensure no missing OS or scanner gate can grant support.
        for bits in 0u8..64 {
            let caps = x86_capabilities(
                bits & 1 != 0,
                bits & 2 != 0,
                bits & 4 != 0,
                bits & 8 != 0,
                bits & 16 != 0,
                bits & 32 != 0,
            );
            assert_eq!(caps.neon, false, "vector {bits}");
            assert_eq!(caps.os_avx, bits & 1 != 0, "vector {bits}");
            assert_eq!(caps.os_avx512, bits & 4 != 0, "vector {bits}");
            assert_eq!(caps.avx2, bits & 18 == 18, "vector {bits}");
            assert_eq!(caps.avx512f, bits & 36 == 36, "vector {bits}");
            assert_eq!(caps.avx512bw, bits & 40 == 40, "vector {bits}");
            let expected = [true, false, bits & 19 == 19, bits & 45 == 45];
            for (backend, available) in BACKENDS.into_iter().zip(expected) {
                assert_eq!(caps.supports(backend), available, "vector {bits}: {backend:?}");
            }
        }
    }

    #[test]
    fn no_features_and_no_scanners_are_scalar_only() {
        let caps = x86_capabilities(false, false, false, false, false, false);
        assert_eq!(caps, CpuCapabilities::scalar_only());
        assert_eq!(caps, CpuCapabilities::default());
        for backend in BACKENDS {
            assert_eq!(caps.supports(backend), backend == BackendId::Scalar);
        }
    }

    #[test]
    fn missing_scanner_builds_preserve_os_state_but_reject_native_backends() {
        let caps = x86_capabilities(true, true, true, true, false, false);
        assert_eq!(caps, CpuCapabilities {
            os_avx: true,
            os_avx512: true,
            ..CpuCapabilities::scalar_only()
        });
        assert!(!caps.supports(BackendId::Avx2));
        assert!(!caps.supports(BackendId::Avx512));
        assert!(caps.supports(BackendId::Scalar));
    }

    #[test]
    fn scanner_support_cannot_supply_missing_cpu_features() {
        assert_eq!(
            x86_capabilities(false, false, false, false, true, true),
            CpuCapabilities::scalar_only(),
        );
    }

    #[test]
    fn missing_avx_os_state_rejects_advertised_features() {
        let caps = x86_capabilities(false, true, true, true, true, true);
        assert!(caps.avx2 && caps.avx512f && caps.avx512bw && caps.os_avx512);
        assert!(!caps.supports(BackendId::Avx2));
        assert!(!caps.supports(BackendId::Avx512));
    }

    #[test]
    fn avx512_requires_both_f_and_bw_but_not_avx2_or_vl() {
        let f_only = x86_capabilities(true, false, true, false, false, true);
        assert!(f_only.avx512f && f_only.os_avx512);
        assert!(!f_only.supports(BackendId::Avx512));

        let bw_only = x86_capabilities(true, false, false, true, false, true);
        assert!(bw_only.avx512bw);
        assert!(!bw_only.os_avx512);
        assert!(!bw_only.supports(BackendId::Avx512));

        // No VL input or AVX2 scanner is needed for the F+BW backend.
        let both = x86_capabilities(true, false, true, true, false, true);
        assert!(both.supports(BackendId::Avx512));
        assert!(!both.supports(BackendId::Avx2));
    }

    #[test]
    fn scanner_gates_are_independent() {
        let avx2_only = x86_capabilities(true, true, true, true, true, false);
        assert!(avx2_only.supports(BackendId::Avx2));
        assert!(!avx2_only.supports(BackendId::Avx512));

        let avx512_only = x86_capabilities(true, true, true, true, false, true);
        assert!(!avx512_only.supports(BackendId::Avx2));
        assert!(avx512_only.supports(BackendId::Avx512));
    }

    #[test]
    fn public_detection_and_availability_agree() {
        let caps = detect();
        for backend in BACKENDS {
            assert_eq!(is_available(backend), caps.supports(backend), "{backend:?}");
        }
        assert!(is_available(BackendId::Scalar));
        assert!(!caps.neon || jmax_scan_neon::is_supported());
        assert!(!caps.avx2 || jmax_scan_avx2::is_supported());
        assert!(!caps.avx512f || jmax_scan_avx512::is_supported());
        assert!(!caps.avx512bw || jmax_scan_avx512::is_supported());
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn std_x86_matches_usable_state_and_dependency_intersection() {
        let caps = detect();
        assert!(!caps.neon);
        assert_eq!(caps.os_avx, std::is_x86_feature_detected!("avx"));
        assert_eq!(caps.os_avx512, std::is_x86_feature_detected!("avx512f"));
        assert_eq!(
            caps.avx2,
            std::is_x86_feature_detected!("avx2") && jmax_scan_avx2::is_supported(),
        );
        assert_eq!(
            caps.avx512f,
            std::is_x86_feature_detected!("avx512f") && jmax_scan_avx512::is_supported(),
        );
        assert_eq!(
            caps.avx512bw,
            std::is_x86_feature_detected!("avx512bw") && jmax_scan_avx512::is_supported(),
        );
    }

    #[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn no_std_x86_uses_only_static_guarantees_and_dependency_support() {
        let caps = detect();
        let avx_state = cfg!(target_feature = "avx")
            || cfg!(target_feature = "avx2")
            || cfg!(target_feature = "avx512f");
        assert!(!caps.neon);
        assert_eq!(caps.os_avx, avx_state);
        assert_eq!(caps.os_avx512, cfg!(target_feature = "avx512f"));
        assert_eq!(
            caps.avx2,
            cfg!(target_feature = "avx2") && jmax_scan_avx2::is_supported(),
        );
        assert_eq!(
            caps.avx512f,
            cfg!(target_feature = "avx512f") && jmax_scan_avx512::is_supported(),
        );
        assert_eq!(
            caps.avx512bw,
            cfg!(target_feature = "avx512bw") && jmax_scan_avx512::is_supported(),
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_reports_only_scanner_neon_support() {
        assert_eq!(detect(), CpuCapabilities {
            neon: jmax_scan_neon::is_supported(),
            ..CpuCapabilities::scalar_only()
        });
        assert!(!is_available(BackendId::Avx2));
        assert!(!is_available(BackendId::Avx512));
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    #[test]
    fn unsupported_targets_including_arm32_are_scalar_only() {
        assert_eq!(detect(), CpuCapabilities::scalar_only());
        for backend in BACKENDS {
            assert_eq!(is_available(backend), backend == BackendId::Scalar);
        }
    }
}
