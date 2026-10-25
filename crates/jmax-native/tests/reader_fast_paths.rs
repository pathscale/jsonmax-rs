use jmax_native::{NativeContext, NumericPolicy, Reader, VmLimits};

fn context(limits: VmLimits) -> NativeContext {
    NativeContext::new(limits, NumericPolicy::strict_default())
}

#[test]
fn flat_scalar_arrays_accept_mixed_integer_and_string_items() {
    for (input, next) in [
        (&br#"[1,"two",-3,"four"] trailing"#[..], Some(b't')),
        (&br#"[ 1 , "two" , -3 , "four" ]]"#[..], Some(b']')),
        (&br#"["escaped\ntext",0]"#[..], None),
        (r#"["雪",1]"#.as_bytes(), None),
    ] {
        let mut reader = Reader::new(input);
        reader
            .skip_value(&mut context(VmLimits::default()), 0)
            .unwrap();
        assert_eq!(reader.peek(), next);
    }
}

#[test]
fn flat_array_fast_path_preserves_strict_grammar_and_limits() {
    for input in [&b"[1,]"[..], b"[,1]", b"[1,,2]", b"[01]", b"[1 2]"] {
        let mut reader = Reader::new(input);
        assert!(
            reader
                .skip_value(&mut context(VmLimits::default()), 0)
                .is_err()
        );
        assert_eq!(reader.pos(), 0);
    }

    let mut limits = VmLimits::default();
    limits.syntax.max_array_items = 2;
    let mut reader = Reader::new(b"[1,2,3]");
    assert!(reader.skip_value(&mut context(limits), 0).is_err());
    assert_eq!(reader.pos(), 0);

    let mut limits = VmLimits::default();
    limits.syntax.max_decoded_bytes = 3;
    let mut reader = Reader::new(br#"["a\nb","c\nd"]"#);
    assert!(reader.skip_value(&mut context(limits), 0).is_err());
    assert_eq!(reader.pos(), 0);
}

#[test]
fn flat_scalar_objects_and_escaped_duplicates_use_strict_fallback() {
    let input = br#"{"n":1,"s":"value","flag":true}"#;
    let mut limits = VmLimits::default();
    limits.syntax.reject_duplicate_keys = false;
    let mut reader = Reader::new(input);
    reader.skip_value(&mut context(limits), 0).unwrap();
    assert_eq!(reader.pos(), input.len());

    let duplicate = br#"{"name":"first","na\u006de":"second"}"#;
    let mut reader = Reader::new(duplicate);
    assert!(
        reader
            .skip_value(&mut context(VmLimits::default()), 0)
            .is_err()
    );
    assert_eq!(reader.pos(), 0);
}

#[test]
fn eight_fraction_digits_keep_coefficients_policies_and_invalid_byte_fallback() {
    use jmax_native::DecimalPolicy;
    fn check(input: &[u8], policy: DecimalPolicy) {
        let mut reader = Reader::new(input);
        let result = reader
            .decimal(&mut context(VmLimits::default()), &policy)
            .and_then(|value| reader.end().map(|()| value));
        let expected = jmax_decimal::parse::parse_quoted(input, 0, &policy);
        assert_eq!(result.is_ok(), expected.is_ok(), "{input:?}/{policy:?}");
        if let (Ok(a), Ok(b)) = (result, expected) {
            assert_eq!(a, b);
        }
    }
    let policy = DecimalPolicy::spot_8();
    for fraction in [0, 1, 11, 12_345_678, 87_654_321, 99_999_999] {
        for integer in [0u64, 1, 999, 9_999_999_999, 99_999_999_999] {
            for negative in [false, true] {
                if negative && integer == 0 && fraction == 0 {
                    continue;
                }
                let input = format!(
                    "\"{}{integer}.{fraction:08}\"",
                    if negative { "-" } else { "" }
                );
                for target_scale in [0, 2, 8, 12, 38] {
                    for tick_step in [1, 2, 100] {
                        check(
                            input.as_bytes(),
                            DecimalPolicy {
                                target_scale,
                                tick_step,
                                ..policy
                            },
                        );
                    }
                }
            }
        }
    }
    for position in 0..8 {
        for byte in 0..=255 {
            let mut input = *b"\"7.12345678\"";
            input[position + 3] = byte;
            check(&input, policy);
        }
    }
}
