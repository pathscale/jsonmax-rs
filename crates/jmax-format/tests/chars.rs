use jmax_format::{registry, FormatFailureKind, FormatId, FormatOptions};

#[test]
fn nanoid_chars_and_base62() {
    let options = FormatOptions::default();
    assert_eq!(registry::lookup("nanoid-chars"), Ok(FormatId::NanoIdChars));
    assert_eq!(registry::lookup("base62"), Ok(FormatId::Base62));
    for id in [FormatId::NanoIdChars, FormatId::Base62] {
        assert_eq!(FormatId::from_u16(id as u16), Some(id));
        assert_eq!(registry::lookup(id.name()), Ok(id));
        // Nonempty only, at any length.
        assert_eq!(registry::validate(id, b"", &options).unwrap_err().kind, FormatFailureKind::InvalidLength);
        for length in [1usize, 16, 21, 22, 64, 1000] {
            assert!(registry::validate(id, &vec![b'a'; length], &options).is_ok());
        }
        for bad in [b' ', b'.', b'/', b'+', 0x80, 0xff, 0] {
            let failure = registry::validate(id, &[b'a', bad, b'b'], &options).unwrap_err();
            assert_eq!((failure.kind, failure.index), (FormatFailureKind::InvalidByte, 1), "{bad}");
        }
    }
    // The alphabets differ exactly in '_' and '-'.
    assert!(registry::validate(FormatId::NanoIdChars, b"a_b-C9", &options).is_ok());
    assert!(registry::validate(FormatId::Base62, b"aC9zZ0", &options).is_ok());
    for byte in [b'_', b'-'] {
        assert!(registry::validate(FormatId::Base62, &[b'a', byte], &options).is_err());
    }
    // Every byte value agrees with the definition.
    for byte in 0..=255u8 {
        let url = byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
        assert_eq!(registry::validate(FormatId::NanoIdChars, &[byte], &options).is_ok(), url);
        assert_eq!(registry::validate(FormatId::Base62, &[byte], &options).is_ok(), byte.is_ascii_alphanumeric());
    }
    assert_eq!(FormatId::from_u16(13), None);
}
