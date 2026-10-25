//! Boundary and adversarial checks for local SIMD and SWAR scanners.

use jmax_abi::number::NumericPolicy;
use jmax_syntax::{Limits, Scratch, validate_borrowed};

fn check(input: &[u8]) -> jmax_syntax::Result<jmax_syntax::ValidationProof> {
    validate_borrowed(
        input,
        Limits::strict_default(),
        Some(NumericPolicy::strict_default()),
        &mut Scratch::default(),
    )
}

#[test]
fn string_specials_at_word_and_vector_edges() {
    for prefix in 0..40 {
        for special in [b'"', b'\\', 0x00, 0x1f] {
            let mut input = vec![b'"'];
            input.extend(std::iter::repeat(b'a').take(prefix));
            input.push(special);
            if special == b'\\' {
                input.push(b'q');
            }
            input.extend_from_slice(b"tail\"");
            let result = check(&input);
            if special == b'"' {
                assert!(result.is_err(), "early quote at {prefix}");
            } else {
                assert!(result.is_err(), "invalid string byte at {prefix}");
            }
        }
        let body = format!("{}é{}", "a".repeat(prefix), "b".repeat(40 - prefix));
        let quoted = format!("\"{body}\"");
        assert!(check(quoted.as_bytes()).is_ok(), "UTF-8 at {prefix}");
    }
}

#[test]
fn whitespace_runs_and_non_json_controls_keep_boundaries() {
    for prefix in 0..40 {
        let mut input = vec![b' '];
        input.extend(std::iter::repeat(b' ').take(prefix));
        input.extend_from_slice(b"\t\r\n true");
        assert!(check(&input).is_ok(), "whitespace run {prefix}");

        input.push(0x0b);
        assert!(check(&input).is_err(), "vertical tab after {prefix}");
    }
}

#[test]
fn key_scans_preserve_duplicate_and_escape_semantics() {
    for len in [0usize, 1, 7, 8, 15, 16, 17, 31, 32, 33, 47] {
        let key = "k".repeat(len);
        let duplicate = format!("{{\"{key}\":0,\"{key}\":1}}");
        assert!(
            check(duplicate.as_bytes()).is_err(),
            "duplicate length {len}"
        );

        let unique = format!("{{\"{key}\":0,\"{}x\":1}}", key);
        assert!(check(unique.as_bytes()).is_ok(), "unique length {len}");
    }
    for input in [
        br#"{"a":0,"\u0061":1}"#.as_slice(),
        br#"{"a\n":0,"a\u000a":1}"#,
    ] {
        assert!(check(input).is_err(), "escaped duplicate {input:?}");
    }
}

#[test]
fn malformed_utf8_and_escaped_string_errors_survive_chunk_edges() {
    for prefix in [6usize, 7, 8, 14, 15, 16, 30, 31, 32, 33] {
        let mut malformed = vec![b'"'];
        malformed.extend(std::iter::repeat(b'a').take(prefix));
        malformed.extend_from_slice(&[0xc3, b'x', b'"']);
        assert!(check(&malformed).is_err(), "UTF-8 at {prefix}");

        let escaped = format!("\"{}\\n{}\"", "a".repeat(prefix), "b".repeat(35));
        assert!(check(escaped.as_bytes()).is_ok(), "escape at {prefix}");
    }
}
