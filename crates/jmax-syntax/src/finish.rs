//! Explicit end of input, complete syntax validation, and buffer ownership transfer.
//!
//! EOF classification remains in the incremental feed so event-driven callers
//! and callers that discard events receive the same diagnostics.

use super::*;

/// Marks the retained input as final without parsing or consuming any events.
///
/// Repeated calls succeed unless a failure has been recorded. A prior failure
/// is returned unchanged, without changing even the EOF marker.
pub fn end_input(state: &mut SyntaxState) -> Result<()> {
    if let Some(error) = state.failure.as_ref() {
        return Err(error.clone());
    }
    state.final_input = true;
    Ok(())
}

/// Ends input and validates the entire document, discarding remaining events.
///
/// This includes pending tokens, open containers, and trailing whitespace.
/// Call [`end_input`] followed by [`feed::next`] to retain the remaining events.
/// Successful finalization is idempotent; the first failure is permanent.
pub fn finish(state: &mut SyntaxState) -> Result<()> {
    end_input(state)?;
    if state.query_skipped {
        let mut discarded = Vec::new();
        let result = (|| {
            let mut cursor = BlockCursor::new(
                &state.input,
                state.limits,
                state.numeric,
                DuplicatePolicyOwner::Syntax,
            )?;
            cursor.skip_value(
                &mut discarded,
                0,
                jmax_abi::syntax::SkipMode::StrictValidate,
            )?;
            cursor.finish_eof()
        })();
        if let Err(error) = result {
            return Err(state.fail(error));
        }
        state.query_skipped = false;
    }
    loop {
        match feed::next(state)? {
            ScanStep::Emit(_) => {}
            ScanStep::Finished => return Ok(()),
            ScanStep::NeedMore => {
                let error = syntax_error(
                    ErrorCode::InternalError,
                    state.byte_offset,
                    "syntax feed requested more input after EOF",
                );
                return Err(state.fail(error));
            }
        }
    }
}

/// Validates and transfers the retained payload and decoded arena to the output.
///
/// Previously emitted spans and string references retain their owner-relative
/// meaning. Input whitespace and decoded arena order are preserved exactly.
/// This establishes JSON syntax validity only, without issuing a schema proof.
pub fn into_output(mut state: SyntaxState) -> Result<SyntaxOutput> {
    finish(&mut state)?;
    state.into_storage()
}
