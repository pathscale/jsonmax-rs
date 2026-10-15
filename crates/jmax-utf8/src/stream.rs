//! Allocation-free incremental UTF-8 validation with transactional chunks.
//!
//! Only a valid incomplete scalar prefix survives a successful call. Call
//! [`finish`] to distinguish a complete stream from a truncated final scalar.

use super::{
    checked_len, Error, ErrorCode, ErrorSource, Result, Utf8Stream, Utf8Summary,
    MAX_PENDING_BYTES, MAX_SEQUENCE_BYTES,
};

/// Validate a chunk, completing any scalar split across previous chunks.
///
/// Successful chunks advance the byte count once, including pending bytes;
/// scalars are counted only when complete. Empty chunks leave live states
/// unchanged. Scanner quote and escape carry are preserved.
///
/// # Errors
/// Malformed UTF-8 reports the absolute start of its first invalid sequence.
/// Cumulative lengths exceeding `u32::MAX` report `PayloadTooLarge` before
/// examining the bytes. Either failure preserves all accepted input state and
/// saves the error for subsequent calls. After successful finalization, every
/// chunk returns `InvalidToken` without changing the finalized state.
pub fn validate_chunk(state: &mut Utf8Stream, bytes: &[u8]) -> Result<()> {
    if let Some(error) = &state.failure {
        return Err(error.clone());
    }
    if state.finished {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Utf8,
            state.total_bytes,
            "UTF-8 stream is already finished",
        ));
    }
    if bytes.is_empty() {
        return Ok(());
    }

    // The state contains only fixed-size values. Staging it requires no
    // allocation and also rolls back completion of an incoming prefix if a
    // later byte in this same chunk is malformed.
    let mut next = state.clone();
    match accept_chunk(&mut next, bytes) {
        Ok(()) => {
            *state = next;
            Ok(())
        }
        Err(error) => {
            state.failure = Some(error.clone());
            Err(error)
        }
    }
}

/// Finalize a stream and return its complete byte and scalar counts.
///
/// Repeated successful calls return the same summary. A pending scalar fails
/// at its original leading byte, preserving accepted counters and carry and
/// saving that error. Failed streams return their first saved error until reset.
pub fn finish(state: &mut Utf8Stream) -> Result<Utf8Summary> {
    if let Some(error) = &state.failure {
        return Err(error.clone());
    }
    if state.carry.utf8_tail_len != 0 {
        let error = invalid_utf8(
            state.total_bytes - u32::from(state.carry.utf8_tail_len),
            "incomplete UTF-8 sequence",
        );
        state.failure = Some(error.clone());
        return Err(error);
    }
    state.finished = true;
    Ok(Utf8Summary {
        byte_len: state.total_bytes,
        scalar_len: state.total_scalars,
        ascii_only: state.ascii_only,
    })
}

fn invalid_utf8(offset: u32, detail: &'static str) -> Error {
    Error::new(ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset, detail)
}

fn length_overflow() -> Error {
    Error::new(
        ErrorCode::PayloadTooLarge,
        ErrorSource::Utf8,
        u32::MAX,
        "UTF-8 stream exceeds the 32-bit ABI length limit",
    )
}

fn add_scalars(state: &mut Utf8Stream, count: usize) -> Result<()> {
    state.total_scalars = state.total_scalars
        .checked_add(checked_len(count)?)
        .ok_or_else(length_overflow)?;
    Ok(())
}

/// Store a prefix already certified incomplete by `core::str::from_utf8`.
fn retain_prefix(state: &mut Utf8Stream, prefix: &[u8]) {
    debug_assert!(prefix.len() <= MAX_PENDING_BYTES);
    state.carry.utf8_tail.fill(0);
    state.carry.utf8_tail_len = 0;
    for &byte in prefix {
        state.carry.utf8_tail[usize::from(state.carry.utf8_tail_len)] = byte;
        state.carry.utf8_tail_len += 1;
    }
}

/// Mutate only the staged state; the caller publishes it on complete success.
fn accept_chunk(state: &mut Utf8Stream, bytes: &[u8]) -> Result<()> {
    let chunk_start = state.total_bytes;
    state.total_bytes = chunk_start
        .checked_add(checked_len(bytes.len())?)
        .ok_or_else(length_overflow)?;
    state.ascii_only &= bytes.is_ascii();

    let mut consumed = 0;
    let pending_len = usize::from(state.carry.utf8_tail_len);
    if pending_len != 0 {
        let prefix_start = chunk_start - u32::from(state.carry.utf8_tail_len);
        // Private stream state can contain only a valid incomplete prefix.
        // Its leader therefore determines a width of exactly two to four.
        let width = match state.carry.utf8_tail[0] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => MAX_SEQUENCE_BYTES,
        };
        let taken = core::cmp::min(width - pending_len, bytes.len());
        let mut sequence = [0u8; MAX_SEQUENCE_BYTES];
        sequence[..pending_len].copy_from_slice(state.pending_bytes());
        sequence[pending_len..pending_len + taken].copy_from_slice(&bytes[..taken]);
        let sequence = &sequence[..pending_len + taken];

        match core::str::from_utf8(sequence) {
            Ok(_) => {
                add_scalars(state, 1)?;
                retain_prefix(state, &[]);
                consumed = taken;
            }
            Err(error) if error.error_len().is_none() => {
                // The chunk ended before this scalar was complete. The safe
                // validator has checked all bytes, including restricted second
                // bytes for overlong encodings, surrogates, and U+10FFFF.
                retain_prefix(state, sequence);
                return Ok(());
            }
            Err(_) => return Err(invalid_utf8(prefix_start, "invalid UTF-8 sequence")),
        }
    }

    let remainder = &bytes[consumed..];
    match core::str::from_utf8(remainder) {
        Ok(text) => {
            add_scalars(state, text.chars().count())?;
            retain_prefix(state, &[]);
        }
        Err(error) => {
            let valid_len = error.valid_up_to();
            if error.error_len().is_some() {
                // Both positions are within the chunk whose cumulative length
                // was checked above, so neither addition can overflow.
                let offset = chunk_start + checked_len(consumed + valid_len)?;
                return Err(invalid_utf8(offset, "invalid UTF-8 sequence"));
            }

            // `valid_up_to` certifies this prefix. Every non-continuation byte
            // in valid UTF-8 begins one scalar, without needing a second decode
            // or any unchecked conversion to str.
            let scalar_count = remainder[..valid_len]
                .iter()
                .filter(|&&byte| byte & 0xc0 != 0x80)
                .count();
            add_scalars(state, scalar_count)?;
            retain_prefix(state, &remainder[valid_len..]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_accepted_state_eq(actual: &Utf8Stream, expected: &Utf8Stream) {
        assert_eq!(actual.carry, expected.carry);
        assert_eq!(actual.total_bytes, expected.total_bytes);
        assert_eq!(actual.total_scalars, expected.total_scalars);
        assert_eq!(actual.ascii_only, expected.ascii_only);
        assert_eq!(actual.finished, expected.finished);
    }

    fn assert_state_eq(actual: &Utf8Stream, expected: &Utf8Stream) {
        assert_accepted_state_eq(actual, expected);
        assert_eq!(actual.failure, expected.failure);
    }

    fn assert_invariants(state: &Utf8Stream) {
        let pending = state.pending_bytes();
        assert!(pending.len() <= MAX_PENDING_BYTES);
        assert!(state.scalars_seen() <= state.bytes_seen());
        assert!(state.bytes_seen() >= u32::from(state.carry.utf8_tail_len));
        assert!(state.carry.utf8_tail[pending.len()..].iter().all(|&byte| byte == 0));
        if !pending.is_empty() {
            let error = core::str::from_utf8(pending).unwrap_err();
            assert_eq!(error.valid_up_to(), 0);
            assert_eq!(error.error_len(), None);
            assert!(!state.ascii_only);
            assert!(!state.is_finished());
        }
        if state.failure().is_some() {
            assert!(!state.is_finished());
        }
    }

    fn assert_error(error: &Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Utf8);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_sticky(state: &mut Utf8Stream, error: &Error) {
        let snapshot = state.clone();
        assert_eq!(state.failure(), Some(error));
        for bytes in [b"".as_slice(), b"a", b"\xff", b"\x80\x80\x80"] {
            assert_eq!(validate_chunk(state, bytes).unwrap_err(), *error);
            assert_state_eq(state, &snapshot);
            assert_eq!(finish(state).unwrap_err(), *error);
            assert_state_eq(state, &snapshot);
        }
        assert_invariants(state);
    }

    fn check_partition(bytes: &[u8], first: usize, second: usize) {
        let mut state = Utf8Stream::new();
        let mut result = Ok(());
        for chunk in [&bytes[..first], &bytes[first..second], &bytes[second..]] {
            let snapshot = state.clone();
            result = validate_chunk(&mut state, chunk);
            assert_invariants(&state);
            if result.is_err() {
                assert_accepted_state_eq(&state, &snapshot);
                break;
            }
            let snapshot = state.clone();
            validate_chunk(&mut state, &[]).unwrap();
            assert_state_eq(&state, &snapshot);
        }
        let actual = result.and_then(|()| finish(&mut state));
        match core::str::from_utf8(bytes) {
            Ok(text) => {
                let summary = actual.unwrap();
                assert_eq!(summary.byte_len, u32::try_from(bytes.len()).unwrap());
                assert_eq!(summary.scalar_len, u32::try_from(text.chars().count()).unwrap());
                assert_eq!(summary.ascii_only, bytes.is_ascii());
                assert_eq!(finish(&mut state).unwrap(), summary);
                assert!(state.is_finished());
            }
            Err(expected) => {
                let error = actual.unwrap_err();
                assert_error(
                    &error,
                    ErrorCode::InvalidUtf8,
                    u32::try_from(expected.valid_up_to()).unwrap(),
                );
                assert_sticky(&mut state, &error);
            }
        }
        assert_invariants(&state);
    }

    fn check_all_partitions(bytes: &[u8]) {
        for first in 0..=bytes.len() {
            for second in first..=bytes.len() {
                check_partition(bytes, first, second);
            }
        }
    }

    #[test]
    fn empty_ascii_and_finished_lifecycle() {
        let mut empty = Utf8Stream::default();
        assert_state_eq(&empty, &Utf8Stream::new());
        validate_chunk(&mut empty, &[]).unwrap();
        assert_state_eq(&empty, &Utf8Stream::new());
        assert_eq!(finish(&mut empty).unwrap(), Utf8Summary::default());
        assert_eq!(finish(&mut empty).unwrap(), Utf8Summary::default());

        let mut ascii = [0u8; 128];
        for (index, byte) in ascii.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        let mut state = Utf8Stream::new();
        for chunk in ascii.chunks(7) {
            validate_chunk(&mut state, chunk).unwrap();
            assert_invariants(&state);
        }
        let summary = finish(&mut state).unwrap();
        assert_eq!(summary, Utf8Summary { byte_len: 128, scalar_len: 128, ascii_only: true });
        for state in [&mut empty, &mut state] {
            let snapshot = state.clone();
            for bytes in [b"".as_slice(), b"a", b"\xff"] {
                let error = validate_chunk(state, bytes).unwrap_err();
                assert_error(&error, ErrorCode::InvalidToken, snapshot.total_bytes);
                assert_state_eq(state, &snapshot);
                assert!(state.failure().is_none());
                assert!(finish(state).is_ok());
                assert_state_eq(state, &snapshot);
            }
        }
    }

    #[test]
    fn scalar_boundaries_and_text_match_across_every_three_chunk_partition() {
        let cases: &[&[u8]] = &[
            b"", b"a\0\x7f", b"\xc2\x80", b"\xdf\xbf",
            b"\xe0\xa0\x80", b"\xed\x9f\xbf", b"\xee\x80\x80",
            b"\xef\xb7\x90", b"\xef\xbf\xbe", b"\xef\xbf\xbf",
            b"\xf0\x90\x80\x80", b"\xf4\x8f\xbf\xbf",
            "A\u{e9}\u{20ac}\u{1f600}Z".as_bytes(),
            "\u{feff}e\u{301}\u{fffd}".as_bytes(),
            "\u{1f469}\u{200d}\u{1f4bb}".as_bytes(),
            br"\uD800\uDC00",
        ];
        for &bytes in cases {
            check_all_partitions(bytes);
        }
    }

    #[test]
    fn four_byte_scalar_completes_one_byte_at_a_time() {
        let mut state = Utf8Stream::new();
        validate_chunk(&mut state, b"ab").unwrap();
        let sequence = b"\xf0\x9f\x98\x80";
        for index in 0..3 {
            validate_chunk(&mut state, &sequence[index..index + 1]).unwrap();
            assert_eq!(state.bytes_seen(), 3 + u32::try_from(index).unwrap());
            assert_eq!(state.scalars_seen(), 2);
            assert_eq!(state.pending_bytes(), &sequence[..index + 1]);
            assert_invariants(&state);
            let snapshot = state.clone();
            validate_chunk(&mut state, &[]).unwrap();
            assert_state_eq(&state, &snapshot);
        }
        validate_chunk(&mut state, &sequence[3..]).unwrap();
        assert_eq!(state.bytes_seen(), 6);
        assert_eq!(state.scalars_seen(), 3);
        assert_eq!(state.carry.utf8_tail, [0; 4]);
        validate_chunk(&mut state, b"!").unwrap();
        assert_eq!(finish(&mut state).unwrap(), Utf8Summary {
            byte_len: 7, scalar_len: 4, ascii_only: false,
        });
    }

    #[test]
    fn replacing_long_tail_with_short_tail_clears_unused_bytes() {
        let mut state = Utf8Stream::new();
        validate_chunk(&mut state, b"\xf0\x9f\x98").unwrap();
        assert_eq!(state.carry.utf8_tail, [0xf0, 0x9f, 0x98, 0]);
        validate_chunk(&mut state, b"\x80a\xc2").unwrap();
        assert_eq!(state.carry.utf8_tail, [0xc2, 0, 0, 0]);
        assert_eq!(state.scalars_seen(), 2);
        validate_chunk(&mut state, b"\xa2").unwrap();
        assert_eq!(state.carry.utf8_tail, [0; 4]);
        assert_eq!(state.carry.utf8_tail_len, 0);
        assert_eq!(finish(&mut state).unwrap().scalar_len, 3);
    }

    #[test]
    fn malformed_and_truncated_sequences_match_at_every_partition() {
        let cases: &[&[u8]] = &[
            b"\x80", b"\xbf", b"\xc0\x80", b"\xc1\xbf",
            b"\xe0\x9f", b"\xe0\x80\x80", b"\xed\xa0", b"\xed\xbf\xbf",
            b"\xf0\x8f", b"\xf0\x80\x80\x80", b"\xf4\x90", b"\xf4\xbf\xbf\xbf",
            b"\xf5\x80\x80\x80", b"\xf8\x88\x80\x80\x80", b"\xfe", b"\xff",
            b"\xc2", b"\xe2", b"\xe2\x82", b"\xf0", b"\xf0\x9f", b"\xf0\x9f\x98",
            b"a\xe2b", b"a\xe2\x82b\xff", b"\xc2\xa2\xe2\x82\xac\xf0\x9f",
            b"\xc2\xa2\xe2\x82\xac\xff\x80", b"\xf0\x9f\x98\x80\x80",
        ];
        for &bytes in cases {
            check_all_partitions(bytes);
        }
    }

    #[test]
    fn invalid_restricted_second_bytes_fail_before_finish() {
        for sequence in [b"\xe0\x9f", b"\xed\xa0", b"\xf0\x8f", b"\xf4\x90"] {
            for split in 0..=1 {
                let mut state = Utf8Stream::new();
                validate_chunk(&mut state, b"abc").unwrap();
                validate_chunk(&mut state, &sequence[..split]).unwrap();
                let snapshot = state.clone();
                let error = validate_chunk(&mut state, &sequence[split..]).unwrap_err();
                assert_error(&error, ErrorCode::InvalidUtf8, 3);
                assert_accepted_state_eq(&state, &snapshot);
                assert_sticky(&mut state, &error);
            }
        }
    }

    #[test]
    fn bad_continuations_at_every_position_preserve_original_start() {
        for sequence in [b"\xc2\xa2".as_slice(), b"\xe2\x82\xac", b"\xf0\x9f\x98\x80"] {
            for position in 1..sequence.len() {
                for replacement in [0x00, 0x7f, 0xc0, 0xc2, 0xe0, 0xf0, 0xff] {
                    let mut bytes = [b'a'; 8];
                    bytes[2..2 + sequence.len()].copy_from_slice(sequence);
                    bytes[2 + position] = replacement;
                    check_all_partitions(&bytes[..2 + sequence.len()]);
                }
            }
        }
    }

    #[test]
    fn finish_truncation_preserves_counters_and_pending_prefix() {
        for sequence in [b"\xc2\xa2".as_slice(), b"\xe2\x82\xac", b"\xf0\x9f\x98\x80"] {
            for end in 1..sequence.len() {
                let mut state = Utf8Stream::new();
                validate_chunk(&mut state, "a\u{e9}".as_bytes()).unwrap();
                validate_chunk(&mut state, &sequence[..end]).unwrap();
                let snapshot = state.clone();
                let error = finish(&mut state).unwrap_err();
                assert_error(&error, ErrorCode::InvalidUtf8, 3);
                assert_eq!(error.detail, "incomplete UTF-8 sequence");
                assert_accepted_state_eq(&state, &snapshot);
                assert_sticky(&mut state, &error);
            }
        }
    }

    #[test]
    fn chunk_failure_rolls_back_all_progress_including_completed_carry() {
        let cases: &[(&[u8], &[u8], u32)] = &[
            (b"abc", b"de\xc2\xa2\xff", 7),
            (b"abc\xf0\x9f", b"\x98\x80de\xe2\x82!", 9),
            (b"abc\xf0\x9f", b"\x98!", 3),
            (b"abc\xc2", b"\xa2\x80", 5),
            (b"abc\xe2", b"\x82\xac\xc2\xa2\xff", 8),
        ];
        for &(prefix, chunk, offset) in cases {
            let mut state = Utf8Stream::new();
            validate_chunk(&mut state, prefix).unwrap();
            let snapshot = state.clone();
            let error = validate_chunk(&mut state, chunk).unwrap_err();
            assert_error(&error, ErrorCode::InvalidUtf8, offset);
            assert_accepted_state_eq(&state, &snapshot);
            assert_sticky(&mut state, &error);
        }
    }

    #[test]
    fn scanner_quote_carry_survives_success_failure_and_finish() {
        for (in_string, escaped) in [(0, 0), (1, 1), (u64::MAX, 0x1234_5678)] {
            let mut state = Utf8Stream::new();
            state.carry.prev_in_string = in_string;
            state.carry.prev_escaped = escaped;
            for bytes in [b"a\xf0".as_slice(), b"", b"\x9f", b"\x98\x80z"] {
                validate_chunk(&mut state, bytes).unwrap();
                assert_eq!(state.carry.prev_in_string, in_string);
                assert_eq!(state.carry.prev_escaped, escaped);
            }
            let mut failed = state.clone();
            validate_chunk(&mut failed, b"\xff").unwrap_err();
            finish(&mut failed).unwrap_err();
            finish(&mut state).unwrap();
            assert_eq!(state.carry.prev_in_string, in_string);
            assert_eq!(state.carry.prev_escaped, escaped);
            assert_eq!(failed.carry.prev_in_string, in_string);
            assert_eq!(failed.carry.prev_escaped, escaped);
        }
    }

    #[test]
    fn reset_reopens_finished_failed_and_pending_streams() {
        for mode in 0..4 {
            let mut state = Utf8Stream::new();
            state.carry.prev_in_string = u64::MAX;
            state.carry.prev_escaped = 1;
            validate_chunk(&mut state, b"abc").unwrap();
            match mode {
                0 => { finish(&mut state).unwrap(); }
                1 => { validate_chunk(&mut state, b"\xff").unwrap_err(); }
                2 => { validate_chunk(&mut state, b"\xf0\x9f").unwrap(); }
                _ => {
                    validate_chunk(&mut state, b"\xe2").unwrap();
                    finish(&mut state).unwrap_err();
                }
            }
            state.reset();
            assert_state_eq(&state, &Utf8Stream::new());
            validate_chunk(&mut state, b"ok").unwrap();
            assert_eq!(finish(&mut state).unwrap(), Utf8Summary {
                byte_len: 2, scalar_len: 2, ascii_only: true,
            });
        }
    }

    #[test]
    fn cumulative_limit_accepts_exact_maximum_and_rejects_overflow_first() {
        // Private-state seeding models a long accepted ASCII history without
        // allocating gigabytes or constructing a fictitious input slice.
        let mut state = Utf8Stream::new();
        state.total_bytes = u32::MAX - 1;
        state.total_scalars = u32::MAX - 1;
        validate_chunk(&mut state, b"a").unwrap();
        assert_eq!(state.bytes_seen(), u32::MAX);
        assert_eq!(state.scalars_seen(), u32::MAX);
        validate_chunk(&mut state, &[]).unwrap();
        let mut completed = state.clone();
        assert_eq!(finish(&mut completed).unwrap(), Utf8Summary {
            byte_len: u32::MAX, scalar_len: u32::MAX, ascii_only: true,
        });
        let error = validate_chunk(&mut completed, b"\xff").unwrap_err();
        assert_error(&error, ErrorCode::InvalidToken, u32::MAX);
        assert!(completed.failure().is_none());

        for bytes in [b"a".as_slice(), b"\xff", b"\xf0\x9f\x98\x80"] {
            let mut overflow = state.clone();
            let error = validate_chunk(&mut overflow, bytes).unwrap_err();
            assert_error(&error, ErrorCode::PayloadTooLarge, u32::MAX);
            assert_accepted_state_eq(&overflow, &state);
            assert_sticky(&mut overflow, &error);
        }
    }

    #[test]
    fn pending_prefix_and_absolute_offsets_at_u32_limit() {
        let mut state = Utf8Stream::new();
        state.total_bytes = u32::MAX - 4;
        state.total_scalars = u32::MAX - 4;
        validate_chunk(&mut state, b"\xf0\x9f\x98").unwrap();
        let snapshot = state.clone();

        let mut truncated = snapshot.clone();
        let error = finish(&mut truncated).unwrap_err();
        assert_error(&error, ErrorCode::InvalidUtf8, u32::MAX - 4);
        assert_accepted_state_eq(&truncated, &snapshot);

        let mut invalid = snapshot.clone();
        let error = validate_chunk(&mut invalid, b"!").unwrap_err();
        assert_error(&error, ErrorCode::InvalidUtf8, u32::MAX - 4);
        assert_accepted_state_eq(&invalid, &snapshot);

        let mut overflow = snapshot.clone();
        let error = validate_chunk(&mut overflow, b"!x").unwrap_err();
        assert_error(&error, ErrorCode::PayloadTooLarge, u32::MAX);
        assert_accepted_state_eq(&overflow, &snapshot);
        assert_sticky(&mut overflow, &error);

        validate_chunk(&mut state, b"\x80").unwrap();
        assert_eq!(finish(&mut state).unwrap(), Utf8Summary {
            byte_len: u32::MAX, scalar_len: u32::MAX - 3, ascii_only: false,
        });

        let mut last_byte = Utf8Stream::new();
        last_byte.total_bytes = u32::MAX - 1;
        last_byte.total_scalars = u32::MAX - 1;
        let error = validate_chunk(&mut last_byte, b"\xff").unwrap_err();
        assert_error(&error, ErrorCode::InvalidUtf8, u32::MAX - 1);
    }

    #[test]
    fn every_single_byte_and_two_byte_input_matches_core_when_split() {
        for first in 0u8..=255 {
            check_all_partitions(&[first]);
            for second in 0u8..=255 {
                check_partition(&[first, second], 1, 2);
            }
        }
    }

    #[test]
    fn block_boundaries_and_unaligned_chunks_preserve_results() {
        let mut storage = [b'a'; 264];
        for alignment in 0..8 {
            for start in [0, 1, 61, 62, 63, 64, 65, 125, 126, 127, 128, 129] {
                for sequence in [b"\xc2\xa2".as_slice(), b"\xe2\x82\xac", b"\xf0\x9f\x98\x80"] {
                    storage.fill(b'a');
                    storage[alignment + start..alignment + start + sequence.len()]
                        .copy_from_slice(sequence);
                    let end = start + sequence.len() + 65;
                    for split in start..=start + sequence.len() {
                        check_partition(&storage[alignment..alignment + end], split, split);
                    }
                    let mut state = Utf8Stream::new();
                    for chunk in storage[alignment..alignment + end].chunks(64) {
                        validate_chunk(&mut state, chunk).unwrap();
                    }
                    assert_eq!(finish(&mut state).unwrap().scalar_len,
                        u32::try_from(start + 66).unwrap());
                    storage[alignment + start + sequence.len() - 1] = 0xff;
                    for split in start..=start + sequence.len() {
                        check_partition(&storage[alignment..alignment + end], split, split);
                    }
                }
            }
        }
    }

    #[test]
    fn deterministic_mutations_and_truncations_match_core() {
        let pattern = b"a\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80";
        let mut bytes = [0u8; 80];
        for chunk in bytes.chunks_exact_mut(pattern.len()) {
            chunk.copy_from_slice(pattern);
        }
        for position in 0..bytes.len() {
            let original = bytes[position];
            for replacement in [0x00, 0x7f, 0x80, 0xbf, 0xc0, 0xc2, 0xe0, 0xed, 0xf4, 0xff] {
                bytes[position] = replacement;
                check_partition(&bytes, position, position + 1);
                check_partition(&bytes[..position + 1], position, position + 1);
            }
            bytes[position] = original;
        }
    }
}
