//! Adversarial coverage for the strict fast string scanner boundaries.

use jmax_abi::number::NumericPolicy;
use jmax_syntax::{Limits, Scratch, validate_borrowed};

fn valid(input: &[u8]) -> bool {
    validate_borrowed(
        input,
        Limits::strict_default(),
        Some(NumericPolicy::strict_default()),
        &mut Scratch::default(),
    )
    .is_ok()
}

#[test]
fn string_scan_handles_word_and_vector_edges() {
    for len in [
        0usize, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 47, 48, 63, 64, 65,
    ] {
        let body = "a".repeat(len);
        let quoted = format!("\"{body}\"");
        assert!(valid(quoted.as_bytes()), "length {len}");

        let escaped = format!("\"{}\\n{}\"", "a".repeat(len), "b".repeat(len));
        assert!(valid(escaped.as_bytes()), "escaped length {len}");

        let utf8 = format!("\"{}é{}\"", "a".repeat(len), "b".repeat(len));
        assert!(valid(utf8.as_bytes()), "UTF-8 length {len}");
    }
}

#[test]
fn string_scan_keeps_special_byte_and_utf8_diagnostics() {
    for len in [0usize, 6, 7, 8, 14, 15, 16, 30, 31, 32, 33] {
        let prefix = "a".repeat(len);
        let mut control = format!("\"{prefix}").into_bytes();
        control.push(0x1f);
        control.extend_from_slice(b"tail\"");
        let error = validate_borrowed(
            &control,
            Limits::strict_default(),
            None,
            &mut Scratch::default(),
        )
        .unwrap_err();
        assert_eq!(error.byte_offset as usize, len + 1, "control at {len}");

        let mut malformed = format!("\"{prefix}").into_bytes();
        malformed.extend_from_slice(&[0xc3, b'x', b'"']);
        assert!(
            validate_borrowed(
                &malformed,
                Limits::strict_default(),
                None,
                &mut Scratch::default(),
            )
            .is_err(),
            "malformed UTF-8 after {len}"
        );
    }
}

#[test]
fn repeated_shapes_and_escaped_keys_keep_duplicate_rejection() {
    let padding = "x".repeat(140);
    let cases = [
        (
            format!(r#"[{{"alpha":"{padding}","beta":2}},{{"alpha":"{padding}","beta":4}}]"#),
            true,
        ),
        (
            format!(
                r#"[{{"alpha":"{padding}","beta":2}},{{"alpha":"{padding}","beta":4,"gamma":5}}]"#
            ),
            true,
        ),
        (
            format!(
                r#"[{{"alpha":"{padding}","beta":2}},{{"alpha":"{padding}","beta":4,"beta":5}}]"#
            ),
            false,
        ),
        (
            format!(r#"[{{"alpha":"{padding}","beta":2}},{{"alpha":"{padding}","b\u0065ta":4}}]"#),
            true,
        ),
        (
            format!(
                r#"[{{"alpha":"{padding}","beta":2}},{{"alpha":"{padding}","gamma":4,"alpha":5}}]"#
            ),
            false,
        ),
    ];
    for (input, accepts) in cases {
        assert_eq!(valid(input.as_bytes()), accepts, "{input}");
    }
}

#[test]
fn repeated_wide_shapes_preserve_duplicate_rejection() {
    let padding = "x".repeat(32);
    let mut input = String::from("[{");
    for index in 0..9 {
        if index != 0 {
            input.push(',');
        }
        input.push_str(&format!("\"k{index}\":\"{padding}\""));
    }
    input.push_str("},{");
    for index in 0..9 {
        if index != 0 {
            input.push(',');
        }
        let key = if index == 8 { 7 } else { index };
        input.push_str(&format!("\"k{key}\":\"{padding}\""));
    }
    input.push_str("}]");

    assert!(!valid(input.as_bytes()));
}
