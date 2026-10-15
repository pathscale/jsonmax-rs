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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockMasks, Error, ErrorCode, ErrorSource, ScanCarry};

    const BACKENDS: [BackendId; 4] = [
        BackendId::Scalar,
        BackendId::Neon,
        BackendId::Avx2,
        BackendId::Avx512,
    ];

    fn carry(inside: u64, escaped: u64) -> ScanCarry {
        ScanCarry {
            prev_in_string: inside,
            prev_escaped: escaped,
            // UTF-8 state belongs to another layer, even with this invalid length.
            utf8_tail: [0xf0, 0x9f, 0xff, 0x80],
            utf8_tail_len: 255,
        }
    }

    fn direct_classifier(backend: BackendId) -> ClassifyFn {
        match backend {
            BackendId::Scalar => jmax_scan_scalar::classify::classify,
            BackendId::Neon => jmax_scan_neon::classify::classify,
            BackendId::Avx2 => jmax_scan_avx2::classify::classify,
            BackendId::Avx512 => jmax_scan_avx512::classify::classify,
        }
    }

    fn for_each_available(mut test: impl FnMut(Dispatch)) {
        for backend in BACKENDS {
            if detect::is_available(backend) {
                let dispatch = resolve(backend).expect("available backend must resolve");
                assert_eq!(dispatch.backend(), backend);
                test(dispatch);
            }
        }
    }

    fn assert_scanner_error(error: &Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Scanner);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn assert_matches_scalar(dispatch: Dispatch, input: &[u8], initial: ScanCarry) {
        let mut expected_carry = initial;
        let mut expected = jmax_scan_scalar::classify::classify(input, &mut expected_carry)
            .expect("valid block and quote carry");
        expected.backend = if input.is_empty() {
            BackendId::Scalar as u8
        } else {
            dispatch.backend() as u8
        };
        let mut actual_carry = initial;
        let actual = dispatch.classify(input, &mut actual_carry).unwrap();
        assert_eq!(actual, expected, "backend={:?}, input={input:?}", dispatch.backend());
        assert_eq!(actual_carry, expected_carry);

        // Exercise the public ABI pointer independently of Dispatch::classify.
        // Behavioral comparison avoids unreliable function-address equality.
        let classifier: ClassifyFn = dispatch.classifier();
        let mut pointer_carry = initial;
        assert_eq!(classifier(input, &mut pointer_carry).unwrap(), actual);
        assert_eq!(pointer_carry, actual_carry);
    }

    fn assert_error_passthrough(
        dispatch: Dispatch,
        input: &[u8],
        initial: ScanCarry,
        code: ErrorCode,
    ) {
        let mut direct_carry = initial;
        let expected = direct_classifier(dispatch.backend())(input, &mut direct_carry)
            .unwrap_err();
        assert_scanner_error(&expected, code);
        assert_eq!(direct_carry, initial);

        let mut actual_carry = initial;
        let actual = dispatch.classify(input, &mut actual_carry).unwrap_err();
        assert_eq!(actual, expected);
        assert_eq!(actual_carry, initial);

        let mut pointer_carry = initial;
        let actual = (dispatch.classifier())(input, &mut pointer_carry).unwrap_err();
        assert_eq!(actual, expected);
        assert_eq!(pointer_carry, initial);
    }

    #[test]
    fn scalar_constructors_preserve_explicit_masks_and_opaque_utf8_state() {
        let input = [
            b'"', b'a', b':', b'\\', b'"', b'"', b',', b'{', b'}',
            b'[', b']', b' ', b'\t', b'\n', b'0', 0xff, 0,
        ];
        let expected = BlockMasks {
            quote_mask: 0x31,
            backslash_mask: 0x08,
            in_string_mask: 0x1e,
            colon_mask: 0x04,
            comma_mask: 0x40,
            lbrace_mask: 0x80,
            rbrace_mask: 0x100,
            lbracket_mask: 0x200,
            rbracket_mask: 0x400,
            structural_mask: 0x7c0,
            whitespace_mask: 0x3800,
            control_mask: 0x13000,
            digit_mask: 0x4000,
            non_ascii_mask: 0x8000,
            valid_mask: 0x1ffff,
            valid_len: 17,
            backend: BackendId::Scalar as u8,
        };
        for dispatch in [scalar(), resolve(BackendId::Scalar).unwrap()] {
            assert_eq!(dispatch.backend(), BackendId::Scalar);
            let mut state = carry(0, 0);
            assert_eq!(dispatch.classify(&input, &mut state).unwrap(), expected);
            assert_eq!(state, carry(0, 0));
        }
    }

    #[test]
    fn resolution_matches_detection_and_never_silently_falls_back() {
        let mut rejected = 0;
        for backend in BACKENDS {
            // Repeated requests must remain independent of earlier resolutions.
            for _ in 0..3 {
                let available = detect::is_available(backend);
                let result = resolve(backend);
                if available {
                    let dispatch = result.unwrap();
                    assert_eq!(dispatch.backend(), backend);
                    assert_matches_scalar(dispatch, b"{}", carry(0, 0));
                } else {
                    let error = result.unwrap_err();
                    assert_scanner_error(&error, ErrorCode::ConstraintViolation);
                    assert_eq!(error.detail, "requested scanner backend is unavailable");
                    rejected += 1;
                }
                assert_eq!(scalar().backend(), BackendId::Scalar);
            }
        }
        // No supported target exposes both NEON and x86 backends. Thus this
        // error-path test cannot pass without exercising an actual rejection.
        assert!(rejected > 0);
    }

    #[test]
    fn empty_blocks_preserve_all_carry_and_selected_backend() {
        for_each_available(|dispatch| {
            let selected = dispatch.backend();
            for inside in 0..=1 {
                for escaped in 0..=1 {
                    let initial = carry(inside, escaped);
                    let mut state = initial;
                    assert_eq!(dispatch.classify(&[], &mut state).unwrap(), BlockMasks::default());
                    assert_eq!(state, initial);
                    assert_eq!(dispatch.backend(), selected);
                }
            }
        });
    }

    #[test]
    fn every_block_length_and_quote_state_matches_scalar() {
        let pattern = [b'{', b'"', b':', b'\\', b'"', b'"', b',', b'9', b'\n', 0xff, 0];
        let mut input = [0; 64];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = pattern[index % pattern.len()];
        }
        for_each_available(|dispatch| {
            for len in 0..=64 {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_matches_scalar(dispatch, &input[..len], carry(inside, escaped));
                    }
                }
            }
        });
    }

    #[test]
    fn every_byte_value_is_classified_without_json_or_utf8_validation() {
        for_each_available(|dispatch| {
            for byte in 0u16..=255 {
                for inside in 0..=1 {
                    for escaped in 0..=1 {
                        assert_matches_scalar(dispatch, &[byte as u8], carry(inside, escaped));
                    }
                }
            }
        });
    }

    #[test]
    fn full_width_high_bit_and_partial_block_padding_are_preserved() {
        let mut input = [b'x'; 64];
        input[0] = b'0';
        input[63] = b':';
        for_each_available(|dispatch| {
            for len in [1, 15, 16, 17, 31, 32, 33, 63, 64] {
                let mut state = carry(0, 0);
                let masks = dispatch.classify(&input[..len], &mut state).unwrap();
                let high = if len == 64 { 1u64 << 63 } else { 0 };
                assert_eq!(masks, BlockMasks {
                    colon_mask: high,
                    structural_mask: high,
                    digit_mask: 1,
                    valid_mask: if len == 64 { u64::MAX } else { (1u64 << len) - 1 },
                    valid_len: len as u8,
                    backend: dispatch.backend() as u8,
                    ..BlockMasks::default()
                });
                assert_eq!(state, carry(0, 0));
            }
        });
    }

    #[test]
    fn quote_and_backslash_carry_survives_full_and_empty_blocks() {
        let mut first = [b'x'; 64];
        first[0] = b'"';
        first[63] = b'\\';
        for_each_available(|dispatch| {
            let mut state = carry(0, 0);
            let masks = dispatch.classify(&first, &mut state).unwrap();
            assert_eq!(masks.quote_mask, 1);
            assert_eq!(masks.backslash_mask, 1u64 << 63);
            assert_eq!(masks.in_string_mask, u64::MAX ^ 1);
            assert_eq!(state, carry(1, 1));

            assert_eq!(dispatch.classify(&[], &mut state).unwrap(), BlockMasks::default());
            assert_eq!(state, carry(1, 1));
            let masks = dispatch.classify(&[b'\\'; 64], &mut state).unwrap();
            assert_eq!(masks.backslash_mask, u64::MAX);
            assert_eq!(masks.in_string_mask, u64::MAX);
            assert_eq!(state, carry(1, 1));

            let masks = dispatch.classify(&[b'"', b':', b'"', b','], &mut state).unwrap();
            assert_eq!(masks.quote_mask, 0b0101);
            assert_eq!(masks.in_string_mask, 0b0011);
            assert_eq!(masks.colon_mask, 0b0010);
            assert_eq!(masks.comma_mask, 0b1000);
            assert_eq!(masks.structural_mask, 0b1000);
            assert_eq!(state, carry(0, 0));
        });
    }

    #[test]
    fn oversized_blocks_preserve_exact_errors_and_carry_before_narrowing() {
        let input = [b'"'; 1024];
        for_each_available(|dispatch| {
            for len in [65, 255, 256, 257, 1024] {
                // Length errors take precedence over invalid quote carry.
                for initial in [carry(0, 0), carry(1, 1), carry(u64::MAX, u64::MAX)] {
                    assert_error_passthrough(
                        dispatch, &input[..len], initial, ErrorCode::InvalidToken,
                    );
                }
            }
            // A failed classification does not poison subsequent use.
            assert_matches_scalar(dispatch, b"[]", carry(0, 0));
        });
    }

    #[test]
    fn invalid_quote_carry_preserves_exact_errors_even_for_empty_input() {
        let input = [b'"'; 64];
        for_each_available(|dispatch| {
            for bad in [2, 3, 255, 256, 1u64 << 63, u64::MAX] {
                for initial in [
                    carry(bad, 0), carry(0, bad), carry(bad, 1),
                    carry(1, bad), carry(bad, bad),
                ] {
                    for len in [0, 1, 63, 64] {
                        assert_error_passthrough(
                            dispatch, &input[..len], initial, ErrorCode::InternalError,
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn copied_dispatches_keep_interleaved_stream_state_independent() {
        let first = scalar();
        let second = first;
        let mut left = carry(0, 0);
        let mut right = carry(0, 0);
        first.classify(b"\"left\\", &mut left).unwrap();
        second.classify(b"[0,1]", &mut right).unwrap();
        assert_eq!(left, carry(1, 1));
        assert_eq!(right, carry(0, 0));
        let masks = second.classify(b"\":\",", &mut left).unwrap();
        assert_eq!(masks.in_string_mask, 0b0011);
        assert_eq!(masks.structural_mask, 0b1000);
        assert_eq!(left, right);
    }
}
