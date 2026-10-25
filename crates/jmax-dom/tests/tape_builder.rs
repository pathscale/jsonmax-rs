//! General inputs produce the same tape and decoded bytes through both parse paths.

use jmax_dom::{DomScratch, ErrorCode, Limits, build};

#[test]
fn general_repeated_shapes_match_tape_and_decoded_baseline() -> jmax_dom::Result<()> {
    let mut raw = String::from("[");
    for index in 0..96 {
        if index != 0 {
            raw.push(',');
        }
        raw.push_str(&format!(
            r#"{{"id":{index},"name":"record-{index}","active":true,"note":"line\\n{index}"}}"#
        ));
    }
    raw.push(']');

    let strict = Limits::strict_default();
    let optimized =
        build::parse_borrowed_with_scratch(raw.as_bytes(), strict, &mut DomScratch::new())?;
    let mut baseline_limits = strict;
    baseline_limits.reject_duplicate_keys = false;
    let baseline = build::parse_borrowed_with_scratch(
        raw.as_bytes(),
        baseline_limits,
        &mut DomScratch::new(),
    )?;
    assert_eq!(optimized.tape(), baseline.tape());
    assert_eq!(optimized.decoded(), baseline.decoded());
    assert_eq!(optimized.root_node().array()?.len(), 96);
    Ok(())
}

#[test]
fn shape_divergence_and_aliases_still_reject_duplicates() {
    for raw in [
        br#"[{"a":0,"b":1},{"a":2,"c":3,"a":4}]"# as &[u8],
        br#"[{"a":0,"b":1},{"b":2,"a":3,"b":4}]"#,
        br#"[{"a":0,"b":1},{"a":2,"c":3,"\u0061":4}]"#,
        br#"[{"\u0061":0,"b":1},{"a":2,"b":3,"\u0061":4}]"#,
    ] {
        let error = build::parse_borrowed(raw, Limits::strict_default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey, "{raw:?}");
    }
}

#[test]
fn direct_path_keeps_malformed_input_errors() {
    let mut baseline_limits = Limits::strict_default();
    baseline_limits.reject_duplicate_keys = false;
    for raw in [
        br#"[{"a":1},{"a":2},]"# as &[u8],
        br#"[{"a":1},{"a":2 "#,
        b"[{\"a\":1},{\"a\":\xff}]",
        br#"[{"a":1},{"a":1e+}]"#,
    ] {
        let direct = build::parse_borrowed(raw, Limits::strict_default());
        let baseline = build::parse_borrowed(raw, baseline_limits);
        assert_eq!(
            direct
                .as_ref()
                .err()
                .map(|error| (error.code, error.byte_offset)),
            baseline
                .as_ref()
                .err()
                .map(|error| (error.code, error.byte_offset)),
            "{raw:?}"
        );
    }
}
