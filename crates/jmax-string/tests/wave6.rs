use jmax_buffer::DecodedArena;
use jmax_string::{
    ErrorCode, SpanSource, decode_escaped_slice_into, escape_mask_64, escape_masks_64,
    first_escape_or_control,
};

#[test]
fn transactional_decode_preserves_prefix_and_counts_scalars() {
    let mut arena = DecodedArena::new(4096);
    arena.append(b"prefix:").unwrap();
    let input = "ordinary € \\".as_bytes();
    assert!(decode_escaped_slice_into(input, 10, &mut arena, 4096).is_err());
    assert_eq!(arena.as_slice(), b"prefix:");
    let input = br#"a\"\\\/\b\f\n\r\t\u0000\u20ac\uD83D\uDE00z"#;
    let output = "a\"\\/\u{8}\u{c}\n\r\t\0€😀z";
    let reference = decode_escaped_slice_into(input, 10, &mut arena, 4096).unwrap();
    assert_eq!(reference.source, SpanSource::DecodedArena);
    assert_eq!(reference.scalar_len, output.chars().count() as u32);
    assert_eq!(
        reference.span.slice(arena.as_slice()).unwrap(),
        output.as_bytes()
    );
    assert_eq!(&arena.as_slice()[..7], b"prefix:");
    let unicode = "€😀e\u{301}";
    let reference = decode_escaped_slice_into(unicode.as_bytes(), 33, &mut arena, 4096).unwrap();
    assert_eq!(
        reference.span.slice(arena.as_slice()).unwrap(),
        unicode.as_bytes()
    );
    assert_eq!(reference.scalar_len, 4);
    let empty = decode_escaped_slice_into(b"", u32::MAX, &mut arena, 0).unwrap();
    assert_eq!(empty.span.len, 0);
    assert_eq!(empty.scalar_len, 0);
}

#[test]
fn every_error_rolls_back_already_appended_runs_and_escapes() {
    let cases: &[(&[u8], ErrorCode, u32)] = &[
        (br"good\nthen\q", ErrorCode::InvalidEscape, 11),
        (br"good\uD800", ErrorCode::LoneSurrogate, 4),
        (br"good\uDC00", ErrorCode::LoneSurrogate, 4),
        (br"good\uD800\u0041", ErrorCode::LoneSurrogate, 10),
        (br"good\u12z4", ErrorCode::InvalidEscape, 8),
        (b"good\n", ErrorCode::ControlCharInString, 4),
        (b"good\"", ErrorCode::InvalidSyntax, 4),
        (b"good\xc0\x80", ErrorCode::InvalidUtf8, 4),
        (b"good\xf0\x90", ErrorCode::InvalidUtf8, 4),
        (b"good\\", ErrorCode::UnexpectedEof, 5),
        (br"good\u12", ErrorCode::UnexpectedEof, 8),
    ];
    for &(input, code, relative) in cases {
        let mut arena = DecodedArena::new(4096);
        arena.append(b"keep").unwrap();
        let error = decode_escaped_slice_into(input, 100, &mut arena, 4096).unwrap_err();
        assert_eq!(
            (error.code, error.byte_offset),
            (code, 100 + relative),
            "{input:?}"
        );
        assert_eq!(arena.as_slice(), b"keep");
    }
}

#[test]
fn byte_limits_utf8_error_order_and_arena_rollback() {
    let mut arena = DecodedArena::new(4096);
    arena.append(b"keep").unwrap();
    for (input, limit, offset) in [
        (&b"ab\xc0\x80"[..], 1, 1),
        (&b"a\xc0\x80"[..], 1, 1),
        (&b"a\xe2\x82\xac"[..], 3, 1),
        (&br"a\u20ac"[..], 3, 1),
    ] {
        let error = decode_escaped_slice_into(input, 30, &mut arena, limit).unwrap_err();
        assert_eq!(error.byte_offset, 30 + offset);
        if input == b"a\xc0\x80" {
            assert_eq!(error.code, ErrorCode::InvalidUtf8);
        } else {
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
        }
        assert_eq!(arena.as_slice(), b"keep");
    }
    let mut small = DecodedArena::new(7);
    small.append(b"keep").unwrap();
    let error = decode_escaped_slice_into(br"ab\ncd", 50, &mut small, 100).unwrap_err();
    assert_eq!(error.code, ErrorCode::DecodedArenaOverflow);
    assert_eq!(small.as_slice(), b"keep");
    assert!(decode_escaped_slice_into(b"x", u32::MAX, &mut arena, 100).is_err());
    assert_eq!(arena.as_slice(), b"keep");
}

#[test]
fn escape_masks_match_every_byte_value_in_every_position() {
    for position in 0..64 {
        for byte in 0..=255u8 {
            let mut input = [b'x'; 64];
            input[position] = byte;
            let masks = escape_masks_64(&input);
            let bit = 1u64 << position;
            assert_eq!(masks.quote_mask, if byte == b'"' { bit } else { 0 });
            assert_eq!(masks.backslash_mask, if byte == b'\\' { bit } else { 0 });
            assert_eq!(masks.control_mask, if byte < 32 { bit } else { 0 });
            assert_eq!(masks.non_ascii_mask, if byte >= 128 { bit } else { 0 });
            let required = byte == b'"' || byte == b'\\' || byte < 32;
            assert_eq!(escape_mask_64(&input), if required { bit } else { 0 });
            assert_eq!(
                first_escape_or_control(&input),
                if required { position } else { 64 }
            );
        }
    }
    for len in 0..=129 {
        let mut input = vec![b'x'; len];
        assert_eq!(first_escape_or_control(&input), len);
        if len != 0 {
            input[len - 1] = 0;
            assert_eq!(first_escape_or_control(&input), len - 1);
        }
    }
}

#[test]
fn dense_escape_decode_crosses_every_block_alignment() {
    let raw = br#"x\n\"\u0061\uD83E\uDD80"#;
    let cooked = "x\n\"a🦀";
    for padding in 0..64 {
        let mut input = vec![b'a'; padding];
        let mut expected = "a".repeat(padding);
        for _ in 0..40 {
            input.extend_from_slice(raw);
            input.extend_from_slice("€".as_bytes());
            expected.push_str(cooked);
            expected.push('€');
        }
        let mut arena = DecodedArena::new(8192);
        arena.append(b"keep:").unwrap();
        let reference = decode_escaped_slice_into(&input, 77, &mut arena, 8192).unwrap();
        assert_eq!(
            reference.span.slice(arena.as_slice()),
            Some(expected.as_bytes())
        );
        assert_eq!(reference.scalar_len as usize, expected.chars().count());
        assert_eq!(&arena.as_slice()[..5], b"keep:");
        let span = jmax_string::Span32::new(0, input.len() as u32);
        let measured = jmax_string::length::decoded_length(&input, span).unwrap();
        let mut sink = b"keep:".to_vec();
        let emitted = jmax_string::unescape::unescape(&input, span, &mut sink).unwrap();
        assert_eq!(measured, emitted);
        assert_eq!(emitted.byte_len as usize, expected.len());
        assert_eq!(emitted.scalar_len as usize, expected.chars().count());
        assert_eq!(&sink[5..], expected.as_bytes());
        let mut token = vec![b'"'];
        token.extend_from_slice(&input);
        token.extend_from_slice(b"\"suffix");
        let scanned = jmax_string::scan::scan_string(&token, 0).unwrap();
        assert_eq!(scanned.content.slice(&token), Some(input.as_slice()));
        assert_eq!(scanned.next_offset as usize, input.len() + 2);
        input.extend_from_slice(br"\q");
        let checkpoint = arena.as_slice().to_vec();
        assert!(decode_escaped_slice_into(&input, 77, &mut arena, 8192).is_err());
        assert_eq!(arena.as_slice(), checkpoint.as_slice());
    }
}
