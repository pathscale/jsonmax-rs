//! Quote and escape resolution over the combined four-lane classification.
//!
//! Portable integer operations preserve byte order across lane boundaries and
//! work on every target, without allocation or additional CPU requirements.

use super::*;

/// Resolve quotes in the low `valid_len` positions and advance quote carry.
///
/// Odd consecutive backslash runs escape the following byte, even outside a
/// string. Unescaped quotes toggle string state and are excluded from string
/// content. Escaped quotes are content only when already inside a string.
/// Incoming escape parity extends only a run starting at position zero.
///
/// Bits beyond the valid length are ignored. Empty blocks preserve carry;
/// UTF-8 state is always opaque and unchanged. Unclosed strings and trailing
/// escapes are valid intermediate states.
///
/// # Errors
///
/// Lengths above 64 return `InvalidToken`. Noncanonical quote carry or live
/// quote/backslash overlap returns `InternalError`. Errors originate from
/// `Scanner` at offset zero and leave every carry field unchanged.
pub fn quote_masks(
    quote_mask: u64,
    backslash_mask: u64,
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<QuoteMasks> {
    let valid = valid_mask(usize::from(valid_len))?;
    validate_carry(carry)?;
    let quotes = quote_mask & valid;
    let mut backslashes = backslash_mask & valid;
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

    // Bit zero may follow an odd run from the preceding block. For every
    // local run, only its first following byte can be an escaped quote.
    let mut escaped_positions = carry.prev_escaped;
    let mut trailing_parity = 0u64;
    while backslashes != 0 {
        let start = backslashes.trailing_zeros();
        let run_len = (backslashes >> start).trailing_ones();
        let end = start + run_len;
        let mut parity = u64::from(run_len & 1);
        if start == 0 {
            parity ^= carry.prev_escaped;
        }
        if end == u32::from(valid_len) {
            trailing_parity = parity;
        } else if parity != 0 {
            // end < valid_len <= 64, so this never shifts by the word width.
            escaped_positions |= 1u64 << end;
        }

        // Remove this entire run. A run reaching bit 63 needs a separate
        // branch to avoid a shift by 64, including an all-backslash block.
        backslashes = if end < 64 {
            backslashes & (u64::MAX << end)
        } else {
            0
        };
    }

    let escaped_quote_mask = quotes & escaped_positions;
    let delimiter_quote_mask = quotes & !escaped_positions;

    // Inclusive prefix XOR: bit i is the parity of delimiters at 0..=i.
    // All four NEON lanes participate in one prefix, with no lane reset.
    let mut prefix = delimiter_quote_mask;
    prefix ^= prefix << 1;
    prefix ^= prefix << 2;
    prefix ^= prefix << 4;
    prefix ^= prefix << 8;
    prefix ^= prefix << 16;
    prefix ^= prefix << 32;
    if carry.prev_in_string != 0 {
        prefix = !prefix;
    }
    let result = QuoteMasks {
        escaped_quote_mask,
        delimiter_quote_mask,
        in_string_mask: prefix & !delimiter_quote_mask & valid,
    };

    // Commit only after validation and calculation. Inspect the last actual
    // byte, never padding, and modify only the two fields owned by quoting.
    carry.prev_in_string = (prefix >> (valid_len - 1)) & 1;
    carry.prev_escaped = trailing_parity;
    Ok(result)
}
