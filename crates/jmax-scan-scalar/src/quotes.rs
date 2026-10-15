//! Bounded quote analysis with consecutive-backslash parity across blocks.

use super::*;

/// Resolve raw quote and backslash masks for zero through 64 live bytes.
///
/// Bit `i` describes byte `i`. Bits beyond `valid_len` are ignored, including
/// overlaps in padding. An odd preceding backslash run escapes a quote even
/// outside a string. Unescaped quotes toggle string state and are excluded
/// from string content; escaped quotes are content only when already inside.
///
/// The outgoing carry records string state and trailing backslash parity.
/// Empty input preserves carry, and UTF-8 fields are always left untouched.
/// Unclosed strings and trailing escapes are valid intermediate states.
///
/// # Errors
///
/// Returns `InvalidToken` for lengths above 64, or `InternalError` for
/// noncanonical quote carry or overlapping live quote and backslash bits.
/// All errors have source `Scanner`, offset zero, and leave carry unchanged.
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

    let mut result = QuoteMasks::default();
    let mut inside = carry.prev_in_string == 1;
    let mut odd_run = carry.prev_escaped == 1;
    for index in 0..usize::from(valid_len) {
        // Length validation bounds every shift to 0..=63.
        let bit = 1u64 << index;
        let is_quote = quotes & bit != 0;
        let is_delimiter = is_quote && !odd_run;
        if is_delimiter {
            result.delimiter_quote_mask |= bit;
            inside = !inside;
        } else {
            if is_quote {
                result.escaped_quote_mask |= bit;
            }
            if inside {
                result.in_string_mask |= bit;
            }
        }

        // Consume the preceding run before resetting it at a non-backslash.
        // This parity is a byte property, independent of the string state.
        odd_run = backslashes & bit != 0 && !odd_run;
    }

    // Every fallible operation precedes publication. With zero live bytes the
    // validated boolean fields retain their original values exactly.
    carry.prev_in_string = u64::from(inside);
    carry.prev_escaped = u64::from(odd_run);
    Ok(result)
}
