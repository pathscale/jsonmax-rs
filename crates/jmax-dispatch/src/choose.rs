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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: CpuCapabilities = CpuCapabilities {
        neon: true,
        avx2: true,
        avx512f: true,
        avx512bw: true,
        os_avx: true,
        os_avx512: true,
    };

    const ZERO: DispatchPolicy = DispatchPolicy {
        neon_min_len: 0,
        avx2_min_len: 0,
        avx512_min_len: 0,
        allow_avx512: true,
    };

    #[test]
    fn every_selection_branch_is_const_evaluable() {
        const POLICY: DispatchPolicy = DispatchPolicy::conservative();
        const SELECTED: [BackendId; 4] = [
            choose(0, ALL, POLICY),
            choose(64, ALL, POLICY),
            choose(256, ALL, POLICY),
            choose(4096, ALL, POLICY),
        ];
        assert_eq!(SELECTED, [
            BackendId::Scalar,
            BackendId::Neon,
            BackendId::Avx2,
            BackendId::Avx512,
        ]);
    }

    #[test]
    fn conservative_threshold_boundaries_are_inclusive() {
        let vectors = [
            (0, BackendId::Scalar),
            (1, BackendId::Scalar),
            (63, BackendId::Scalar),
            (64, BackendId::Neon),
            (65, BackendId::Neon),
            (255, BackendId::Neon),
            (256, BackendId::Avx2),
            (257, BackendId::Avx2),
            (4095, BackendId::Avx2),
            (4096, BackendId::Avx512),
            (4097, BackendId::Avx512),
            (usize::MAX, BackendId::Avx512),
        ];
        for (input_len, expected) in vectors {
            assert_eq!(
                choose(input_len, ALL, DispatchPolicy::conservative()),
                expected,
                "input_len={input_len}",
            );
        }
    }

    #[test]
    fn zero_thresholds_allow_empty_input_and_preserve_priority() {
        let vectors = [
            (ALL, BackendId::Avx512),
            (CpuCapabilities { avx512bw: false, ..ALL }, BackendId::Avx2),
            (CpuCapabilities { os_avx: false, ..ALL }, BackendId::Neon),
            (CpuCapabilities::scalar_only(), BackendId::Scalar),
        ];
        for (capabilities, expected) in vectors {
            assert_eq!(choose(0, capabilities, ZERO), expected);
        }
    }

    #[test]
    fn disabling_avx512_preserves_lower_priority_backends() {
        let policy = DispatchPolicy { allow_avx512: false, ..ZERO };
        let vectors = [
            (ALL, BackendId::Avx2),
            (CpuCapabilities { avx2: false, ..ALL }, BackendId::Neon),
            (CpuCapabilities { neon: false, avx2: false, ..ALL }, BackendId::Scalar),
        ];
        for (capabilities, expected) in vectors {
            for input_len in [0, 4096, usize::MAX] {
                assert_eq!(choose(input_len, capabilities, policy), expected);
            }
        }
    }

    #[test]
    fn missing_cpu_or_os_prerequisites_fall_back_without_errors() {
        // The frozen API has no error result. Rejected eligibility must select
        // the next eligible backend, including scalar when none remain.
        let vectors = [
            (CpuCapabilities { avx512f: false, ..ALL }, BackendId::Avx2),
            (CpuCapabilities { avx512bw: false, ..ALL }, BackendId::Avx2),
            (CpuCapabilities { os_avx512: false, ..ALL }, BackendId::Avx2),
            (CpuCapabilities { os_avx: false, ..ALL }, BackendId::Neon),
            (CpuCapabilities { os_avx: false, neon: false, ..ALL }, BackendId::Scalar),
            // AVX-512 does not require AVX2 or NEON, even in synthetic data.
            (CpuCapabilities { avx2: false, neon: false, ..ALL }, BackendId::Avx512),
            // OS state alone cannot supply missing CPU features.
            (CpuCapabilities {
                os_avx: true,
                os_avx512: true,
                ..CpuCapabilities::scalar_only()
            }, BackendId::Scalar),
        ];
        for (capabilities, expected) in vectors {
            assert_eq!(choose(usize::MAX, capabilities, ZERO), expected);
        }
    }

    #[test]
    fn thresholds_are_independent_and_need_not_be_monotonic() {
        let vectors = [
            (DispatchPolicy {
                neon_min_len: usize::MAX,
                avx2_min_len: usize::MAX,
                avx512_min_len: 7,
                allow_avx512: true,
            }, BackendId::Avx512),
            (DispatchPolicy {
                neon_min_len: usize::MAX,
                avx2_min_len: 7,
                avx512_min_len: usize::MAX,
                allow_avx512: true,
            }, BackendId::Avx2),
            (DispatchPolicy {
                neon_min_len: 7,
                avx2_min_len: usize::MAX,
                avx512_min_len: usize::MAX,
                allow_avx512: true,
            }, BackendId::Neon),
        ];
        for (policy, expected) in vectors {
            assert_eq!(choose(6, ALL, policy), BackendId::Scalar);
            assert_eq!(choose(7, ALL, policy), expected);
            assert_eq!(choose(8, ALL, policy), expected);
        }
    }

    #[test]
    fn maximum_thresholds_are_reachable_without_overflow() {
        let policy = DispatchPolicy {
            neon_min_len: usize::MAX,
            avx2_min_len: usize::MAX,
            avx512_min_len: usize::MAX,
            allow_avx512: true,
        };
        let vectors = [
            (ALL, BackendId::Avx512),
            (CpuCapabilities { avx512f: false, ..ALL }, BackendId::Avx2),
            (CpuCapabilities { os_avx: false, ..ALL }, BackendId::Neon),
            (CpuCapabilities::scalar_only(), BackendId::Scalar),
        ];
        for (capabilities, expected) in vectors {
            assert_eq!(choose(usize::MAX - 1, capabilities, policy), BackendId::Scalar);
            assert_eq!(choose(usize::MAX, capabilities, policy), expected);
        }
    }

    #[test]
    fn exhaustive_capabilities_and_independent_threshold_grid() {
        // 128,000 deterministic cases cover every combination of the six
        // capability bits, both AVX-512 permission values, and independent
        // thresholds including zero and the largest representable size.
        let thresholds = [0, 1, 64, 256, usize::MAX];
        let lengths = [0, 1, 63, 64, 65, 256, usize::MAX - 1, usize::MAX];
        for bits in 0u8..64 {
            let capabilities = CpuCapabilities {
                neon: bits & 1 != 0,
                avx2: bits & 2 != 0,
                avx512f: bits & 4 != 0,
                avx512bw: bits & 8 != 0,
                os_avx: bits & 16 != 0,
                os_avx512: bits & 32 != 0,
            };
            for allow_avx512 in [false, true] {
                for neon_min_len in thresholds {
                    for avx2_min_len in thresholds {
                        for avx512_min_len in thresholds {
                            let policy = DispatchPolicy {
                                neon_min_len,
                                avx2_min_len,
                                avx512_min_len,
                                allow_avx512,
                            };
                            for input_len in lengths {
                                // Independent oracle: walk from lowest to
                                // highest priority using raw capability bits,
                                // without calling CpuCapabilities::supports.
                                let candidates = [
                                    (BackendId::Neon, bits & 1 == 1, neon_min_len),
                                    (BackendId::Avx2, bits & 18 == 18, avx2_min_len),
                                    (BackendId::Avx512,
                                        bits & 60 == 60 && allow_avx512,
                                        avx512_min_len),
                                ];
                                let mut expected = BackendId::Scalar;
                                for (backend, supported, minimum) in candidates {
                                    if supported && minimum <= input_len {
                                        expected = backend;
                                    }
                                }
                                assert_eq!(
                                    choose(input_len, capabilities, policy),
                                    expected,
                                    "bits={bits}, input_len={input_len}, policy={policy:?}",
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
