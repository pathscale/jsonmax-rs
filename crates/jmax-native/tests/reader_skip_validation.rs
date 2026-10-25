use jmax_native::{NativeContext, NumericPolicy, Reader, VmLimits};

fn context(limits: VmLimits) -> NativeContext {
    NativeContext::new(limits, NumericPolicy::strict_default())
}

#[test]
fn decoded_key_misses_stop_before_values_and_keep_the_key() {
    let mut ctx = context(VmLimits::default());
    for input in [
        &b"\"other\": [\"\\\\lots of text\"]"[..],
        b"\"na\\u0078\": 1",
        b"\"nameX\": 1",
        b"\"name\"x: 1",
    ] {
        let mut reader = Reader::new(input);
        assert!(!reader.match_key_decoded(&mut ctx, b"\"name\":"));
        assert_eq!(reader.pos(), 0);
    }

    let mut escaped = Reader::new(br#"  "na\u006de" 	: true"#);
    assert!(escaped.match_key_decoded(&mut ctx, b"\"name\":"));
    assert_eq!(escaped.pos(), 16);
    assert!(escaped.boolean().unwrap());
    escaped.end().unwrap();
}

#[test]
fn decoded_key_match_respects_limits_and_colon_syntax() {
    let mut limits = VmLimits::default();
    limits.syntax.max_string_bytes = 3;
    let mut ctx = context(limits);
    let mut too_long = Reader::new(b"\"name\": 1");
    assert!(!too_long.match_key_decoded(&mut ctx, b"\"name\":"));
    assert_eq!(too_long.pos(), 0);

    let mut ctx = context(VmLimits::default());
    let mut missing_colon = Reader::new(br#""na\u006de" x"#);
    assert!(!missing_colon.match_key_decoded(&mut ctx, b"\"name\":"));
    assert_eq!(missing_colon.pos(), 0);
}

#[test]
fn strict_skip_literals_match_boundaries_and_reject_near_misses() {
    for (token, length) in [
        (&b"true,"[..], 4usize),
        (b"false]", 5),
        (b"null}", 4),
        (b" true\n", 5),
    ] {
        let mut ctx = context(VmLimits::default());
        let mut reader = Reader::new(token);
        reader.skip_value(&mut ctx, 0).unwrap();
        assert_eq!(reader.pos(), length);
    }

    for token in [&b"trueX"[..], b"falsefalse", b"nullx", b"tru"] {
        let mut ctx = context(VmLimits::default());
        let mut reader = Reader::new(token);
        assert!(reader.skip_value(&mut ctx, 0).is_err());
    }
}

#[test]
fn strict_skip_literal_observes_payload_depth_and_numeric_policy() {
    let mut limits = VmLimits::default();
    limits.syntax.max_payload_bytes = 3;
    let mut reader = Reader::new(b"true");
    assert!(reader.skip_value(&mut context(limits), 0).is_err());

    let mut limits = VmLimits::default();
    limits.syntax.max_depth = 0;
    let mut reader = Reader::new(b"true");
    reader.skip_value(&mut context(limits), 1).unwrap_err();

    let mut policy = NumericPolicy::strict_default();
    policy.max_exponent_abs = -1;
    let mut ctx = NativeContext::new(VmLimits::default(), policy);
    let mut reader = Reader::new(b"true");
    assert!(reader.skip_value(&mut ctx, 0).is_err());
}
