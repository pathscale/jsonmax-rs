//! Adversarial checks at scanner block edges and strict-validation fallbacks.

use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::DuplicatePolicyOwner;
use jmax_syntax::{BlockCursor, Limits, Scratch, validate_borrowed};

fn scan_number(input: &[u8], start: usize, numeric: NumericPolicy) -> Option<(u32, u32)> {
    let mut cursor = BlockCursor::new(
        input,
        Limits::strict_default(),
        Some(numeric),
        DuplicatePolicyOwner::Syntax,
    )
    .ok()?;
    cursor.advance(start as u32);
    cursor
        .scan_number_span()
        .ok()
        .map(|span| (span.offset, span.len))
}

#[test]
fn number_scanner_matches_general_path_across_eight_and_sixteen_byte_edges() {
    let numeric = NumericPolicy::strict_default();
    let mut fast_count = 0;
    for start in [7usize, 8, 9, 15, 16, 17, 31, 32, 33] {
        for token in ["0", "12", "-12", "1.25e+3", "-0", "1e-2"] {
            let mut input = vec![b' '; start];
            input.extend_from_slice(token.as_bytes());
            input.push(b',');
            let mut cursor = BlockCursor::new(
                &input,
                Limits::strict_default(),
                Some(numeric),
                DuplicatePolicyOwner::Syntax,
            )
            .unwrap();
            cursor.advance(start as u32);
            let reference = cursor
                .scan_number_span()
                .map(|span| (span.offset, span.len));

            let mut fast_cursor = BlockCursor::new(
                &input,
                Limits::strict_default(),
                Some(numeric),
                DuplicatePolicyOwner::Syntax,
            )
            .unwrap();
            fast_cursor.advance(start as u32);
            if let Some(span) = fast_cursor.try_scan_number_span_fast() {
                fast_count += 1;
                assert_eq!(
                    (span.offset, span.len),
                    *reference.as_ref().unwrap(),
                    "{token} at {start}"
                );
            }
            assert_eq!(
                scan_number(&input, start, numeric),
                reference.ok(),
                "{token} at {start}"
            );
        }
    }
    assert!(fast_count > 0);
}

#[test]
fn string_fast_and_event_paths_agree_across_block_edges() {
    for start in [7usize, 8, 9, 15, 16, 17, 31, 32, 33] {
        for token in [br#""plain""#.as_slice(), br#""a\nb""#, br#""\u00e9""#] {
            let mut input = vec![b' '; start];
            input.extend_from_slice(token);
            let fast = validate_borrowed(
                &input,
                Limits::strict_default(),
                None,
                &mut Scratch::default(),
            )
            .err()
            .map(|error| (error.code, error.byte_offset));
            let mut cursor = BlockCursor::new(
                &input,
                Limits::strict_default(),
                None,
                DuplicatePolicyOwner::Syntax,
            )
            .unwrap();
            let mut decoded = Vec::new();
            let event = (|| {
                while let jmax_syntax::ScanStep::Emit(_) = cursor.next_event(&mut decoded)? {}
                cursor.finish_eof()
            })()
            .err()
            .map(|error| (error.code, error.byte_offset));
            assert_eq!(fast, event, "{token:?} at {start}");
            assert!(fast.is_none());
        }
    }
}

#[test]
fn strict_numbers_strings_duplicates_and_limits_reject_adversarial_inputs() {
    for raw in [
        &b"01"[..],
        b"-01",
        b"[00]",
        b"[1.]",
        b"[1e+]",
        b"[-0]",
        b"[1e999]",
        b"[\"\xff\"]",
        b"[\"\x01\"]",
        br#"["\q"]"#,
        br#"["\uD800"]"#,
        br#"[{"a":1,"\u0061":2}]"#,
        br#"[{"a":1,"b":2},{"x":0,"x":1}]"#,
    ] {
        assert!(
            validate_borrowed(
                raw,
                Limits::strict_default(),
                Some(NumericPolicy::strict_default()),
                &mut Scratch::default()
            )
            .is_err(),
            "{raw:?}"
        );
    }

    let mut limits = Limits::strict_default();
    limits.max_number_digits = 2;
    assert!(validate_borrowed(b"123", limits, None, &mut Scratch::default()).is_err());
    limits = Limits::strict_default();
    limits.max_string_bytes = 2;
    assert!(validate_borrowed(br#""abc""#, limits, None, &mut Scratch::default()).is_err());
    limits = Limits::strict_default();
    limits.max_decoded_bytes = 1;
    assert!(validate_borrowed(br#"["\n\t"]"#, limits, None, &mut Scratch::default()).is_err());
    limits = Limits::strict_default();
    limits.max_array_items = 1;
    assert!(validate_borrowed(b"[0,1]", limits, None, &mut Scratch::default()).is_err());
}

#[test]
fn negative_zero_policy_is_checked_at_block_boundaries() {
    let mut permissive = NumericPolicy::strict_default();
    permissive.reject_negative_zero = false;
    for start in [7usize, 8, 15, 16, 31, 32] {
        let mut input = vec![b' '; start];
        input.extend_from_slice(b"-0,");
        assert!(scan_number(&input, start, permissive).is_some());
        assert!(scan_number(&input, start, NumericPolicy::strict_default()).is_none());
    }
}

#[test]
fn fused_boundary_and_ascii_scan_matches_scalar_at_all_lane_edges() {
    for len in [0, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 95, 96] {
        for align in [0, 1, 7, 15, 16, 31] {
            for pos in 0..=len {
                for value in [0, 0x1f, b'"', b'\\', 0x7f, 0x80, 0xff] {
                    for non_ascii_prefix in [false, true] {
                        let mut storage = [b'a'; 128];
                        if pos < len {
                            storage[align + pos] = value;
                        }
                        if non_ascii_prefix && pos > 0 {
                            storage[align] = 0x80;
                        }
                        let input = &storage[align..align + len];
                        let boundary = input
                            .iter()
                            .position(|&byte| byte < 0x20 || byte == b'"' || byte == b'\\')
                            .unwrap_or(len);
                        assert_eq!(
                            jmax_dispatch::short::string_special_ascii(input),
                            (boundary, input[..boundary].is_ascii()),
                            "len={len} align={align} pos={pos} value={value}"
                        );
                    }
                }
            }
        }
    }
}
