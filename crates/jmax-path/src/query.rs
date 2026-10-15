//! Exact-path extraction and borrowed views into the result's own storage.
//!
//! Spans describe raw JSON tokens, including string quotes and both container
//! delimiters. Cooked strings resolve through the payload or decoded arena
//! retained by `SelectedValues`. Neither spans nor results are schema proofs.

use super::*;

/// Execute all compiled paths in one cursor traversal.
///
/// Slots retain input path order, including repeated paths. Each slot receives
/// its first complete match; missing paths remain absent, while JSON null is a
/// present value. Numeric spelling and whitespace inside containers are kept.
///
/// Strict mode validates through EOF. Query mode may return before validating
/// the suffix, so callers must inspect `SelectedValues::fully_validated` when
/// full syntax validation matters. The result retains the entire supplied
/// payload in either mode. Cursor errors propagate without modification.
pub fn execute(
    payload: Payload,
    paths: &CompiledPaths,
    mode: SkipMode,
    limits: Limits,
) -> Result<SelectedValues> {
    jmax_cursor::select::extract(payload, paths.plan(), mode, limits)
}

/// Return the selected raw token span, or `None` for a missing or invalid slot.
///
/// The offsets refer to `results.payload()` only. A copied span does not carry
/// its owner and does not establish validity for bytes from another payload.
pub fn span(results: &SelectedValues, slot: u32) -> Option<Span32> {
    let slot = usize::try_from(slot).ok()?;
    results.slots().get(slot)?.as_ref().map(|value| value.span)
}

/// Borrow a complete raw token from its result owner using checked bounds.
///
/// Strings include quotes; containers include their delimiters and internal
/// whitespace. Missing slots, out-of-range slots, and invalid spans yield `None`.
pub fn raw(results: &SelectedValues, slot: u32) -> Option<&[u8]> {
    results.raw(slot)
}

/// Borrow a selected string's decoded UTF-8 content from its result owner.
///
/// Resolution uses the string reference's tagged raw or decoded storage.
/// Missing or out-of-range slots, non-string values, invalid references, and
/// invalid UTF-8 yield `None`. An empty string yields `Some("")`.
pub fn string(results: &SelectedValues, slot: u32) -> Option<&str> {
    results.string(slot)
}
