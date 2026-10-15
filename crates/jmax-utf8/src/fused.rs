//! Safe integration of scanner classifications with incremental UTF-8 validation.
//!
//! Public masks are untrusted metadata. Only the provided slice is read, and
//! its high bits are checked before the ordinary stream validator is called.
//! This adapter allocates nothing and requires no optional backend dependency.

use super::{BlockMasks, Error, ErrorCode, ErrorSource, Result, Utf8Stream, BLOCK_BYTES};

/// Validate one classified block of at most 64 actual bytes.
///
/// `valid_len` must equal `bytes.len()`, `valid_mask` must contain exactly that
/// many low bits, and `non_ascii_mask` must exactly describe the bytes whose
/// high bit is set. Other classifications and the backend tag are ignored.
/// Masks never authorize reads beyond `bytes` or establish a borrowed proof.
///
/// Once metadata is accepted, this is exactly [`super::stream::validate_chunk`],
/// including validation of an incoming incomplete scalar for an ASCII block.
/// Call [`super::stream::finish`] to reject a truncated final sequence.
///
/// # Errors
/// Oversized blocks or mismatched metadata return `InvalidToken`, source
/// `Utf8`, at the current stream byte offset, without changing any state.
/// These checks precede saved errors, finalization, and cumulative overflow.
/// With accepted metadata, stream errors and their poisoning behavior apply.
pub fn validate_classified(
    bytes: &[u8],
    masks: &BlockMasks,
    state: &mut Utf8Stream,
) -> Result<()> {
    if bytes.len() > BLOCK_BYTES || usize::from(masks.valid_len) != bytes.len() {
        return Err(invalid_metadata(state, "UTF-8 block length does not match classification"));
    }

    // Handle the full block explicitly so a shift by 64 is never evaluated.
    let valid_mask = if bytes.len() == BLOCK_BYTES {
        u64::MAX
    } else {
        (1u64 << bytes.len()) - 1
    };
    if masks.valid_mask != valid_mask || masks.non_ascii_mask & !valid_mask != 0 {
        return Err(invalid_metadata(state, "UTF-8 classification has invalid occupied lanes"));
    }

    let mut non_ascii_mask = 0u64;
    for (index, &byte) in bytes.iter().enumerate() {
        // The length guard bounds index to 0..64, including unaligned tails.
        if !byte.is_ascii() {
            non_ascii_mask |= 1u64 << index;
        }
    }
    if masks.non_ascii_mask != non_ascii_mask {
        return Err(invalid_metadata(state, "UTF-8 classification does not match byte high bits"));
    }

    super::stream::validate_chunk(state, bytes)
}

fn invalid_metadata(state: &Utf8Stream, detail: &'static str) -> Error {
    Error::new(ErrorCode::InvalidToken, ErrorSource::Utf8, state.bytes_seen(), detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{proof, stream, validate, Utf8Summary};

    fn classify(bytes: &[u8]) -> BlockMasks {
        assert!(bytes.len() <= BLOCK_BYTES);
        let mut masks = BlockMasks {
            valid_len: u8::try_from(bytes.len()).unwrap(),
            ..BlockMasks::default()
        };
        // Build occupied lanes individually, independently of the adapter's
        // low-bit mask formula, including the empty and full-width cases.
        for (index, &byte) in bytes.iter().enumerate() {
            masks.valid_mask |= 1u64 << index;
            if byte >= 0x80 {
                masks.non_ascii_mask |= 1u64 << index;
            }
        }
        masks
    }

    fn feed(state: &mut Utf8Stream, bytes: &[u8]) -> Result<()> {
        validate_classified(bytes, &classify(bytes), state)
    }

    fn assert_accepted_eq(actual: &Utf8Stream, expected: &Utf8Stream) {
        assert_eq!(actual.carry, expected.carry);
        assert_eq!(actual.total_bytes, expected.total_bytes);
        assert_eq!(actual.total_scalars, expected.total_scalars);
        assert_eq!(actual.ascii_only, expected.ascii_only);
        assert_eq!(actual.finished, expected.finished);
    }

    fn assert_state_eq(actual: &Utf8Stream, expected: &Utf8Stream) {
        assert_accepted_eq(actual, expected);
        assert_eq!(actual.failure, expected.failure);
    }

    fn assert_error(error: &Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Utf8);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_rejected(bytes: &[u8], masks: &BlockMasks, state: &mut Utf8Stream) {
        let before = state.clone();
        let error = validate_classified(bytes, masks, state).unwrap_err();
        assert_error(&error, ErrorCode::InvalidToken, before.bytes_seen());
        assert_state_eq(state, &before);
    }

    fn compare_chunk(fused: &mut Utf8Stream, ordinary: &mut Utf8Stream, bytes: &[u8]) {
        assert_eq!(feed(fused, bytes), stream::validate_chunk(ordinary, bytes));
        assert_state_eq(fused, ordinary);
        assert!(fused.scalars_seen() <= fused.bytes_seen());
        let pending_len = fused.pending_bytes().len();
        assert!(pending_len <= crate::MAX_PENDING_BYTES);
        assert!(fused.carry.utf8_tail[pending_len..].iter().all(|&byte| byte == 0));
    }

    fn compare_partition(bytes: &[u8], first: usize, second: usize) {
        let mut fused = Utf8Stream::new();
        fused.carry.prev_in_string = u64::MAX;
        fused.carry.prev_escaped = 0x1234_5678;
        let mut ordinary = fused.clone();
        for chunk in [&bytes[..first], &bytes[first..second], &bytes[second..]] {
            // Longer inputs still enter the adapter as actual blocks.
            for block in chunk.chunks(BLOCK_BYTES) {
                compare_chunk(&mut fused, &mut ordinary, block);
            }
            compare_chunk(&mut fused, &mut ordinary, b"");
        }
        let actual = stream::finish(&mut fused);
        assert_eq!(actual, stream::finish(&mut ordinary));
        assert_state_eq(&fused, &ordinary);
        match core::str::from_utf8(bytes) {
            Ok(text) => {
                let summary = actual.unwrap();
                assert_eq!(summary, Utf8Summary {
                    byte_len: u32::try_from(bytes.len()).unwrap(),
                    scalar_len: u32::try_from(text.chars().count()).unwrap(),
                    ascii_only: bytes.is_ascii(),
                });
                assert_eq!(summary, validate::validate(bytes).unwrap());
                let proof = proof::validate(bytes).unwrap();
                assert_eq!(summary, proof.summary());
                assert_eq!(proof.as_bytes().as_ptr(), bytes.as_ptr());
            }
            Err(expected) => assert_error(
                &actual.unwrap_err(),
                ErrorCode::InvalidUtf8,
                u32::try_from(expected.valid_up_to()).unwrap(),
            ),
        }
        assert_eq!(fused.carry.prev_in_string, u64::MAX);
        assert_eq!(fused.carry.prev_escaped, 0x1234_5678);
        assert_eq!(stream::finish(&mut fused), stream::finish(&mut ordinary));
        compare_chunk(&mut fused, &mut ordinary, b"after finish");
    }

    #[test]
    fn every_block_length_and_alignment_reads_only_selected_bytes() {
        let mut storage = [0xff; 144];
        for alignment in 0..64 {
            for len in 0..=BLOCK_BYTES {
                storage.fill(0xff);
                for (index, byte) in storage[alignment..alignment + len].iter_mut().enumerate() {
                    *byte = u8::try_from((alignment + index) % 128).unwrap();
                }
                let bytes = &storage[alignment..alignment + len];
                let mut state = Utf8Stream::new();
                feed(&mut state, bytes).unwrap();
                assert_eq!(stream::finish(&mut state).unwrap(), Utf8Summary {
                    byte_len: u32::try_from(len).unwrap(),
                    scalar_len: u32::try_from(len).unwrap(),
                    ascii_only: true,
                });
            }
        }
    }

    #[test]
    fn every_metadata_length_and_every_mask_bit_are_checked() {
        let mut storage = [b'a'; BLOCK_BYTES];
        for (index, byte) in storage.iter_mut().enumerate() {
            if index % 3 == 1 {
                *byte = 0x80;
            }
        }
        let mut state = Utf8Stream::new();
        feed(&mut state, b"abc\xf0\x9f").unwrap();
        state.carry.prev_in_string = 1;
        state.carry.prev_escaped = u64::MAX;
        for len in 0..=BLOCK_BYTES {
            let bytes = &storage[..len];
            let correct = classify(bytes);
            for declared in 0u8..=255 {
                if usize::from(declared) != len {
                    let bad = BlockMasks { valid_len: declared, ..correct };
                    assert_rejected(bytes, &bad, &mut state);
                }
            }
            for bit in 0..64 {
                let bad = BlockMasks { valid_mask: correct.valid_mask ^ (1u64 << bit), ..correct };
                assert_rejected(bytes, &bad, &mut state);
                let bad = BlockMasks {
                    non_ascii_mask: correct.non_ascii_mask ^ (1u64 << bit),
                    ..correct
                };
                assert_rejected(bytes, &bad, &mut state);
            }
        }
        // Metadata rejection must not poison or consume the pending scalar.
        feed(&mut state, b"\x98\x80").unwrap();
        assert_eq!(stream::finish(&mut state).unwrap().scalar_len, 4);
    }

    #[test]
    fn blocks_larger_than_64_are_rejected_without_consumption() {
        let storage = [b'a'; 257];
        let mut state = Utf8Stream::new();
        feed(&mut state, b"prefix").unwrap();
        for len in [65, 127, 128, 255, 256, 257] {
            for valid_len in [0, 1, 63, 64, 65, 127, 255] {
                let masks = BlockMasks { valid_len, valid_mask: u64::MAX, ..BlockMasks::default() };
                assert_rejected(&storage[..len], &masks, &mut state);
            }
        }
        feed(&mut state, b"ok").unwrap();
        assert_eq!(state.bytes_seen(), 8);
    }

    #[test]
    fn forged_ascii_and_truthful_non_ascii_masks_have_distinct_errors() {
        for position in 0..BLOCK_BYTES {
            let mut bytes = [b'a'; BLOCK_BYTES];
            bytes[position] = 0xff;
            let mut state = Utf8Stream::new();
            feed(&mut state, b"pre").unwrap();
            let correct = classify(&bytes);
            assert_eq!(correct.non_ascii_mask, 1u64 << position);
            let forged = BlockMasks { non_ascii_mask: 0, ..correct };
            assert_rejected(&bytes, &forged, &mut state);
            let before = state.clone();
            let error = validate_classified(&bytes, &correct, &mut state).unwrap_err();
            assert_error(&error, ErrorCode::InvalidUtf8, 3 + u32::try_from(position).unwrap());
            assert_accepted_eq(&state, &before);
            assert_eq!(state.failure(), Some(&error));
        }
    }

    #[test]
    fn unrelated_classifications_and_all_backend_tags_are_ignored() {
        for backend in 0u8..=255 {
            for bytes in [b"".as_slice(), b"ascii", b"\xc2\xa2", b"\xf0\x9f", b"\xff"] {
                let masks = BlockMasks {
                    quote_mask: u64::MAX,
                    backslash_mask: u64::MAX,
                    in_string_mask: u64::MAX,
                    colon_mask: u64::MAX,
                    comma_mask: u64::MAX,
                    lbrace_mask: u64::MAX,
                    rbrace_mask: u64::MAX,
                    lbracket_mask: u64::MAX,
                    rbracket_mask: u64::MAX,
                    structural_mask: u64::MAX,
                    whitespace_mask: u64::MAX,
                    control_mask: u64::MAX,
                    digit_mask: u64::MAX,
                    backend,
                    ..classify(bytes)
                };
                let original = masks;
                let mut fused = Utf8Stream::new();
                let mut ordinary = fused.clone();
                assert_eq!(validate_classified(bytes, &masks, &mut fused),
                    stream::validate_chunk(&mut ordinary, bytes));
                assert_state_eq(&fused, &ordinary);
                assert_eq!(masks, original);
            }
        }
    }

    #[test]
    fn metadata_errors_precede_terminal_states_without_replacing_saved_errors() {
        for mode in 0..4 {
            let mut state = Utf8Stream::new();
            feed(&mut state, b"abc").unwrap();
            match mode {
                0 => { feed(&mut state, b"\xf0\x9f").unwrap(); }
                1 => { feed(&mut state, b"\xff").unwrap_err(); }
                2 => {
                    feed(&mut state, b"\xe2").unwrap();
                    stream::finish(&mut state).unwrap_err();
                }
                _ => { stream::finish(&mut state).unwrap(); }
            }
            assert_rejected(b"x", &BlockMasks::default(), &mut state);
            assert_rejected(b"", &classify(b"x"), &mut state);
            assert_rejected(b"\xff", &classify(b"a"), &mut state);
            assert_rejected(&[b'a'; 65], &classify(&[b'a'; 64]), &mut state);
            let mut ordinary = state.clone();
            for bytes in [b"".as_slice(), b"\x98\x80", b"\xff", b"a"] {
                compare_chunk(&mut state, &mut ordinary, bytes);
            }
            state.reset();
            assert_state_eq(&state, &Utf8Stream::new());
            feed(&mut state, b"ok").unwrap();
            assert_eq!(stream::finish(&mut state).unwrap(), Utf8Summary {
                byte_len: 2, scalar_len: 2, ascii_only: true,
            });
        }
    }

    #[test]
    fn ascii_never_bypasses_a_pending_non_ascii_prefix() {
        for sequence in [b"\xc2\xa2".as_slice(), b"\xe2\x82\xac", b"\xf0\x9f\x98\x80"] {
            for end in 1..sequence.len() {
                let mut state = Utf8Stream::new();
                feed(&mut state, b"abc").unwrap();
                feed(&mut state, &sequence[..end]).unwrap();
                let before = state.clone();
                feed(&mut state, b"").unwrap();
                assert_state_eq(&state, &before);
                let error = feed(&mut state, b"a").unwrap_err();
                assert_error(&error, ErrorCode::InvalidUtf8, 3);
                assert_accepted_eq(&state, &before);
                assert_eq!(feed(&mut state, &sequence[end..]).unwrap_err(), error);
                assert_eq!(stream::finish(&mut state).unwrap_err(), error);
            }
        }
    }

    #[test]
    fn pending_completion_is_rolled_back_when_later_bytes_fail() {
        let mut state = Utf8Stream::new();
        feed(&mut state, b"abc\xf0\x9f").unwrap();
        let before = state.clone();
        let error = feed(&mut state, b"\x98\x80de\xe2\x82!").unwrap_err();
        assert_error(&error, ErrorCode::InvalidUtf8, 9);
        assert_accepted_eq(&state, &before);
        assert_eq!(state.pending_bytes(), b"\xf0\x9f");
        assert_eq!(state.failure(), Some(&error));
        let poisoned = state.clone();
        for bytes in [b"".as_slice(), b"ok", b"\xff"] {
            assert_eq!(feed(&mut state, bytes).unwrap_err(), error);
            assert_state_eq(&state, &poisoned);
        }
    }

    #[test]
    fn fused_and_ordinary_chunks_share_one_carry_and_clear_unused_tail() {
        let mut state = Utf8Stream::new();
        stream::validate_chunk(&mut state, b"a\xf0\x9f\x98").unwrap();
        feed(&mut state, b"\x80b\xc2").unwrap();
        assert_eq!(state.carry.utf8_tail, [0xc2, 0, 0, 0]);
        assert_eq!(state.scalars_seen(), 3);
        stream::validate_chunk(&mut state, b"\xa2\xe2").unwrap();
        feed(&mut state, b"\x82").unwrap();
        assert_eq!(state.pending_bytes(), b"\xe2\x82");
        feed(&mut state, b"\xac").unwrap();
        assert_eq!(state.carry.utf8_tail, [0; 4]);
        assert_eq!(state.carry.utf8_tail_len, 0);
        assert_eq!(stream::finish(&mut state).unwrap(), Utf8Summary {
            byte_len: 11, scalar_len: 5, ascii_only: false,
        });
    }

    #[test]
    fn valid_scalar_limits_and_malformed_vectors_match_every_partition() {
        let vectors: &[&[u8]] = &[
            b"", b"\0\x7f\"\\\n", br"\uD800\uDC00",
            b"\xc2\x80", b"\xdf\xbf", b"\xe0\xa0\x80", b"\xed\x9f\xbf",
            b"\xee\x80\x80", b"\xef\xb7\x90", b"\xef\xbf\xbe", b"\xef\xbf\xbf",
            b"\xf0\x90\x80\x80", b"\xf4\x8f\xbf\xbf",
            "A\u{e9}\u{20ac}\u{1f600}Z".as_bytes(),
            "\u{feff}e\u{301}\u{fffd}".as_bytes(),
            b"\x80", b"\xbf", b"\xc0\x80", b"\xc1\xbf", b"\xe0\x9f",
            b"\xe0\x80\x80", b"\xed\xa0\x80", b"\xed\xbf\xbf",
            b"\xf0\x8f\xbf\xbf", b"\xf4\x90\x80\x80", b"\xf5\x80\x80\x80",
            b"\xf8\x88\x80\x80\x80", b"\xfe", b"\xff",
            b"\xc2", b"\xe2", b"\xe2\x82", b"\xf0", b"\xf0\x9f", b"\xf0\x9f\x98",
            b"a\xc2!", b"a\xe2!\xac", b"a\xe2\x82!\xff",
            b"a\xf0!\x98\x80", b"a\xf0\x9f!\x80", b"a\xf0\x9f\x98!",
            b"\xc2\xa2\xe2\x82\xac\xff\x80",
        ];
        for &bytes in vectors {
            for first in 0..=bytes.len() {
                for second in first..=bytes.len() {
                    compare_partition(bytes, first, second);
                }
            }
        }
    }

    #[test]
    fn every_one_and_two_byte_input_matches_core_and_stream() {
        for first in 0u8..=255 {
            compare_partition(&[first], 0, 1);
            for second in 0u8..=255 {
                compare_partition(&[first, second], 1, 1);
            }
        }
    }

    #[test]
    fn multibyte_sequences_cross_full_blocks_and_unaligned_tails() {
        let mut storage = [0xff; 208];
        for alignment in 0..8 {
            for start in [61, 62, 63, 64, 65, 125, 126, 127, 128, 129] {
                for sequence in [b"\xc2\xa2".as_slice(), b"\xe2\x82\xac", b"\xf0\x9f\x98\x80"] {
                    storage.fill(0xff);
                    let end = start + sequence.len() + 65;
                    storage[alignment..alignment + end].fill(b'a');
                    storage[alignment + start..alignment + start + sequence.len()]
                        .copy_from_slice(sequence);
                    let bytes = &storage[alignment..alignment + end];
                    compare_partition(bytes, 0, 0);
                    for split in start..=start + sequence.len() {
                        compare_partition(bytes, split, split);
                        compare_partition(&bytes[..split], 0, 0);
                    }
                    storage[alignment + start + sequence.len() - 1] = 0xff;
                    compare_partition(&storage[alignment..alignment + end], 0, 0);
                }
            }
        }
    }

    #[test]
    fn exact_abi_limit_overflow_and_metadata_precedence() {
        // Model a long accepted ASCII history without allocating a huge slice.
        let mut state = Utf8Stream::new();
        state.total_bytes = u32::MAX - 1;
        state.total_scalars = u32::MAX - 1;
        feed(&mut state, b"a").unwrap();
        feed(&mut state, b"").unwrap();
        assert_eq!(state.bytes_seen(), u32::MAX);
        assert_eq!(state.scalars_seen(), u32::MAX);
        for bytes in [b"a".as_slice(), b"\xff", b"\xf0\x9f\x98\x80"] {
            let mut overflow = state.clone();
            assert_rejected(bytes, &BlockMasks::default(), &mut overflow);
            let error = feed(&mut overflow, bytes).unwrap_err();
            assert_error(&error, ErrorCode::PayloadTooLarge, u32::MAX);
            assert_accepted_eq(&overflow, &state);
            assert_eq!(overflow.failure(), Some(&error));
            assert_rejected(b"a", &BlockMasks::default(), &mut overflow);
            assert_eq!(feed(&mut overflow, b"").unwrap_err(), error);
        }
        assert_eq!(stream::finish(&mut state).unwrap(), Utf8Summary {
            byte_len: u32::MAX, scalar_len: u32::MAX, ascii_only: true,
        });
        let before = state.clone();
        assert_error(&feed(&mut state, b"\xff").unwrap_err(), ErrorCode::InvalidToken, u32::MAX);
        assert_state_eq(&state, &before);
    }

    #[test]
    fn pending_prefix_offsets_remain_exact_near_the_abi_limit() {
        let mut state = Utf8Stream::new();
        state.total_bytes = u32::MAX - 4;
        state.total_scalars = u32::MAX - 4;
        feed(&mut state, b"\xf0\x9f\x98").unwrap();
        let before = state.clone();
        let mut truncated = before.clone();
        let error = stream::finish(&mut truncated).unwrap_err();
        assert_error(&error, ErrorCode::InvalidUtf8, u32::MAX - 4);
        assert_eq!(feed(&mut truncated, b"\x80").unwrap_err(), error);
        for (bytes, code, offset) in [
            (b"!".as_slice(), ErrorCode::InvalidUtf8, u32::MAX - 4),
            (b"!x".as_slice(), ErrorCode::PayloadTooLarge, u32::MAX),
        ] {
            let mut failed = before.clone();
            assert_error(&feed(&mut failed, bytes).unwrap_err(), code, offset);
            assert_accepted_eq(&failed, &before);
        }
        feed(&mut state, b"\x80").unwrap();
        assert_eq!(stream::finish(&mut state).unwrap(), Utf8Summary {
            byte_len: u32::MAX, scalar_len: u32::MAX - 3, ascii_only: false,
        });
    }
}
