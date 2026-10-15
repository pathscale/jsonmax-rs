//! Forward, tapeless traversal over the syntax parser's owned input.
//!
//! Feeding bytes and declaring EOF do not consume events or establish validity.
//! Only advancing through [`ScanStep::Finished`] validates the complete input.

use super::*;

/// Retains a complete payload in a new cursor and declares end of input.
///
/// The cursor retains its own bytes after the supplied owner is dropped. No
/// events are consumed: even empty or malformed input is diagnosed by traversal,
/// unless the payload exceeds its configured byte limit. The syntax scanner
/// reads the original immutable payload in place, including across suspension.
pub fn from_payload(payload: Payload, limits: Limits, mode: SkipMode) -> Result<Cursor> {
    let syntax = SyntaxState::from_payload(payload.clone(), limits)?;
    Ok(Cursor { syntax, payload: Some(payload), mode, current: None, skipping: None })
}

/// Retains complete input and checks numeric policy during token scanning.
pub fn from_payload_with_numeric(
    payload: Payload,
    limits: Limits,
    mode: SkipMode,
    numeric: jmax_abi::number::NumericPolicy,
) -> Result<Cursor> {
    let syntax = SyntaxState::from_payload_with_numeric(payload.clone(), limits, numeric)?;
    Ok(Cursor { syntax, payload: Some(payload), mode, current: None, skipping: None })
}

/// Copies a chunk into retained input without advancing the event stream.
///
/// Empty chunks are permitted before EOF. All pushes after EOF, including empty
/// chunks, fail according to the syntax parser's sticky error rules. Feeding is
/// permitted while a subtree skip is suspended.
pub fn push(cursor: &mut Cursor, bytes: &[u8]) -> Result<()> {
    jmax_syntax::feed::push(&mut cursor.syntax, bytes)
}

/// Declares EOF without consuming events. Repeated declarations are harmless.
///
/// An existing syntax failure is returned unchanged. A suspended subtree skip
/// does not prevent declaring EOF.
pub fn end_input(cursor: &mut Cursor) -> Result<()> {
    jmax_syntax::finish::end_input(&mut cursor.syntax)
}

/// Advances to the next event, chunk boundary, or validated end of input.
///
/// Only an emitted event replaces [`Cursor::current`]. A suspended subtree skip
/// must be resumed through `skip::subtree`; attempting ordinary traversal returns
/// a recoverable cursor error without changing parser or skip state. Previously
/// recorded syntax failures remain sticky, including during a suspended skip.
pub fn next(cursor: &mut Cursor) -> Result<ScanStep> {
    if let Some(error) = cursor.syntax.failure() {
        return Err(error.clone());
    }
    if cursor.skipping.is_some() {
        return Err(cursor_error(
            ErrorCode::InvalidSyntax,
            cursor.state().byte_offset,
            "cannot advance while a subtree skip is suspended",
        ));
    }

    let step = jmax_syntax::feed::next(&mut cursor.syntax)?;
    if let ScanStep::Emit(event) = step {
        cursor.current = Some(event);
    }
    Ok(step)
}
