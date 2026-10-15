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
