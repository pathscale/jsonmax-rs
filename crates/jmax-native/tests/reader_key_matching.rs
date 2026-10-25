use jmax_native::{NativeContext, NumericPolicy, Reader, VmLimits};

fn context(limits: VmLimits) -> NativeContext {
    NativeContext::new(limits, NumericPolicy::strict_default())
}

#[test]
fn match_key_accepts_json_colon_spacing_and_escaped_names() {
    let mut ctx = context(VmLimits::default());
    let mut spaced = Reader::new(b"  \"name\" \t : true");
    assert!(spaced.match_key_decoded(&mut ctx, b"\"name\":"));
    assert!(spaced.boolean().unwrap());
    spaced.end().unwrap();

    let mut escaped = Reader::new(br#""na\u006de" : false"#);
    assert!(escaped.match_key_decoded(&mut ctx, b"\"name\":"));
    assert!(!escaped.boolean().unwrap());
    escaped.end().unwrap();
}

#[test]
fn match_key_miss_leaves_key_for_unknown_field_validation() {
    let mut ctx = context(VmLimits::default());
    let mut reader = Reader::new(b" \"other\" : 7");
    assert!(!reader.match_key(&mut ctx, b"\"name\":"));
    assert_eq!(reader.key_bytes(&mut ctx).unwrap(), b"other");
    assert_eq!(reader.uint(&mut ctx, u64::MAX).unwrap(), 7);
    reader.end().unwrap();
}

#[test]
fn unknown_value_skip_keeps_strict_syntax_validation() {
    let mut ctx = context(VmLimits::default());
    let mut valid = Reader::new(b" {\"nested\":[1,{\"ok\":true}]} ");
    valid.skip_value(&mut ctx, 0).unwrap();
    valid.end().unwrap();

    let mut malformed = Reader::new(b"{\"nested\":[1,]}");
    assert!(malformed.skip_value(&mut ctx, 0).is_err());
}
