use jmax_buffer::DecodedArena;
use jmax_string::{
    ErrorCode, ErrorSource, Span32, SpanSource, cook_into, decode_escaped_slice_into,
    scan::scan_string, unescape::decode_escape, unescape::unescape,
};

#[test]
fn scan_finds_boundaries_without_accepting_bad_escape_spellings() {
    let input = br#"prefix:"a\\\"b\q"tail"#;
    let scan = scan_string(input, 7).unwrap();
    assert_eq!(scan.raw.slice(input), Some(&input[7..17]));
    assert_eq!(scan.content.slice(input), Some(br#"a\\\"b\q"#.as_slice()));
    assert_eq!(scan.first_escape, Some(9));
    assert_eq!(scan.next_offset, 17);

    let error = decode_escape(input, 14).unwrap_err();
    assert_eq!((error.code, error.source, error.byte_offset),
        (ErrorCode::InvalidEscape, ErrorSource::Decode, 15));
}

#[test]
fn decode_escape_handles_simple_unicode_and_surrogate_escapes() {
    let cases: &[(&[u8], char, u8)] = &[
        (br#"\""#, '"', 2),
        (br#"\\"#, '\\', 2),
        (br#"\/"#, '/', 2),
        (br"\b", '\u{8}', 2),
        (br"\f", '\u{c}', 2),
        (br"\n", '\n', 2),
        (br"\r", '\r', 2),
        (br"\t", '\t', 2),
        (br"\u20aC", '€', 6),
        (br"\uD83D\uDE00", '😀', 12),
    ];
    for &(bytes, scalar, consumed) in cases {
        let decoded = decode_escape(bytes, 0).unwrap();
        assert_eq!((decoded.scalar, decoded.consumed), (scalar, consumed), "{bytes:?}");
    }

    for (bytes, code, offset) in [
        (&br"\q"[..], ErrorCode::InvalidEscape, 1),
        (&br"\u12x4"[..], ErrorCode::InvalidEscape, 4),
        (&br"\uD800"[..], ErrorCode::LoneSurrogate, 0),
        (&br"\uDC00"[..], ErrorCode::LoneSurrogate, 0),
        (&br"\uD800\u0041"[..], ErrorCode::LoneSurrogate, 6),
        (&br"\u12"[..], ErrorCode::UnexpectedEof, 4),
    ] {
        let error = decode_escape(bytes, 0).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (code, offset), "{bytes:?}");
    }
    let eof = decode_escape(b"x", 9).unwrap_err();
    assert_eq!((eof.code, eof.byte_offset), (ErrorCode::UnexpectedEof, 1));
}

#[test]
fn unescape_appends_once_with_decoded_byte_and_scalar_lengths() {
    let input = r#"skip:a\n€\uD83D\uDE00\u0000"#.as_bytes();
    let content = Span32::new(5, (input.len() - 5) as u32);
    let mut sink = b"prefix:".to_vec();
    let lengths = unescape(input, content, &mut sink).unwrap();
    let expected = "a\n€😀\0";
    assert_eq!(lengths.byte_len as usize, expected.len());
    assert_eq!(lengths.scalar_len, expected.chars().count() as u32);
    assert_eq!(&sink[..7], b"prefix:");
    assert_eq!(&sink[7..], expected.as_bytes());

    let empty = unescape(b"", Span32::new(0, 0), &mut sink).unwrap();
    assert_eq!((empty.byte_len, empty.scalar_len), (0, 0));
    assert_eq!(&sink[7..], expected.as_bytes());
}

#[test]
fn invalid_content_is_transactional_for_unescape_and_cook_into_reuse() {
    let mut sink = b"keep".to_vec();
    for (content, code, offset) in [
        (&br"ok\q"[..], ErrorCode::InvalidEscape, 13),
        (&b"ok\""[..], ErrorCode::InvalidSyntax, 12),
        (&b"ok\n"[..], ErrorCode::ControlCharInString, 12),
        (&b"ok\xc0\x80"[..], ErrorCode::InvalidUtf8, 12),
    ] {
        let mut input = b"0123456789".to_vec();
        input.extend_from_slice(content);
        let error = unescape(&input, Span32::new(10, content.len() as u32), &mut sink).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (code, offset), "{content:?}");
        assert_eq!(sink, b"keep");
    }

    let mut arena = DecodedArena::new(128);
    arena.append(b"keep:").unwrap();
    let first = decode_escaped_slice_into(br"one\n", 20, &mut arena, 64).unwrap();
    assert_eq!(first.source, SpanSource::DecodedArena);
    assert_eq!(first.span.slice(arena.as_slice()), Some(&b"one\n"[..]));
    let second = decode_escaped_slice_into(br"\uD83D\uDE00", 30, &mut arena, 64).unwrap();
    assert_eq!(second.span.slice(arena.as_slice()), Some("😀".as_bytes()));
    assert_eq!(second.scalar_len, 1);
    let checkpoint = arena.as_slice().to_vec();
    let error = decode_escaped_slice_into(br"valid\n\uD800", 40, &mut arena, 64).unwrap_err();
    assert_eq!((error.code, error.byte_offset), (ErrorCode::LoneSurrogate, 47));
    assert_eq!(arena.as_slice(), checkpoint);

    let reused = cook_into(b"z", 50, &mut arena, 64).unwrap();
    assert_eq!(reused.span.offset, checkpoint.len() as u32);
    assert_eq!(reused.span.slice(arena.as_slice()), Some(&b"z"[..]));
}
