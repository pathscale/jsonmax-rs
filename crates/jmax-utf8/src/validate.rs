//! Complete, allocation-free UTF-8 validation through safe backend facades.
//!
//! Backend selection is delegated to `ps-simdutf8` when the `simd-utf8`
//! feature is enabled. Otherwise validation uses `core` directly. Both paths
//! preserve the input borrow and report the first malformed sequence's start.

use super::{checked_len, Error, ErrorCode, ErrorSource, Result, Utf8Summary};

/// Validate an entire slice and count its bytes and Unicode scalar values.
///
/// Empty input is valid and ASCII-only. Counts include control characters,
/// byte-order marks, and combining characters without normalization or JSON
/// escape processing. A non-BMP scalar counts once, regardless of its width.
///
/// # Errors
/// Returns `PayloadTooLarge` before validation if the input exceeds the ABI's
/// 32-bit length limit. Invalid or incomplete UTF-8 returns `InvalidUtf8` at
/// the leading byte of the first malformed sequence.
pub fn validate(bytes: &[u8]) -> Result<Utf8Summary> {
    let text = validate_str(bytes)?;
    let byte_len = checked_len(text.len())?;
    let scalar_len = checked_len(text.chars().count())?;

    // In valid UTF-8 each scalar occupies one to four bytes. Equality holds
    // exactly when every scalar is ASCII, avoiding another pass over the data.
    Ok(Utf8Summary {
        byte_len,
        scalar_len,
        ascii_only: scalar_len == byte_len,
    })
}

/// Validate an entire slice and borrow it as a string without allocation.
///
/// The returned string covers the exact input bytes with the input's lifetime.
/// No byte-order mark is stripped and no replacement or decoding is performed.
///
/// # Errors
/// Returns `PayloadTooLarge` for input longer than `u32::MAX` before invoking
/// the backend. Invalid or incomplete UTF-8 returns `InvalidUtf8`, with source
/// `Utf8` and the same byte offset as `core::str::Utf8Error::valid_up_to()`.
pub fn validate_str(bytes: &[u8]) -> Result<&str> {
    checked_len(bytes.len())?;

    #[cfg(feature = "simd-utf8")]
    let validated = ps_simdutf8::compat::from_utf8(bytes);
    #[cfg(not(feature = "simd-utf8"))]
    let validated = core::str::from_utf8(bytes);

    match validated {
        Ok(text) => Ok(text),
        Err(error) => Err(Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            checked_len(error.valid_up_to())?,
            if error.error_len().is_none() {
                "incomplete UTF-8 sequence"
            } else {
                "invalid UTF-8 sequence"
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_valid(bytes: &[u8], scalar_len: u32, ascii_only: bool) {
        let text = validate_str(bytes).expect("valid UTF-8");
        assert_eq!(text.as_bytes(), bytes);
        assert_eq!(text.as_ptr(), bytes.as_ptr());
        assert_eq!(
            validate(bytes).expect("valid summary"),
            Utf8Summary {
                byte_len: u32::try_from(bytes.len()).unwrap(),
                scalar_len,
                ascii_only,
            },
        );
    }

    fn assert_invalid(bytes: &[u8], offset: u32) {
        let error = validate_str(bytes).expect_err("invalid UTF-8");
        assert_eq!(error.code, ErrorCode::InvalidUtf8);
        assert_eq!(error.source, ErrorSource::Utf8);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
        assert_eq!(validate(bytes).unwrap_err(), error);
    }

    fn assert_matches_core(bytes: &[u8]) {
        match core::str::from_utf8(bytes) {
            Ok(text) => assert_valid(
                bytes,
                u32::try_from(text.chars().count()).unwrap(),
                bytes.is_ascii(),
            ),
            Err(error) => {
                assert_invalid(bytes, u32::try_from(error.valid_up_to()).unwrap());
            }
        }
    }

    #[test]
    fn empty_and_all_ascii_bytes() {
        assert_valid(b"", 0, true);
        assert_eq!(validate(b"").unwrap(), Utf8Summary::default());
        assert_valid(b"\0\t\n\r\x1f \"\\/\x7f", 10, true);

        let mut ascii = [0u8; 128];
        for (index, byte) in ascii.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        assert_valid(&ascii, 128, true);
    }

    #[test]
    fn scalar_encoding_boundaries_and_noncharacters_are_valid() {
        let cases: &[(&[u8], char)] = &[
            (b"\x00", '\u{0}'),
            (b"\x7f", '\u{7f}'),
            (b"\xc2\x80", '\u{80}'),
            (b"\xdf\xbf", '\u{7ff}'),
            (b"\xe0\xa0\x80", '\u{800}'),
            (b"\xed\x9f\xbf", '\u{d7ff}'),
            (b"\xee\x80\x80", '\u{e000}'),
            (b"\xef\xb7\x90", '\u{fdd0}'),
            (b"\xef\xbf\xbe", '\u{fffe}'),
            (b"\xef\xbf\xbf", '\u{ffff}'),
            (b"\xf0\x90\x80\x80", '\u{10000}'),
            (b"\xf4\x8f\xbf\xbf", '\u{10ffff}'),
        ];
        for &(bytes, scalar) in cases {
            assert_valid(bytes, 1, scalar.is_ascii());
            assert_eq!(validate_str(bytes).unwrap().chars().next(), Some(scalar));
        }
    }

    #[test]
    fn counts_scalars_without_normalizing_or_decoding() {
        assert_valid("A\u{e9}\u{20ac}\u{1f600}".as_bytes(), 4, false);
        assert_valid("e\u{301}".as_bytes(), 2, false);
        assert_valid("\u{1f469}\u{200d}\u{1f4bb}".as_bytes(), 3, false);
        assert_valid("\u{feff}x\u{feff}".as_bytes(), 3, false);
        assert_valid("\u{fffd}".as_bytes(), 1, false);
        assert_valid(br"\uD800\uDC00", 12, true);
    }

    #[test]
    fn returned_string_borrows_only_the_requested_subslice() {
        fn borrow<'a>(input: &'a [u8]) -> Result<&'a str> {
            validate_str(input)
        }

        let storage = [0xff, b'a', 0xc2, 0xa2, 0xfe];
        let bytes = &storage[1..4];
        let text = borrow(bytes).unwrap();
        assert_eq!(text, "a\u{a2}");
        assert_eq!(text.as_ptr(), bytes.as_ptr());
        assert_eq!(text.len(), bytes.len());
        assert_valid(bytes, 2, false);
        assert_valid(&storage[2..2], 0, true);
        assert_invalid(&storage, 0);
        assert_invalid(&storage[1..], 3);
    }

    #[test]
    fn rejects_illegal_leaders_and_isolated_continuations() {
        for byte in 0x80u8..=0xff {
            assert_invalid(&[byte], 0);
            assert_invalid(&[b'a', byte], 1);
        }
        for byte in 0xf5u8..=0xff {
            assert_invalid(&[byte, 0x80, 0x80, 0x80], 0);
        }
    }

    #[test]
    fn rejects_overlong_surrogate_and_out_of_range_encodings() {
        let invalid: &[&[u8]] = &[
            b"\xc0\x80",
            b"\xc1\xbf",
            b"\xe0\x80\x80",
            b"\xe0\x9f\xbf",
            b"\xf0\x80\x80\x80",
            b"\xf0\x8f\xbf\xbf",
            b"\xed\xa0\x80",
            b"\xed\xaf\xbf",
            b"\xed\xb0\x80",
            b"\xed\xbf\xbf",
            b"\xed\xa0\x80\xed\xb0\x80",
            b"\xf4\x90\x80\x80",
            b"\xf4\xbf\xbf\xbf",
            b"\xf5\x80\x80\x80",
            b"\xf8\x88\x80\x80\x80",
            b"\xfc\x84\x80\x80\x80\x80",
        ];
        for &bytes in invalid {
            assert_invalid(bytes, 0);
            let mut prefixed = [b'a'; 16];
            prefixed[3..3 + bytes.len()].copy_from_slice(bytes);
            assert_invalid(&prefixed[..3 + bytes.len()], 3);
        }
    }

    #[test]
    fn rejects_bad_continuations_at_every_position() {
        let sequences: &[&[u8]] = &[
            b"\xc2\xa2",
            b"\xe2\x82\xac",
            b"\xf0\x9f\x98\x80",
        ];
        for &sequence in sequences {
            for position in 1..sequence.len() {
                for replacement in [0x00, 0x7f, 0xc0, 0xc2, 0xe0, 0xf0, 0xff] {
                    let mut bytes = [b'a'; 8];
                    bytes[2..2 + sequence.len()].copy_from_slice(sequence);
                    bytes[2 + position] = replacement;
                    assert_invalid(&bytes[..2 + sequence.len()], 2);
                }
            }
        }
    }

    #[test]
    fn incomplete_sequences_report_their_start_instead_of_eof() {
        let sequences: &[&[u8]] = &[
            b"\xc2\x80",
            b"\xdf\xbf",
            b"\xe0\xa0\x80",
            b"\xed\x9f\xbf",
            b"\xef\xbf\xbf",
            b"\xf0\x90\x80\x80",
            b"\xf4\x8f\xbf\xbf",
        ];
        for &sequence in sequences {
            for end in 1..sequence.len() {
                assert_invalid(&sequence[..end], 0);
                let mut bytes = [b'a'; 8];
                bytes[3..3 + end].copy_from_slice(&sequence[..end]);
                assert_invalid(&bytes[..3 + end], 3);
                assert_eq!(
                    validate_str(&bytes[..3 + end]).unwrap_err().detail,
                    "incomplete UTF-8 sequence",
                );
            }
        }
        // These prefixes are already impossible, even though they are short.
        for bytes in [b"\xe0\x9f".as_slice(), b"\xed\xa0", b"\xf4\x90"] {
            assert_invalid(bytes, 0);
            assert_eq!(validate_str(bytes).unwrap_err().detail, "invalid UTF-8 sequence");
        }
    }

    #[test]
    fn first_error_offsets_count_bytes_in_multibyte_prefixes() {
        assert_invalid(b"\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80\xff", 9);
        assert_invalid(b"\xc2\xa2\xe2\x82\xac\xf0\x9f", 5);
        assert_invalid(b"a\xe2\x82b\xff", 1);
        assert_invalid(b"a\x80b\xc0\x80", 1);
        assert_invalid(b"\xf0\x9f\x98\x80\x80", 4);
    }

    #[test]
    fn block_boundaries_alignment_and_tails_preserve_results() {
        let sequences: &[&[u8]] = &[
            b"\xc2\xa2",
            b"\xe2\x82\xac",
            b"\xf0\x9f\x98\x80",
        ];
        let mut storage = [b'a'; 320];
        for alignment in 0..16 {
            for start in 0..=129 {
                for &sequence in sequences {
                    storage.fill(b'a');
                    let end = start + sequence.len();
                    storage[alignment + start..alignment + end].copy_from_slice(sequence);
                    let bytes = &storage[alignment..alignment + end];
                    assert_valid(bytes, u32::try_from(start + 1).unwrap(), false);
                    for tail in 1..sequence.len() {
                        assert_invalid(&bytes[..start + tail], u32::try_from(start).unwrap());
                    }
                    assert_valid(
                        &storage[alignment..alignment + end + 65],
                        u32::try_from(start + 66).unwrap(),
                        false,
                    );
                    storage[alignment + end - 1] = b'!';
                    assert_invalid(
                        &storage[alignment..alignment + end + 65],
                        u32::try_from(start).unwrap(),
                    );
                }
            }
        }
    }

    #[test]
    fn every_one_and_two_byte_input_matches_core() {
        for first in 0u8..=255 {
            assert_matches_core(&[first]);
            for second in 0u8..=255 {
                assert_matches_core(&[first, second]);
            }
        }
    }

    #[test]
    fn deterministic_mutations_of_long_valid_text_match_core() {
        let pattern = b"a\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80";
        let mut bytes = [0u8; 260];
        for chunk in bytes.chunks_exact_mut(pattern.len()) {
            chunk.copy_from_slice(pattern);
        }
        assert_valid(&bytes, 104, false);
        for position in 0..bytes.len() {
            let original = bytes[position];
            for replacement in [0x00, 0x7f, 0x80, 0xbf, 0xc0, 0xc2, 0xe0, 0xed, 0xf4, 0xff] {
                bytes[position] = replacement;
                assert_matches_core(&bytes);
                assert_matches_core(&bytes[..position + 1]);
            }
            bytes[position] = original;
        }
    }

    #[test]
    fn checked_length_accepts_representable_boundaries() {
        assert_eq!(checked_len(0).unwrap(), 0);
        assert_eq!(checked_len(1).unwrap(), 1);
        if let Ok(maximum) = usize::try_from(u32::MAX) {
            assert_eq!(checked_len(maximum).unwrap(), u32::MAX);
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn oversized_lengths_use_the_canonical_error_without_large_allocations() {
        // Exercise the entry points' shared guard directly: constructing a
        // fictitious oversized slice would violate the safe-only contract.
        let maximum = usize::try_from(u32::MAX).unwrap();
        for length in [maximum + 1, maximum + 2, usize::MAX] {
            let error = checked_len(length).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            assert_eq!(error.source, ErrorSource::Utf8);
            assert_eq!(error.byte_offset, u32::MAX);
            assert_eq!((error.schema_id, error.field_id), (0, 0));
            assert!(!error.detail.is_empty());
        }
    }
}
