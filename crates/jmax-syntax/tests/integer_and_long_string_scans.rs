//! Regression cases for the integer-array and long-string fast scans.

use jmax_abi::number::NumericPolicy;
use jmax_syntax::{Limits, Scratch, ValidationProof, validate_borrowed};

fn check(
    input: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
) -> jmax_syntax::Result<ValidationProof> {
    validate_borrowed(input, limits, numeric, &mut Scratch::default())
}

#[test]
fn flat_integer_arrays_keep_json_grammar_and_limits() {
    let strict = Limits::strict_default();
    let numeric = Some(NumericPolicy::strict_default());
    for input in [
        b"[]".as_slice(),
        b"[0]",
        b"[-1,0,23,456789]",
        b"[ \n-12,\t0 , 34\r\n]",
        b"[[1,2],3]",
        b"[1.25,2]",
        b"[1e2,-3E-4]",
        b"[1,true,\"mixed\"]",
    ] {
        assert!(check(input, strict, numeric).is_ok(), "valid {input:?}");
    }
    for input in [
        b"[01]".as_slice(),
        b"[-]",
        b"[+]",
        b"[1,]",
        b"[1 2]",
        b"[1x]",
        b"[--1]",
        b"[1e]",
        b"[1.]",
    ] {
        assert!(check(input, strict, numeric).is_err(), "invalid {input:?}");
    }

    let mut bounded = strict;
    bounded.max_array_items = 0;
    assert!(check(b"[]", bounded, None).is_ok());
    assert!(check(b"[0]", bounded, None).is_err());
    bounded = strict;
    bounded.max_array_items = 2;
    assert!(check(b"[1,2]", bounded, None).is_ok());
    assert!(check(b"[1,2,3]", bounded, None).is_err());
    bounded = strict;
    bounded.max_number_digits = 3;
    assert!(check(b"[123,-9]", bounded, None).is_ok());
    assert!(check(b"[1234]", bounded, None).is_err());

    assert!(check(b"[-0,0]", strict, Some(NumericPolicy::strict_default())).is_err());
    assert!(check(b"[-0,0]", strict, None).is_ok());
}

#[test]
fn integer_scanner_handles_long_runs_and_nested_depth_edges() {
    let strict = Limits::strict_default();
    let digits = "12345678".repeat(128);
    let input = format!("[{digits},-{},0]", "9".repeat(1024));
    assert!(check(input.as_bytes(), strict, None).is_ok());

    let mut one_level = strict;
    one_level.max_depth = 1;
    assert!(check(b"[1,2,3]", one_level, None).is_ok());
    assert!(check(b"[[1],2]", one_level, None).is_err());
}

#[test]
fn long_string_scan_preserves_special_and_utf8_boundaries() {
    let strict = Limits::strict_default();
    for prefix in [15usize, 16, 31, 32, 47, 48, 63, 64, 65, 79, 95, 127, 191] {
        let mut quoted = vec![b'"'];
        quoted.extend(std::iter::repeat(b'a').take(prefix));
        quoted.extend_from_slice(b"plain tail\"");
        assert!(
            check(&quoted, strict, None).is_ok(),
            "valid string at {prefix}"
        );

        for special in [0x00, 0x1f, b'\\'] {
            let mut invalid = vec![b'"'];
            invalid.extend(std::iter::repeat(b'a').take(prefix));
            invalid.push(special);
            if special == b'\\' {
                invalid.push(b'q');
            }
            invalid.push(b'"');
            assert!(
                check(&invalid, strict, None).is_err(),
                "special at {prefix}"
            );
        }

        let mut utf8 = vec![b'"'];
        utf8.extend(std::iter::repeat(b'a').take(prefix));
        utf8.extend_from_slice("é中".as_bytes());
        utf8.push(b'"');
        assert!(check(&utf8, strict, None).is_ok(), "UTF-8 at {prefix}");
    }
}

#[test]
fn uncached_string_metadata_matches_cursor_across_boundaries_and_failures() {
    use jmax_abi::syntax::DuplicatePolicyOwner;
    use jmax_syntax::block_cursor::BlockCursor;
    for len in [0, 1, 7, 8, 15, 16, 31, 32, 63, 64, 95, 511, 512, 513, 1024] {
        for suffix in ["plain", "é雪", "\\n\\u03bb", "\\ud800", "\\x", "\u{1}"] {
            let prefix = "[0,\"ignored\",";
            let raw = format!("{prefix}\"{}{suffix}\"]", "a".repeat(len));
            let mut a = BlockCursor::new(
                raw.as_bytes(),
                Limits::strict_default(),
                None,
                DuplicatePolicyOwner::Syntax,
            )
            .unwrap();
            let mut b = BlockCursor::new(
                raw.as_bytes(),
                Limits::strict_default(),
                None,
                DuplicatePolicyOwner::Syntax,
            )
            .unwrap();
            a.advance(prefix.len() as u32);
            b.advance(prefix.len() as u32);
            let (mut x, mut y) = (Vec::new(), Vec::new());
            let actual = a.scan_string_meta_uncached(&mut x);
            let expected = b.scan_string_meta(&mut y);
            assert_eq!(actual, expected, "{len}/{suffix}");
            assert_eq!(x, y);
            assert_eq!(a.offset(), b.offset());
        }
    }
}
