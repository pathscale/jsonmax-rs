//! Quote and escape carry analysis over a complete 64-bit predicate mask.
//!
//! Parallel prefix operations cross every byte boundary in the mask. They use
//! only portable integer operations, so this module requires no CPU features,
//! allocations, byte loads, or unsafe code.

use super::*;

/// Analyze the live quote and backslash bits and commit the outgoing carry.
///
/// An odd consecutive backslash run escapes the following quote, including
/// outside strings and across block boundaries. Unescaped quotes toggle the
/// string state but are excluded from string content. Escaped quotes belong
/// to string content only when the current state is inside a string.
///
/// Bits at or above `valid_len` are ignored. Empty input preserves all carry;
/// nonempty input changes only the two quote state fields. UTF-8 state remains
/// opaque, and unfinished strings and escape runs are valid outgoing states.
///
/// # Errors
///
/// Lengths above 64 produce `InvalidToken`. Noncanonical quote carry or live
/// quote/backslash overlap produces `InternalError`. All errors have source
/// `Scanner` and offset zero, and leave every carry field unchanged. Length
/// validation takes precedence over carry and mask validation.
pub fn quote_masks(
    quote_mask: u64,
    backslash_mask: u64,
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<QuoteMasks> {
    let valid = valid_mask(usize::from(valid_len))?;
    validate_carry(carry)?;
    let quotes = quote_mask & valid;
    let backslashes = backslash_mask & valid;
    if quotes & backslashes != 0 {
        return Err(Error::new(
            ErrorCode::InternalError,
            ErrorSource::Scanner,
            0,
            "quote and backslash masks overlap at a live byte",
        ));
    }
    if valid_len == 0 {
        return Ok(QuoteMasks::default());
    }

    let mut odd_after = backslash_run_parity(backslashes);
    if carry.prev_escaped != 0 {
        // Incoming parity affects precisely the initial uninterrupted run.
        // Adding one clears its low one bits. Wrapping handles 64 backslashes.
        let initial_run = backslashes & !backslashes.wrapping_add(1);
        odd_after ^= initial_run;
    }
    let escaped = quotes & ((odd_after << 1) | carry.prev_escaped);
    let delimiters = quotes & !escaped;

    // Inclusive XOR prefix: bit i is the parity of delimiters through byte i.
    let mut string_parity = delimiters;
    string_parity ^= string_parity << 1;
    string_parity ^= string_parity << 2;
    string_parity ^= string_parity << 4;
    string_parity ^= string_parity << 8;
    string_parity ^= string_parity << 16;
    string_parity ^= string_parity << 32;
    let inside = if carry.prev_in_string != 0 {
        !string_parity
    } else {
        string_parity
    };
    let result = QuoteMasks {
        escaped_quote_mask: escaped,
        delimiter_quote_mask: delimiters,
        in_string_mask: inside & !delimiters & valid,
    };

    // Validation and all fallible operations precede these writes. The final
    // live bit is in 0..=63, even for a full block; padding cannot affect carry.
    let last = u32::from(valid_len) - 1;
    carry.prev_in_string ^= (string_parity >> last) & 1;
    carry.prev_escaped = (odd_after >> last) & 1;
    Ok(result)
}

/// Bit i is the parity of the backslash run ending at i, with zero input carry.
#[inline]
fn backslash_run_parity(backslashes: u64) -> u64 {
    // Per-byte state obeys p[i] = b[i] XOR (b[i] AND p[i - 1]).
    // Compose these affine Boolean transitions in spans of 1, 2, 4, ... bytes.
    // A span propagates earlier state only when every byte is a backslash.
    // Left shifts supply zero state below byte zero, never a lane-local reset.
    let mut parity = backslashes;
    let mut propagate = backslashes;
    for shift in [1u32, 2, 4, 8, 16, 32] {
        parity ^= propagate & (parity << shift);
        propagate &= propagate << shift;
    }
    parity
}
