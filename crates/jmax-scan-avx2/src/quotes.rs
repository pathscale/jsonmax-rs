//! Quote and escape carry for the combined two-lane AVX2 masks.
//!
//! Whole-word prefix operations propagate state across byte 31/32 without
//! requiring extra CPU features. This stage is allocation-free, works on every
//! target, and also handles masks produced by the portable classifier.

use super::*;

/// Resolve quote delimiters and string content for at most 64 real bytes.
///
/// Backslash parity applies inside and outside strings. Only unescaped quotes
/// toggle string state; neither opening nor closing delimiters are content.
/// Bits above `valid_len` are ignored, including overlapping padding bits.
/// Empty input preserves canonical carry. UTF-8 fields are always untouched.
/// Unclosed strings and trailing backslashes are valid intermediate states.
///
/// # Errors
///
/// Returns `InvalidToken` if `valid_len` exceeds 64, or `InternalError` if either
/// quote carry field is not zero or one or live quote and backslash bits
/// overlap. Errors have source `Scanner`, offset zero, and leave carry intact.
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

    // Segmented inclusive XOR: bit i becomes the parity of the consecutive
    // backslashes ending at i. Before each step, connected marks positions
    // ending in at least `distance` consecutive backslashes. It gates the
    // contribution from the preceding segment so parity never crosses a gap.
    let mut odd_runs = backslashes;
    let mut connected = backslashes;
    for distance in [1, 2, 4, 8, 16, 32] {
        odd_runs ^= (odd_runs << distance) & connected;
        connected &= connected << distance;
    }

    // Adding one clears precisely the low consecutive set bits. Their mask
    // also works for an all-backslash block because addition is wrapping.
    // Only this initial run can inherit parity from the preceding block.
    let initial_run = backslashes & !backslashes.wrapping_add(1);
    odd_runs ^= initial_run & 0u64.wrapping_sub(carry.prev_escaped);
    let escaped_quotes = quotes & ((odd_runs << 1) | carry.prev_escaped);
    let delimiters = quotes & !escaped_quotes;

    // Inclusive delimiter parity is the state after each byte, relative to
    // the incoming string state. Clear both kinds of delimiter from content.
    // These shifts deliberately span the boundary between the two AVX2 lanes.
    let mut prefix = delimiters;
    prefix ^= prefix << 1;
    prefix ^= prefix << 2;
    prefix ^= prefix << 4;
    prefix ^= prefix << 8;
    prefix ^= prefix << 16;
    prefix ^= prefix << 32;
    let states = prefix ^ 0u64.wrapping_sub(carry.prev_in_string);
    let masks = QuoteMasks {
        escaped_quote_mask: escaped_quotes,
        delimiter_quote_mask: delimiters,
        in_string_mask: states & !delimiters & valid,
    };

    // All validation has succeeded. The nonempty length bounds last to 0..63;
    // reading the final live bit avoids treating padding as an escape reset.
    let last = u32::from(valid_len) - 1;
    carry.prev_in_string = (states >> last) & 1;
    carry.prev_escaped = (odd_runs >> last) & 1;
    Ok(masks)
}
