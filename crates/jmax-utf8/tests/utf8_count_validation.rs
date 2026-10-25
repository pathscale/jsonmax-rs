use jmax_utf8::{count_continuation_bytes, count_scalars_trusted_utf8, validate_and_count_scalars};

#[test]
fn bulk_count_matches_scalar_reference_at_every_word_boundary() {
    let text = "a¢€😀\u{10ffff}\u{301}".repeat(100);
    for start in 0..32 {
        let bytes = &text.as_bytes()[start..];
        for len in 0..bytes.len().min(160) {
            let chunk = &bytes[..len];
            let expected = chunk.iter().filter(|&&b| b & 0xc0 == 0x80).count();
            assert_eq!(count_continuation_bytes(chunk), expected);
        }
    }
    for text in ["", "ascii", "a¢€😀\u{10ffff}\u{301}"] {
        assert_eq!(
            validate_and_count_scalars(text.as_bytes(), 19).unwrap(),
            text.chars().count() as u32
        );
        assert_eq!(
            count_scalars_trusted_utf8(text.as_bytes()),
            text.chars().count() as u32
        );
    }
    for b in 0..=255 {
        assert_eq!(
            count_continuation_bytes(&[b; 65]),
            if b & 0xc0 == 0x80 { 65 } else { 0 }
        );
    }
}

#[test]
fn invalid_and_truncated_sequences_have_absolute_leading_offsets() {
    for invalid in [
        &b"ab\xc0\x80"[..],
        &b"ab\xed\xa0\x80"[..],
        &b"ab\xf4\x90\x80\x80"[..],
        &b"ab\xf0\x90"[..],
        &b"ab\x80"[..],
    ] {
        let error = validate_and_count_scalars(invalid, 71).unwrap_err();
        assert_eq!(error.code, jmax_utf8::ErrorCode::InvalidUtf8);
        assert_eq!(error.byte_offset, 73);
    }
    assert!(validate_and_count_scalars(b"x", u32::MAX).is_err());
    assert_eq!(validate_and_count_scalars(b"", u32::MAX).unwrap(), 0);
}
