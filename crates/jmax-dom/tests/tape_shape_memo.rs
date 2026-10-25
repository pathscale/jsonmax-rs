//! Exact raw-key shape reuse must preserve duplicate checks and tape output.

use jmax_dom::{DomScratch, ErrorCode, Limits, build};

#[test]
fn memoized_exact_shapes_emit_identical_tape_words() -> jmax_dom::Result<()> {
    let raw = br#"[{"alpha":1,"beta":2},{"alpha":3,"beta":4},{"alpha":5,"beta":6}]"#;
    let mut warm_scratch = DomScratch::new();
    let warm = build::parse_borrowed_with_scratch(raw, Limits::strict_default(), &mut warm_scratch)?;
    let mut no_duplicate_check = Limits::strict_default();
    no_duplicate_check.reject_duplicate_keys = false;
    let baseline = build::parse_borrowed_with_scratch(raw, no_duplicate_check, &mut DomScratch::new())?;
    assert_eq!(warm.tape(), baseline.tape());
    Ok(())
}

#[test]
fn divergence_seeds_matched_prefix_before_duplicate_lookup() {
    for (raw, duplicate) in [
        (
            br#"[{"a":0,"b":1},{"a":2,"b":3,"a":4}]"# as &[u8],
            br#""a":4"# as &[u8],
        ),
        (
            br#"[{"a":0,"b":1},{"b":2,"a":3,"b":4}]"#,
            br#""b":4"#,
        ),
    ] {
        let error = build::parse_borrowed(raw, Limits::strict_default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
        let expected = raw
            .windows(duplicate.len())
            .rposition(|window| window == duplicate)
            .unwrap() as u32;
        assert_eq!(error.byte_offset, expected);
    }
}

#[test]
fn raw_escape_mismatch_and_member_limits_keep_existing_semantics() -> jmax_dom::Result<()> {
    let valid = br#"[{"a":0,"b":1},{"\u0061":2,"b":3}]"#;
    let doc = build::parse_borrowed(valid, Limits::strict_default())?;
    assert_eq!(doc.root_node().array()?.len(), 2);

    let duplicate_alias = br#"[{"a":0,"b":1},{"\u0061":2,"b":3,"a":4}]"#;
    let error = build::parse_borrowed(duplicate_alias, Limits::strict_default()).unwrap_err();
    assert_eq!(error.code, ErrorCode::DuplicateObjectKey);

    let mut limits = Limits::strict_default();
    limits.max_fields_per_object = 2;
    let too_many = br#"[{"a":0,"b":1},{"a":2,"b":3,"c":4}]"#;
    let error = build::parse_borrowed(too_many, limits).unwrap_err();
    assert_eq!(error.code, ErrorCode::ConstraintViolation);
    Ok(())
}
