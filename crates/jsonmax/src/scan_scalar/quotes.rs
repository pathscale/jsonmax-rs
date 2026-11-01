//! Word-level escape followers and inclusive delimiter prefix parity.

use super::*;

/// Resolve only live bytes, preserving all carry on errors and empty blocks.
/// Odd backslash runs escape their non-backslash follower, including byte zero
/// from incoming carry. Delimiters are excluded from the string-content mask.
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

    // Adding a bit at each run start propagates a carry to its first follower.
    // Split starts by position parity: an odd run ends on opposite parity.
    // Wrapping intentionally discards followers beyond a full 64-byte block.
    const EVEN: u64 = 0x5555_5555_5555_5555;
    let starts = backslashes & !(backslashes << 1);
    let even_carries = backslashes.wrapping_add(starts & EVEN);
    let odd_carries = backslashes.wrapping_add(starts & !EVEN);
    let mut escaped_followers = ((even_carries & !EVEN) | (odd_carries & EVEN)) & !backslashes;
    if carry.prev_escaped != 0 {
        // Toggle precisely the follower of the initial run (bit zero if the
        // block starts with a non-backslash). No follower for 64 backslashes.
        escaped_followers ^= !backslashes & backslashes.wrapping_add(1);
    }
    // Align the last live byte to bit 63; padding cannot alter trailing parity.
    let trailing = (backslashes << (64 - u32::from(valid_len))).leading_ones();
    let trailing_parity = u64::from(trailing & 1)
        ^ if trailing == u32::from(valid_len) {
            carry.prev_escaped
        } else {
            0
        };
    let delimiter_quote_mask = quotes & !escaped_followers;
    let mut prefix = prefix_xor(delimiter_quote_mask);
    if carry.prev_in_string != 0 {
        prefix = !prefix;
    }
    let result = QuoteMasks {
        escaped_quote_mask: quotes & escaped_followers,
        delimiter_quote_mask,
        in_string_mask: prefix & !delimiter_quote_mask & valid,
    };
    carry.prev_in_string = (prefix >> (valid_len - 1)) & 1;
    carry.prev_escaped = trailing_parity;
    Ok(result)
}

#[inline]
fn portable_prefix_xor(mut bits: u64) -> u64 {
    bits ^= bits << 1;
    bits ^= bits << 2;
    bits ^= bits << 4;
    bits ^= bits << 8;
    bits ^= bits << 16;
    bits ^= bits << 32;
    bits
}

#[inline]
fn prefix_xor(bits: u64) -> u64 {
    portable_prefix_xor(bits)
}
