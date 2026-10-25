//! Checks for the strict fast path over arrays of two-number rows.

use jmax_abi::number::NumericPolicy;
use jmax_syntax::{Limits, Scratch, validate_borrowed};

fn accepts(input: &[u8], limits: Limits, numeric: Option<NumericPolicy>) -> bool {
    validate_borrowed(input, limits, numeric, &mut Scratch::default()).is_ok()
}

#[test]
fn two_number_rows_accept_spacing_and_number_forms() {
    for input in [
        &b"[[1,2],[3,4]]"[..],
        b"[ [ -1.25e+2 , 0 ], [3E-2, -4] ]",
        b"{\"rings\":[[[1,2],[3,4]],[[5,6]]]} ",
        b"[[1],[2,3]]",
        b"[[1,2,3]]",
        b"[[1,2],true]",
    ] {
        assert!(accepts(input, Limits::strict_default(), None), "{input:?}");
    }

    let rows = (0..256)
        .map(|n| format!("[{n},{n}.5]"))
        .collect::<Vec<_>>()
        .join(",");
    let document = format!("[{rows}]");
    assert!(accepts(
        document.as_bytes(),
        Limits::strict_default(),
        Some(NumericPolicy::strict_default()),
    ));
}

#[test]
fn matrix_candidates_fall_back_for_bad_rows_and_strict_numbers() {
    let numeric = Some(NumericPolicy::strict_default());
    for input in [
        &b"[[1,2],]"[..],
        b"[[1,2],[3,4]",
        b"[[01,2]]",
        b"[[-0,2]]",
        b"[[1e999,2]]",
    ] {
        assert!(
            validate_borrowed(
                input,
                Limits::strict_default(),
                numeric,
                &mut Scratch::default(),
            )
            .is_err(),
            "{input:?}"
        );
    }
}

#[test]
fn matrix_path_obeys_depth_item_and_digit_limits() {
    let mut limits = Limits::strict_default();
    limits.max_depth = 1;
    assert!(!accepts(b"[[1,2]]", limits, None));

    limits = Limits::strict_default();
    limits.max_array_items = 1;
    assert!(!accepts(b"[[1,2]]", limits, None));
    assert!(!accepts(b"[[1,2],[3,4]]", limits, None));

    limits = Limits::strict_default();
    limits.max_number_digits = 1;
    assert!(!accepts(b"[[12,1]]", limits, None));
}

#[test]
fn coordinate_shortcut_matches_event_parser_with_policies_and_limits() {
    use jmax_syntax::{BlockCursor, DuplicatePolicyOwner, ScanStep};
    for token in ["1.25", "-123.456789012345", "0.12", "-0.0", "1e2", "1.2e9999",
        "01.2", "1.", "1.2345678901234567", "99999999.1", "1.25x", "1.2.3"] {
        for digits in [1, 4, 64] {
            for reject_negative_zero in [false, true] {
                let input = format!("[[{token},2.5],[123.456789012345,45.678901234567]]");
                let mut limits = Limits::strict_default();
                limits.max_number_digits = digits;
                let numeric = Some(NumericPolicy { reject_negative_zero, ..NumericPolicy::strict_default() });
                let actual = validate_borrowed(input.as_bytes(), limits, numeric, &mut Scratch::default())
                    .err().map(|e| (e.code, e.byte_offset));
                let expected = (|| {
                    let mut cursor = BlockCursor::new(input.as_bytes(), limits, numeric, DuplicatePolicyOwner::Syntax)?;
                    let mut decoded = Vec::new();
                    while let ScanStep::Emit(_) = cursor.next_event(&mut decoded)? {}
                    cursor.finish_eof()
                })().err().map(|e| (e.code, e.byte_offset));
                assert_eq!(actual, expected, "{token}, digits={digits}, negative_zero={reject_negative_zero}");
            }
        }
    }
}

#[test]
fn scalar_strict_skips_at_late_offsets_match_cursor_diagnostics() {
    use jmax_abi::syntax::SkipMode;
    use jmax_syntax::{BlockCursor, DuplicatePolicyOwner, SkipScratch, skip_strict};
    for token in [br#""plain""#.as_slice(), br#""a\nb""#, br#""\u0061""#,
        b"123.5", b"-0", b"1e999", b"null", b"true", b"falsex", b"01", br#""\q""#] {
        for decoded_limit in [1, 1024] {
            let mut raw = vec![b' '; 768];
            raw.extend_from_slice(token);
            raw.push(b',');
            let mut limits = Limits::strict_default();
            limits.max_decoded_bytes = decoded_limit;
            limits.max_depth = 0;
            let numeric = Some(NumericPolicy::strict_default());
            let mut a = Vec::new();
            let mut b = Vec::new();
            let actual = skip_strict(&raw, 768, 0, limits, numeric, DuplicatePolicyOwner::Syntax,
                &mut a, &mut SkipScratch::default()).map_err(|e| (e.code, e.byte_offset, e.detail));
            let mut cursor = BlockCursor::new(&raw, limits, numeric, DuplicatePolicyOwner::Syntax).unwrap();
            cursor.advance(768);
            let expected = cursor.skip_value(&mut b, 0, SkipMode::StrictValidate)
                .map(|_| cursor.offset()).map_err(|e| (e.code, e.byte_offset, e.detail));
            assert_eq!(actual, expected, "{token:?}, decoded={decoded_limit}");
            assert_eq!(a, b);
        }
    }
}

#[test]
fn shape_changes_preserve_cumulative_discarded_decoded_total() {
    use jmax_syntax::{BlockCursor, DuplicatePolicyOwner, ScanStep};
    let fill = "x".repeat(120);
    let raw = format!(r#"[{{"a":"{fill}","b":1}},{{"a":"{fill}","b":1}},{{"a":"\n","b":[0]}}]"#);
    let mut limits = Limits::strict_default();
    limits.max_decoded_bytes = 1;
    validate_borrowed(raw.as_bytes(), limits, None, &mut Scratch::default()).unwrap();
    let mut cursor = BlockCursor::new(raw.as_bytes(), limits, None, DuplicatePolicyOwner::Syntax).unwrap();
    let mut decoded = Vec::new();
    while let ScanStep::Emit(_) = cursor.next_event(&mut decoded).unwrap() {}
    cursor.finish_eof().unwrap();
}
