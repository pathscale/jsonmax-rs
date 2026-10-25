use jmax_dom::{DomScratch, ErrorCode, Limits, build};

#[test]
fn coordinate_ring_and_reference_array_have_identical_tape() -> jmax_dom::Result<()> {
    let coordinate = "1".repeat(100);
    let other = "2".repeat(100);
    let mut matrix = String::from("[");
    for row in 0..8 {
        if row != 0 {
            matrix.push(',');
        }
        matrix.push('[');
        matrix.push_str(&coordinate);
        matrix.push(',');
        matrix.push_str(&other);
        matrix.push(']');
    }
    matrix.push(']');
    let raw = matrix.as_bytes();
    assert!(raw.len() >= 1024);
    {
        let optimized = build::parse_borrowed_with_numeric_and_scratch(
            raw,
            Limits::strict_default(),
            jmax_abi::number::NumericPolicy::strict_default(),
            &mut DomScratch::new(),
        )?;
        let mut limits = Limits::strict_default();
        limits.reject_duplicate_keys = false;
        let reference = build::parse_borrowed_with_numeric_and_scratch(
            raw,
            limits,
            jmax_abi::number::NumericPolicy::strict_default(),
            &mut DomScratch::new(),
        )?;
        assert_eq!(optimized.tape(), reference.tape());
    }
    Ok(())
}

#[test]
fn coordinate_ring_numeric_policy_matches_reference_and_defers_uncertain_values() {
    let numeric = jmax_abi::number::NumericPolicy::strict_default();
    let ring = |first: &str, second: &str| {
        let row = format!("[{first},{second}]");
        format!("[{0},{0},{0},{0},{0},{0},{0},{0}]", row)
    };
    for input in [ring("1.25", "-2.5"), ring("1e2", "2")] {
        let raw = input.as_bytes();
        let optimized = build::parse_borrowed_with_numeric(raw, Limits::strict_default(), numeric);
        let mut limits = Limits::strict_default();
        limits.reject_duplicate_keys = false;
        let reference = build::parse_borrowed_with_numeric(raw, limits, numeric);
        assert_eq!(
            optimized.as_ref().map(|doc| doc.tape().to_vec()),
            reference.as_ref().map(|doc| doc.tape().to_vec())
        );
    }
    let negative_zero = ring("-0", "1");
    let optimized = build::parse_borrowed_with_numeric(
        negative_zero.as_bytes(),
        Limits::strict_default(),
        numeric,
    );
    let mut limits = Limits::strict_default();
    limits.reject_duplicate_keys = false;
    let reference = build::parse_borrowed_with_numeric(negative_zero.as_bytes(), limits, numeric);
    assert_eq!(
        optimized.as_ref().err().map(|e| (e.code, e.byte_offset)),
        reference.as_ref().err().map(|e| (e.code, e.byte_offset))
    );
    assert!(optimized.is_err());
}

#[test]
fn escaped_alias_divergence_and_malformed_coordinates_keep_semantics() {
    let value = "7".repeat(220);
    let escaped =
        format!(r#"[{{"a":{value},"b":{value}}},{{"\u0061":{value},"b":{value},"a":{value}}}]"#);
    let diverged =
        format!(r#"[{{"a":{value},"b":{value}}},{{"a":{value},"c":{value},"a":{value}}}]"#);
    for raw in [escaped.as_bytes(), diverged.as_bytes()] {
        assert!(raw.len() >= 1024);
        let error = build::parse_borrowed(raw, Limits::strict_default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
    }
    let coordinate = "3".repeat(120);
    let other = "4".repeat(120);
    let mut malformed_ring = String::from("[");
    for row in 0..8 {
        if row != 0 {
            malformed_ring.push(',');
        }
        malformed_ring.push('[');
        malformed_ring.push_str(&coordinate);
        malformed_ring.push(',');
        malformed_ring.push_str(&other);
        malformed_ring.push(']');
    }
    malformed_ring.push_str(",]");
    let mut malformed = vec![br#"[[1,2,]]"#.as_slice(), br#"[[01,2]]"#, br#"[[1e2,2]]"#];
    malformed.push(malformed_ring.as_bytes());
    for raw in malformed {
        let optimized = build::parse_borrowed(raw, Limits::strict_default());
        let mut limits = Limits::strict_default();
        limits.reject_duplicate_keys = false;
        let reference = build::parse_borrowed(raw, limits);
        assert_eq!(
            optimized.as_ref().err().map(|e| (e.code, e.byte_offset)),
            reference.as_ref().err().map(|e| (e.code, e.byte_offset))
        );
    }
}

#[test]
fn coordinate_rows_require_each_opening_bracket() {
    let raw = format!("[[1,2],x3,4],{}]", " ".repeat(1100));
    assert!(build::parse_borrowed(raw.as_bytes(), Limits::strict_default()).is_err());
}
