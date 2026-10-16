//! Ownership transfer for suspended, completed, and partially consumed cursors.
//!
//! Suspension preserves the parser itself, including unfinished tokens and skips.
//! Complete output validates through EOF; partial query output retains all fed
//! bytes without interpreting an unconsumed suffix or issuing a schema proof.

use super::*;

/// Moves every part of a cursor into an owned continuation without advancing it.
///
/// This does not allocate, copy buffers, mark EOF, or perform background work.
/// Incremental chunks are retained by `walk::push`; complete input retains its
/// original shared payload. The continuation does not borrow caller storage.
pub fn suspend(cursor: Cursor) -> SuspendedCursor {
    SuspendedCursor { cursor }
}

/// Restores the exact cursor held by a continuation without advancing it.
pub fn resume(suspended: SuspendedCursor) -> Cursor {
    suspended.cursor
}

/// Consumes a cursor and validates all remaining input, including in query mode.
///
/// EOF is declared first. A suspended subtree skip is completed using its own
/// stored mode, which may differ from the cursor's mode. The syntax output
/// conversion then drains through `Finished` and transfers the backing owners.
/// Already completed cursors are accepted. Existing and newly discovered syntax
/// errors retain their original code, source, offset, and context.
pub fn finish(mut cursor: Cursor) -> Result<CursorOutput> {
    walk::end_input(&mut cursor)?;
    if let Some(progress) = cursor.skipping {
        if let SkipStep::NeedMore = skip::subtree(&mut cursor, progress.mode)? {
            // EOF was declared above, so a successful skip cannot await input.
            return Err(cursor_error(
                ErrorCode::InternalError,
                cursor.state().byte_offset,
                "subtree skip requested input after EOF",
            ));
        }
    }
    let output = jmax_syntax::finish::into_output(cursor.syntax)?;
    Ok(CursorOutput {
        payload: output.payload,
        decoded: output.decoded,
        fully_validated: true,
    })
}

/// Consumes a query cursor without marking EOF or consuming another event.
///
/// An existing parser failure is returned unchanged. Otherwise strict mode is
/// rejected with `ConstraintViolation`, and an active skip is rejected with
/// `TransportIncomplete`, even if enough bytes to finish it have since arrived.
/// Input ownership and the decoded arena are transferred without copying.
/// The validation flag reflects actual syntax completion, not EOF declaration,
/// a complete root event, or successful completion of a single subtree.
pub fn into_partial(cursor: Cursor) -> Result<CursorOutput> {
    if let Some(error) = cursor.syntax.failure() {
        return Err(error.clone());
    }
    let at = cursor.state().byte_offset;
    if cursor.mode != SkipMode::Query {
        return Err(cursor_error(
            ErrorCode::ConstraintViolation,
            at,
            "partial output requires query cursor mode",
        ));
    }
    if cursor.skipping.is_some() {
        return Err(cursor_error(
            ErrorCode::TransportIncomplete,
            at,
            "partial output requires completion of the pending subtree skip",
        ));
    }

    let fully_validated = cursor.syntax.fully_validated();
    let output = cursor.syntax.into_storage()?;
    Ok(CursorOutput {
        payload: output.payload,
        decoded: output.decoded,
        fully_validated,
    })
}
