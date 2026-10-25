//! Fresh scratch reservations and SIMD/SWAR boundary scans preserve the public tape.

use jmax_dom::{DomScratch, ErrorCode, Limits, build};

#[test]
fn fresh_scratch_builds_match_tape_across_string_and_number_boundaries() -> jmax_dom::Result<()> {
    // The leading long number selects the dense numeric path,
    // while the following strings cross each 16-byte SIMD boundary.
    let mut raw = format!("[{},", "1".repeat(1023));
    for len in [0, 1, 7, 8, 15, 16, 17, 31, 32, 33] {
        if len != 0 || raw.ends_with('"') {
            raw.push(',');
        }
        raw.push('"');
        raw.push_str(&"a".repeat(len));
        raw.push('"');
    }
    raw.push_str(",[0,1,12,123456789012345678901234567890,-0.125,6.02e23]]");
    let strict = Limits::strict_default();
    let mut permissive = strict;
    permissive.reject_duplicate_keys = false;
    let mut scratch = DomScratch::new();
    let fresh = build::parse_borrowed_with_scratch(raw.as_bytes(), strict, &mut scratch)?;
    let reference = build::parse_borrowed(raw.as_bytes(), permissive)?;
    assert_eq!(fresh.tape(), reference.tape());
    assert_eq!(fresh.decoded(), reference.decoded());
    assert_eq!(fresh.root_node().array()?.len(), 12);
    Ok(())
}

#[test]
fn every_decoded_alias_duplicate_is_rejected_after_scratch_reuse() {
    let mut scratch = DomScratch::new();
    let valid = br#"[{"a":0,"b":1},{"c":2}]"#;
    let doc =
        build::parse_borrowed_with_scratch(valid, Limits::strict_default(), &mut scratch).unwrap();
    doc.recycle(&mut scratch);

    for raw in [
        br#"{"a":0,"\u0061":1}"# as &[u8],
        br#"{"a":0,"\u0061":1,"a":2}"#,
        br#"[{"a":0},{"\u0061":1,"a":2}]"#,
        br#"{"\u0061":0,"a":1}"#,
    ] {
        let error = build::parse_borrowed_with_scratch(raw, Limits::strict_default(), &mut scratch)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey, "{raw:?}");
    }
}

#[test]
fn malformed_bytes_near_vector_boundaries_keep_scalar_diagnostics() {
    for raw in [
        b"[\"aaaaaaaaaaaaaaa\x01\"]" as &[u8],
        b"[\"aaaaaaaaaaaaaaaa\xff\"]",
        b"[123456789012345678901234567890x]",
        b"[1e+]",
    ] {
        assert!(build::parse_borrowed(raw, Limits::strict_default()).is_err());
    }
}

#[test]
fn inline_duplicate_buckets_spill_and_reset_without_losing_aliases() {
    for count in [5, 6, 7, 63, 64, 65, 129] {
        let fields = (0..count)
            .map(|i| format!("\"k{i}\":{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let valid = format!("[{{{fields}}},{{{fields}}}]");
        assert!(build::parse_borrowed(valid.as_bytes(), Limits::strict_default()).is_ok());
        let invalid = format!("[{{{fields}}},{{{fields},\"\\u006b0\":0}}]");
        assert_eq!(
            build::parse_borrowed(invalid.as_bytes(), Limits::strict_default())
                .unwrap_err()
                .code,
            ErrorCode::DuplicateObjectKey
        );
    }
}

#[test]
fn container_stack_spill_preserves_deep_tapes() {
    let raw = format!("{}0{}", "[".repeat(80), "]".repeat(80));
    let doc = build::parse_borrowed(raw.as_bytes(), Limits::strict_default()).unwrap();
    assert_eq!(doc.max_depth(), 80);
}

#[test]
fn deep_duplicate_storage_spills_and_recycles() {
    let mut scratch = DomScratch::new();
    let wrap = |value: &str| format!("{}{value}{}", "{\"n\":".repeat(20), "}".repeat(20));
    let valid = wrap("{\"a\":0}");
    let doc = build::parse_borrowed_with_scratch(
        valid.as_bytes(),
        Limits::strict_default(),
        &mut scratch,
    )
    .unwrap();
    doc.recycle(&mut scratch);
    let duplicate = wrap("{\"a\":0,\"\\u0061\":1}");
    assert_eq!(
        build::parse_borrowed_with_scratch(
            duplicate.as_bytes(),
            Limits::strict_default(),
            &mut scratch
        )
        .unwrap_err()
        .code,
        ErrorCode::DuplicateObjectKey
    );
}
