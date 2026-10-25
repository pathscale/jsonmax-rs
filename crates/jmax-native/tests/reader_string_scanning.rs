use jmax_native::{NativeContext, NumericPolicy, Reader, VmLimits};

fn context(limits: VmLimits) -> NativeContext {
    NativeContext::new(limits, NumericPolicy::strict_default())
}

#[test]
fn string_scanner_handles_short_boundaries_and_simd_spans() {
    for length in [0usize, 1, 7, 8, 15, 16, 31, 32, 33, 64] {
        let mut input = Vec::with_capacity(length + 2);
        input.push(b'"');
        input.resize(length + 1, b'a');
        input.push(b'"');
        let mut reader = Reader::new(&input);
        let mut ctx = context(VmLimits::default());
        let got = reader.string(&mut ctx).unwrap();
        assert_eq!(got.len(), length);
        assert!(got.bytes().all(|byte| byte == b'a'));
        reader.end().unwrap();
    }
}

#[test]
fn string_scanner_preserves_escape_utf8_and_limit_validation() {
    let mut ctx = context(VmLimits::default());
    for prefix in [0, 7, 15, 31, 32, 47, 64] {
        let value = format!("{}é{}", "a".repeat(prefix), "b".repeat(40));
        let input = format!("\"{value}\"");
        assert_eq!(
            Reader::new(input.as_bytes()).string(&mut ctx).unwrap(),
            value
        );
    }
    let mut escaped = Reader::new(br#""prefix\u0061\n""#);
    assert_eq!(escaped.string(&mut ctx).unwrap(), "prefixa\n");

    for input in [&b"\"bad\ntext\""[..], b"\"bad\xff\"", br#""\ud800""#] {
        let mut reader = Reader::new(input);
        let mut ctx = context(VmLimits::default());
        assert!(reader.string(&mut ctx).is_err());
        assert_eq!(reader.pos(), 0);
    }

    let mut limits = VmLimits::default();
    limits.syntax.max_string_bytes = 8;
    let mut reader = Reader::new(br#""a\u0062c""#);
    assert_eq!(reader.string(&mut context(limits)).unwrap(), "abc");
    let mut limits = VmLimits::default();
    limits.syntax.max_string_bytes = 2;
    let mut reader = Reader::new(br#""a\u0062c""#);
    assert!(reader.string(&mut context(limits)).is_err());
    assert_eq!(reader.pos(), 0);
}

#[test]
fn decoded_key_fast_match_keeps_misses_and_accepts_escaped_name() {
    let mut ctx = context(VmLimits::default());
    let mut input = Vec::from(&b"  \"na\\u006de\" \t: true"[..]);
    let mut reader = Reader::new(&input);
    assert!(reader.match_key_decoded(&mut ctx, b"\"name\":"));
    assert!(reader.boolean().unwrap());
    reader.end().unwrap();

    input = Vec::from(br#""nameX": ["a long ignored value"]"#);
    let mut reader = Reader::new(&input);
    assert!(!reader.match_key_decoded(&mut ctx, b"\"name\":"));
    assert_eq!(reader.pos(), 0);
}

#[test]
fn strict_skip_checks_nested_duplicates_and_does_not_advance_on_error() {
    let mut ctx = context(VmLimits::default());
    let mut valid = Reader::new(br#"{"items":[true,null,{"x":1}]} "#);
    valid.skip_value(&mut ctx, 0).unwrap();
    assert_eq!(valid.pos(), br#"{"items":[true,null,{"x":1}]}"#.len());

    let mut duplicate = Reader::new(br#"{"x":1,"x":2}"#);
    assert!(
        duplicate
            .skip_value(&mut context(VmLimits::default()), 0)
            .is_err()
    );
    assert_eq!(duplicate.pos(), 0);

    let mut malformed = Reader::new(br#"[1,]"#);
    assert!(
        malformed
            .skip_value(&mut context(VmLimits::default()), 0)
            .is_err()
    );
    assert_eq!(malformed.pos(), 0);
}
