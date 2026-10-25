//! Exact public tape and decoded-arena invariants on adversarial inputs.

use jmax_abi::buffer::Payload;
use jmax_dom::{DomScratch, ErrorCode, Limits, build};

#[test]
fn owned_and_borrowed_parses_publish_identical_tape_and_decoded_bytes() -> jmax_dom::Result<()> {
    let raw = br#"[{"a":1,"b":"line\nfeed"},{"x":[true,null],"y":"\u0061"}]"#;
    let owned = build::parse(Payload::from_slice(raw), Limits::strict_default())?;
    let borrowed = build::parse_borrowed(raw, Limits::strict_default())?;
    assert_eq!(owned.tape(), borrowed.tape());
    assert_eq!(owned.decoded(), borrowed.decoded());
    assert_eq!(owned.metadata(), borrowed.metadata());
    assert_eq!(borrowed.decoded(), b"line\nfeeda");

    let rows = borrowed.root_node().array()?;
    assert_eq!(rows.len(), 2);
    let first = rows.get(0).unwrap().object()?;
    let second = rows.get(1).unwrap().object()?;
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    assert_eq!(first.get("b").unwrap().string()?, "line\nfeed");
    assert_eq!(second.get("y").unwrap().string()?, "a");
    Ok(())
}

#[test]
fn decoded_duplicate_detection_survives_key_shape_changes_and_scratch_reuse() -> jmax_dom::Result<()>
{
    let mut scratch = DomScratch::new();
    let duplicate = br#"{"alpha":0,"b":1,"gamma":2,"\u0061lpha":3}"#;
    let error =
        build::parse_borrowed_with_scratch(duplicate, Limits::strict_default(), &mut scratch)
            .unwrap_err();
    assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
    let duplicate_key = br#""\u0061lpha""#;
    let expected_offset = duplicate
        .windows(duplicate_key.len())
        .position(|window| window == duplicate_key)
        .unwrap() as u32;
    assert_eq!(error.byte_offset, expected_offset);

    let valid = br#"[{"alpha":0,"b":1,"gamma":2},{"x":3,"y":4},{"alpha":5}]"#;
    let doc = build::parse_borrowed_with_scratch(valid, Limits::strict_default(), &mut scratch)?;
    assert_eq!(doc.root_node().array()?.len(), 3);
    assert_eq!(doc.decoded(), b"");
    Ok(())
}

#[test]
fn malformed_utf8_controls_escapes_and_tight_limits_never_publish_a_tape() {
    for raw in [
        &b"[\"\xff\"]"[..],
        b"[\"\x00\"]",
        br#"["\q"]"#,
        br#"["\uD800"]"#,
        b"[01]",
        b"[1e+]",
    ] {
        assert!(
            build::parse_borrowed(raw, Limits::strict_default()).is_err(),
            "{raw:?}"
        );
    }
    assert!(build::parse_borrowed(b"[-0]", Limits::strict_default()).is_ok());
    let mut limits = Limits::strict_default();
    limits.max_string_bytes = 2;
    assert!(build::parse_borrowed(br#""abc""#, limits).is_err());
    limits = Limits::strict_default();
    limits.max_number_digits = 2;
    assert!(build::parse_borrowed(b"123", limits).is_err());
    limits = Limits::strict_default();
    limits.max_decoded_bytes = 1;
    assert!(build::parse_borrowed(br#"["\n\t"]"#, limits).is_err());
}
