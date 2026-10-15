//! Explicit scanner overrides and allocation-free selection reports.
//!
//! Parsing is independent of hardware. Selection uses fresh effective
//! capabilities and the checked portable resolver, without process-wide state.

#![forbid(unsafe_code)]

use super::{
    choose, detect, portable, unavailable_backend, BackendId, BackendOverride,
    CpuCapabilities, Dispatch, DispatchPolicy, DispatchReport, Error, ErrorCode,
    ErrorSource, Result,
};

/// Parses one exact, lowercase override spelling without inspecting hardware.
///
/// Accepted values are `auto`, `scalar`, `neon`, `avx2`, `avx512`, and each
/// backend name prefixed with `prefer:`. Bare backend names require that backend.
/// Whitespace, case variations, and aliases are not accepted.
///
/// # Errors
///
/// Any other value returns `InvalidToken` from `Scanner` at offset zero. The
/// diagnostic is static and never includes bytes from the supplied value.
pub fn parse(value: &str) -> Result<BackendOverride> {
    match value {
        "auto" => Ok(BackendOverride::Auto),
        "scalar" => Ok(BackendOverride::Require(BackendId::Scalar)),
        "neon" => Ok(BackendOverride::Require(BackendId::Neon)),
        "avx2" => Ok(BackendOverride::Require(BackendId::Avx2)),
        "avx512" => Ok(BackendOverride::Require(BackendId::Avx512)),
        "prefer:scalar" => Ok(BackendOverride::Prefer(BackendId::Scalar)),
        "prefer:neon" => Ok(BackendOverride::Prefer(BackendId::Neon)),
        "prefer:avx2" => Ok(BackendOverride::Prefer(BackendId::Avx2)),
        "prefer:avx512" => Ok(BackendOverride::Prefer(BackendId::Avx512)),
        _ => Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "invalid scanner backend override",
        )),
    }
}

/// Resolves an override and reports the request, policy, and effective selection.
///
/// `input_len` is a whole-input hint and can exceed the scanner's 64-byte block
/// limit. Supported explicit requests bypass all size thresholds and the
/// `allow_avx512` policy flag, while retaining the resolver's safety checks.
/// An unavailable preferred backend falls back to automatic policy selection.
///
/// If native resolution fails after the initial capability snapshot, automatic
/// and preferred requests resolve scalar and refresh the reported capabilities.
/// `used_fallback` is true only when a preferred backend could not be resolved;
/// automatic selection always reports it as false, including scalar recovery.
/// The report's selected backend describes the dispatch, not the tag of a later
/// block classification (an empty block can carry a Scalar tag).
///
/// # Errors
///
/// An unavailable required backend returns `ConstraintViolation` from `Scanner`
/// at offset zero. A required backend's resolution error is returned unchanged.
pub fn select(
    requested: BackendOverride,
    input_len: usize,
    policy: DispatchPolicy,
) -> Result<(Dispatch, DispatchReport)> {
    select_with(requested, input_len, policy, detect::detect, portable::resolve)
}

// Dependency injection is private so public callers cannot use synthetic
// capabilities to authorize native execution. It also lets tests exercise a
// change in availability without mutable globals or architecture assumptions.
fn select_with(
    requested: BackendOverride,
    input_len: usize,
    policy: DispatchPolicy,
    mut capabilities_now: impl FnMut() -> CpuCapabilities,
    mut resolve: impl FnMut(BackendId) -> Result<Dispatch>,
) -> Result<(Dispatch, DispatchReport)> {
    let mut capabilities = capabilities_now();
    let (candidate, mut used_fallback) = match requested {
        BackendOverride::Auto => (choose::choose(input_len, capabilities, policy), false),
        BackendOverride::Require(backend) => {
            if !capabilities.supports(backend) {
                return Err(unavailable_backend());
            }
            (backend, false)
        }
        BackendOverride::Prefer(backend) => {
            if capabilities.supports(backend) {
                (backend, false)
            } else {
                (choose::choose(input_len, capabilities, policy), true)
            }
        }
    };

    let dispatch = match resolve(candidate) {
        Ok(dispatch) => dispatch,
        Err(error) => {
            if candidate == BackendId::Scalar
                || matches!(requested, BackendOverride::Require(_))
            {
                return Err(error);
            }
            // Do not retry another native candidate using the stale snapshot.
            // Scalar resolution cannot fail in the production resolver.
            capabilities = capabilities_now();
            used_fallback = matches!(requested, BackendOverride::Prefer(_));
            resolve(BackendId::Scalar)?
        }
    };

    let report = DispatchReport {
        requested,
        selected: dispatch.backend(),
        capabilities,
        policy,
        input_len,
        used_fallback,
    };
    Ok((dispatch, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockMasks, ClassifyFn, ScanCarry};

    const SPELLINGS: [(&str, BackendOverride); 9] = [
        ("auto", BackendOverride::Auto),
        ("scalar", BackendOverride::Require(BackendId::Scalar)),
        ("neon", BackendOverride::Require(BackendId::Neon)),
        ("avx2", BackendOverride::Require(BackendId::Avx2)),
        ("avx512", BackendOverride::Require(BackendId::Avx512)),
        ("prefer:scalar", BackendOverride::Prefer(BackendId::Scalar)),
        ("prefer:neon", BackendOverride::Prefer(BackendId::Neon)),
        ("prefer:avx2", BackendOverride::Prefer(BackendId::Avx2)),
        ("prefer:avx512", BackendOverride::Prefer(BackendId::Avx512)),
    ];

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

    const BLOCK_NATIVE: DispatchPolicy = DispatchPolicy {
        neon_min_len: usize::MAX,
        avx2_min_len: usize::MAX,
        avx512_min_len: usize::MAX,
        allow_avx512: false,
    };

    fn carry(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            utf8_tail: [0xf0, 0x9f, 0xff, 0x80],
            utf8_tail_len: 255,
        }
    }

    fn resolution_error() -> Error {
        // Distinct metadata verifies that Require preserves the resolver error.
        Error::new(
            ErrorCode::ConstraintViolation, ErrorSource::Scanner, 17,
            "test resolution rejected",
        ).with_schema(23, 29)
    }

    fn assert_invalid(value: &str) {
        assert_eq!(parse(value).unwrap_err(), Error::new(
            ErrorCode::InvalidToken, ErrorSource::Scanner, 0,
            "invalid scanner backend override",
        ), "value={value:?}");
    }

    // Independent policy oracle uses raw capability fields rather than either
    // the production chooser or CpuCapabilities::supports.
    fn supported(caps: CpuCapabilities, backend: BackendId) -> bool {
        match backend {
            BackendId::Scalar => true,
            BackendId::Neon => caps.neon,
            BackendId::Avx2 => caps.avx2 && caps.os_avx,
            BackendId::Avx512 => caps.avx512f && caps.avx512bw && caps.os_avx && caps.os_avx512,
        }
    }

    fn automatic(caps: CpuCapabilities, len: usize, policy: DispatchPolicy) -> BackendId {
        let mut selected = BackendId::Scalar;
        for (backend, minimum, enabled) in [
            (BackendId::Neon, policy.neon_min_len, true),
            (BackendId::Avx2, policy.avx2_min_len, true),
            (BackendId::Avx512, policy.avx512_min_len, policy.allow_avx512),
        ] {
            if enabled && supported(caps, backend) && len >= minimum {
                selected = backend;
            }
        }
        selected
    }

    #[test]
    fn exact_spellings_parse_independently_of_hardware() {
        for (value, expected) in SPELLINGS {
            assert_eq!(parse(value).unwrap(), expected);
        }
        assert_eq!(parse("auto").unwrap(), BackendOverride::default());
    }

    #[test]
    fn malformed_aliases_whitespace_unicode_and_payloads_are_rejected() {
        for value in [
            "", " ", "\t", "\n", "AUTO", "Auto", "Scalar", "NEON", "AVX2", "AVX512",
            " auto", "auto ", "auto\n", "\tauto", "scalar\r\n", "prefer:avx2 ",
            "prefer: avx2", "prefer :avx2", "Prefer:avx2", "prefer:AVX2", "prefer:",
            "prefer", "prefer:auto", "prefer:prefer:scalar", "require:scalar",
            "require:avx512", "auto:scalar", "avx", "avx-2", "avx-512", "avx512f",
            "avx512bw", "sse2", "native", "default", "none", "0", "scalar,neon",
            "auto\0", "\0scalar", "prefer:\0avx2", "avx2/../../secret", "{avx2}",
            "\u{feff}auto", "auto\u{a0}", "\u{200b}scalar", "ａｕｔｏ", "🦀",
        ] {
            assert_invalid(value);
        }
        let long = [b'x'; 4096];
        assert_invalid(core::str::from_utf8(&long).unwrap());
    }

    #[test]
    fn every_ascii_single_byte_mutation_and_truncation_is_exact() {
        for (value, _) in SPELLINGS {
            for end in 0..value.len() {
                assert_invalid(&value[..end]);
            }
            let mut bytes = [0u8; 14];
            bytes[..value.len()].copy_from_slice(value.as_bytes());
            for index in 0..value.len() {
                let original = bytes[index];
                for replacement in 0u8..=127 {
                    if replacement == original {
                        continue;
                    }
                    bytes[index] = replacement;
                    let mutated = core::str::from_utf8(&bytes[..value.len()]).unwrap();
                    match SPELLINGS.iter().find(|(spelling, _)| *spelling == mutated) {
                        Some((_, expected)) => assert_eq!(parse(mutated).unwrap(), *expected),
                        None => assert_invalid(mutated),
                    }
                }
                bytes[index] = original;
            }
            for suffix in 0u8..=127 {
                bytes[value.len()] = suffix;
                assert_invalid(core::str::from_utf8(&bytes[..value.len() + 1]).unwrap());
            }
        }
    }

    #[test]
    fn exhaustive_capabilities_requests_and_policy_boundaries() {
        let policies = [
            DispatchPolicy::conservative(), ZERO, BLOCK_NATIVE,
            DispatchPolicy {
                neon_min_len: 256, avx2_min_len: 64, avx512_min_len: 1,
                allow_avx512: true,
            },
        ];
        let lengths = [0, 1, 63, 64, 65, 255, 256, 257, 4095, 4096, 4097,
            usize::MAX - 1, usize::MAX];
        for bits in 0u8..64 {
            let caps = CpuCapabilities {
                neon: bits & 1 != 0,
                avx2: bits & 2 != 0,
                avx512f: bits & 4 != 0,
                avx512bw: bits & 8 != 0,
                os_avx: bits & 16 != 0,
                os_avx512: bits & 32 != 0,
            };
            for policy in policies {
                for input_len in lengths {
                    for (_, requested) in SPELLINGS {
                        let auto = automatic(caps, input_len, policy);
                        let (candidate, missing_required, preferred_missing) = match requested {
                            BackendOverride::Auto => (auto, false, false),
                            BackendOverride::Require(id) => (id, !supported(caps, id), false),
                            BackendOverride::Prefer(id) => {
                                let missing = !supported(caps, id);
                                (if missing { auto } else { id }, false, missing)
                            }
                        };
                        let mut detections = 0;
                        let mut attempts = [None; 2];
                        let mut resolutions = 0;
                        let result = select_with(requested, input_len, policy, || {
                            detections += 1;
                            if detections == 1 { caps } else { CpuCapabilities::scalar_only() }
                        }, |id| {
                            attempts[resolutions] = Some(id);
                            resolutions += 1;
                            // Synthetic support never authorizes a native call.
                            // Only real scalar dispatches are returned by this resolver.
                            if id == BackendId::Scalar {
                                Ok(portable::scalar())
                            } else {
                                Err(resolution_error())
                            }
                        });
                        if missing_required {
                            assert_eq!(result.unwrap_err(), unavailable_backend());
                            assert_eq!(attempts, [None, None]);
                            assert_eq!(detections, 1);
                        } else if matches!(requested, BackendOverride::Require(_))
                            && candidate != BackendId::Scalar
                        {
                            assert_eq!(result.unwrap_err(), resolution_error());
                            assert_eq!(attempts, [Some(candidate), None]);
                            assert_eq!(detections, 1);
                        } else {
                            let (dispatch, report) = result.unwrap();
                            let recovered = candidate != BackendId::Scalar;
                            assert_eq!(dispatch.backend(), BackendId::Scalar);
                            assert_eq!(attempts, [Some(candidate),
                                if recovered { Some(BackendId::Scalar) } else { None }]);
                            assert_eq!(detections, if recovered { 2 } else { 1 });
                            assert_eq!(report, DispatchReport {
                                requested,
                                selected: dispatch.backend(),
                                capabilities: if recovered {
                                    CpuCapabilities::scalar_only()
                                } else {
                                    caps
                                },
                                policy,
                                input_len,
                                used_fallback: preferred_missing
                                    || (recovered && matches!(requested, BackendOverride::Prefer(_))),
                            });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn resolution_failure_refreshes_report_without_retrying_native_backends() {
        for requested in [BackendOverride::Auto, BackendOverride::Prefer(BackendId::Avx512)] {
            for refreshed in [ALL, CpuCapabilities { avx512bw: false, ..ALL }] {
                let mut detections = 0;
                let mut resolutions = 0;
                let (dispatch, report) = select_with(requested, usize::MAX, ZERO, || {
                    detections += 1;
                    if detections == 1 { ALL } else { refreshed }
                }, |id| {
                    resolutions += 1;
                    if resolutions == 1 {
                        assert_eq!(id, BackendId::Avx512);
                        Err(unavailable_backend())
                    } else {
                        assert_eq!(resolutions, 2);
                        assert_eq!(id, BackendId::Scalar);
                        portable::resolve(id)
                    }
                }).unwrap();
                assert_eq!(detections, 2);
                assert_eq!(resolutions, 2);
                assert_eq!(dispatch.backend(), BackendId::Scalar);
                assert_eq!(report, DispatchReport {
                    requested, selected: BackendId::Scalar, capabilities: refreshed,
                    policy: ZERO, input_len: usize::MAX,
                    used_fallback: matches!(requested, BackendOverride::Prefer(_)),
                });
            }
        }
    }

    #[test]
    fn scalar_resolution_errors_are_not_retried_or_rewritten() {
        for requested in [
            BackendOverride::Auto,
            BackendOverride::Require(BackendId::Scalar),
            BackendOverride::Prefer(BackendId::Scalar),
            BackendOverride::Prefer(BackendId::Neon),
        ] {
            let mut detections = 0;
            let mut resolutions = 0;
            let error = select_with(requested, 0, ZERO, || {
                detections += 1;
                CpuCapabilities::scalar_only()
            }, |id| {
                resolutions += 1;
                assert_eq!(id, BackendId::Scalar);
                Err(resolution_error())
            }).unwrap_err();
            assert_eq!(error, resolution_error());
            assert_eq!(detections, 1);
            assert_eq!(resolutions, 1);
        }
    }

    #[test]
    fn public_selection_reports_match_actual_capabilities_and_policy() {
        let mut unavailable_requests = 0;
        for policy in [DispatchPolicy::conservative(), ZERO, BLOCK_NATIVE] {
            for input_len in [0, 63, 64, 65, 255, 256, 257, 4095, 4096, 4097, usize::MAX] {
                for (spelling, requested) in SPELLINGS {
                    let caps = detect::detect();
                    let result = select(parse(spelling).unwrap(), input_len, policy);
                    if let BackendOverride::Require(id) = requested {
                        if !supported(caps, id) {
                            assert_eq!(result.unwrap_err(), unavailable_backend());
                            unavailable_requests += 1;
                            continue;
                        }
                    }
                    let (dispatch, report) = result.unwrap();
                    let (expected, fallback) = match requested {
                        BackendOverride::Auto => (automatic(caps, input_len, policy), false),
                        BackendOverride::Require(id) => (id, false),
                        BackendOverride::Prefer(id) => {
                            if supported(caps, id) { (id, false) }
                            else { (automatic(caps, input_len, policy), true) }
                        }
                    };
                    assert_eq!(dispatch.backend(), expected);
                    assert_eq!(report, DispatchReport {
                        requested, selected: expected, capabilities: caps, policy, input_len,
                        used_fallback: fallback,
                    });
                    if requested == BackendOverride::Auto {
                        assert_eq!(crate::dispatch(input_len, policy).unwrap().backend(), expected);
                    }
                }
            }
        }
        // Current supported targets cannot provide both NEON and x86 backends.
        assert!(unavailable_requests > 0);
    }

    fn for_each_selected(mut test: impl FnMut(Dispatch, DispatchReport)) {
        for (spelling, requested) in SPELLINGS {
            match select(parse(spelling).unwrap(), usize::MAX, DispatchPolicy::conservative()) {
                Ok((dispatch, report)) => {
                    assert_eq!(report.requested, requested);
                    assert_eq!(report.selected, dispatch.backend());
                    test(dispatch, report);
                }
                Err(error) => {
                    assert!(matches!(requested, BackendOverride::Require(_)));
                    assert_eq!(error, unavailable_backend());
                }
            }
        }
    }

    #[test]
    fn parsed_selections_preserve_every_explicit_mask_field() {
        let input = [
            b'"', b'a', b':', b'\\', b'"', b'"', b',', b'{', b'}',
            b'[', b']', b' ', b'\t', b'\n', b'0', 0xff, 0,
        ];
        for_each_selected(|dispatch, report| {
            let expected = BlockMasks {
                quote_mask: 0x31, backslash_mask: 0x08, in_string_mask: 0x1e,
                colon_mask: 0x04, comma_mask: 0x40, lbrace_mask: 0x80,
                rbrace_mask: 0x100, lbracket_mask: 0x200, rbracket_mask: 0x400,
                structural_mask: 0x7c0, whitespace_mask: 0x3800, control_mask: 0x13000,
                digit_mask: 0x4000, non_ascii_mask: 0x8000, valid_mask: 0x1ffff,
                valid_len: 17, backend: report.selected as u8,
            };
            let mut state = carry(0, 0);
            assert_eq!(dispatch.classify(&input, &mut state).unwrap(), expected);
            assert_eq!(state, carry(0, 0));
        });
    }

    #[test]
    fn selected_classifiers_match_scalar_at_every_block_length_and_carry() {
        let pattern = [b'{', b'"', b':', b'\\', b'"', b'"', b',', b'9', b'\n', 0xff, 0];
        let mut input = [0u8; 64];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for_each_selected(|dispatch, report| {
            for len in 0..=64 {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        let initial = carry(inside, escaped);
                        let mut expected_carry = initial;
                        let mut expected = portable::scalar()
                            .classify(&input[..len], &mut expected_carry).unwrap();
                        expected.backend = if len == 0 {
                            BackendId::Scalar as u8
                        } else {
                            report.selected as u8
                        };
                        let mut state = initial;
                        assert_eq!(dispatch.classify(&input[..len], &mut state).unwrap(), expected);
                        assert_eq!(state, expected_carry);
                        let classifier: ClassifyFn = dispatch.classifier();
                        let mut pointer_state = initial;
                        assert_eq!(classifier(&input[..len], &mut pointer_state).unwrap(), expected);
                        assert_eq!(pointer_state, expected_carry);
                        assert_eq!(dispatch.backend(), report.selected);
                    }
                }
            }
        });
    }

    #[test]
    fn empty_and_full_width_blocks_keep_report_identity_and_high_bits() {
        for_each_selected(|dispatch, report| {
            let mut state = carry(0, 0);
            assert_eq!(dispatch.classify(&[], &mut state).unwrap(), BlockMasks::default());
            assert_eq!(state, carry(0, 0));
            let mut input = [b'x'; 64];
            input[0] = b'0';
            input[63] = b':';
            for len in [1, 15, 16, 17, 31, 32, 33, 63, 64] {
                let high = if len == 64 { 1u64 << 63 } else { 0 };
                assert_eq!(dispatch.classify(&input[..len], &mut state).unwrap(), BlockMasks {
                    digit_mask: 1, colon_mask: high, structural_mask: high,
                    valid_mask: if len == 64 { u64::MAX } else { (1u64 << len) - 1 },
                    valid_len: len as u8, backend: report.selected as u8,
                    ..BlockMasks::default()
                });
                assert_eq!(state, carry(0, 0));
            }
            assert_eq!(dispatch.backend(), report.selected);
            assert_eq!(report.input_len, usize::MAX);
        });
    }

    #[test]
    fn quote_carry_survives_empty_blocks_and_copied_dispatches() {
        for_each_selected(|dispatch, report| {
            let copy = dispatch;
            let mut state = carry(0, 0);
            let mut first = [b'x'; 64];
            first[0] = b'"';
            first[63] = b'\\';
            let masks = dispatch.classify(&first, &mut state).unwrap();
            assert_eq!(masks.quote_mask, 1);
            assert_eq!(masks.backslash_mask, 1u64 << 63);
            assert_eq!(masks.in_string_mask, u64::MAX ^ 1);
            assert_eq!(state, carry(1, 1));
            assert_eq!(copy.classify(&[], &mut state).unwrap(), BlockMasks::default());
            assert_eq!(state, carry(1, 1));
            let masks = copy.classify(b"\":\",", &mut state).unwrap();
            assert_eq!(masks.quote_mask, 0b0101);
            assert_eq!(masks.in_string_mask, 0b0011);
            assert_eq!(masks.colon_mask, 0b0010);
            assert_eq!(masks.comma_mask, 0b1000);
            assert_eq!(masks.structural_mask, 0b1000);
            assert_eq!(state, carry(0, 0));
            assert_eq!(copy.backend(), report.selected);
        });
    }

    #[test]
    fn classification_errors_pass_through_without_changing_carry_or_selection() {
        let input = [b'"'; 257];
        for_each_selected(|dispatch, report| {
            let direct = portable::resolve(report.selected).unwrap();
            for len in [0, 1, 63, 64, 65, 255, 256, 257] {
                for initial in [carry(0, 0), carry(1, 1), carry(2, 0),
                    carry(0, 2), carry(u64::MAX, u64::MAX)]
                {
                    if len <= 64 && initial.prev_in_string <= 1 && initial.prev_escaped <= 1 {
                        continue;
                    }
                    let mut direct_state = initial;
                    let expected = direct.classify(&input[..len], &mut direct_state).unwrap_err();
                    assert_eq!(expected.code, if len > 64 {
                        ErrorCode::InvalidToken
                    } else {
                        ErrorCode::InternalError
                    });
                    assert_eq!(expected.source, ErrorSource::Scanner);
                    assert_eq!(expected.byte_offset, 0);
                    assert_eq!(direct_state, initial);
                    let mut state = initial;
                    assert_eq!(dispatch.classify(&input[..len], &mut state).unwrap_err(), expected);
                    assert_eq!(state, initial);
                    let mut pointer_state = initial;
                    assert_eq!((dispatch.classifier())(&input[..len], &mut pointer_state)
                        .unwrap_err(), expected);
                    assert_eq!(pointer_state, initial);
                    assert_eq!(dispatch.backend(), report.selected);
                }
            }
            let mut state = carry(0, 0);
            assert_eq!(dispatch.classify(b"[]", &mut state).unwrap().structural_mask, 3);
            assert_eq!(state, carry(0, 0));
        });
    }

    #[test]
    fn repeated_explicit_requests_do_not_change_automatic_selection() {
        let policy = DispatchPolicy::conservative();
        let (_, before) = select(BackendOverride::Auto, usize::MAX, policy).unwrap();
        for _ in 0..3 {
            for (spelling, _) in SPELLINGS {
                let requested = parse(spelling).unwrap();
                match select(requested, 0, BLOCK_NATIVE) {
                    Ok((dispatch, report)) => assert_eq!(dispatch.backend(), report.selected),
                    Err(error) => {
                        assert!(matches!(requested, BackendOverride::Require(_)));
                        assert_eq!(error, unavailable_backend());
                    }
                }
                let (_, after) = select(BackendOverride::Auto, usize::MAX, policy).unwrap();
                assert_eq!(after, before);
            }
        }
    }
}
