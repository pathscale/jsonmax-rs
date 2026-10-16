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
    Ok(Cursor {
        syntax,
        payload: Some(payload),
        mode,
        current: None,
        skipping: None,
    })
}

/// Retains complete input and checks numeric policy during token scanning.
pub fn from_payload_with_numeric(
    payload: Payload,
    limits: Limits,
    mode: SkipMode,
    numeric: jmax_abi::number::NumericPolicy,
) -> Result<Cursor> {
    let syntax = SyntaxState::from_payload_with_numeric(payload.clone(), limits, numeric)?;
    Ok(Cursor {
        syntax,
        payload: Some(payload),
        mode,
        current: None,
        skipping: None,
    })
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

/// Borrowed complete-input traversal, with no payload owner or staging copy.
#[derive(Debug)]
pub struct BorrowedCursor<'a> {
    pub(crate) scanner: BlockCursor<'a>,
    pub(crate) decoded: Vec<u8>,
    mode: SkipMode,
    current: Option<Event>,
}

pub fn walk_borrowed(input: &[u8], limits: Limits, mode: SkipMode) -> Result<BorrowedCursor<'_>> {
    walk_borrowed_with_numeric(input, limits, mode, None)
}
pub fn walk_borrowed_with_numeric(
    input: &[u8],
    limits: Limits,
    mode: SkipMode,
    numeric: Option<jmax_abi::number::NumericPolicy>,
) -> Result<BorrowedCursor<'_>> {
    Ok(BorrowedCursor {
        scanner: BlockCursor::new(input, limits, numeric, DuplicatePolicyOwner::Syntax)?,
        decoded: Vec::new(),
        mode,
        current: None,
    })
}
impl<'a> BorrowedCursor<'a> {
    pub fn next(&mut self) -> Result<ScanStep> {
        let step = self.scanner.next_event(&mut self.decoded)?;
        if let ScanStep::Emit(event) = step {
            self.current = Some(event);
        }
        Ok(step)
    }
    pub const fn input(&self) -> &'a [u8] {
        self.scanner.input()
    }
    pub fn raw_bytes(&self) -> &'a [u8] {
        self.scanner.input()
    }
    pub fn decoded_bytes(&self) -> &[u8] {
        &self.decoded
    }
    pub const fn current(&self) -> Option<Event> {
        self.current
    }
    pub const fn mode(&self) -> SkipMode {
        self.mode
    }
    pub fn state(&self) -> CursorState {
        self.scanner.state()
    }
    pub const fn fully_validated(&self) -> bool {
        self.scanner.fully_validated()
    }
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        let owner = match reference.source {
            SpanSource::RawPayload => self.input(),
            SpanSource::DecodedArena => &self.decoded,
        };
        core::str::from_utf8(reference.span.slice(owner)?).ok()
    }
    pub fn skip_current(&mut self, mode: ValueSkipMode) -> Result<SelectedValue> {
        if let Some(error) = self.scanner.failure() {
            return Err(error.clone());
        }
        match self.current {
            Some(Event::Scalar {
                kind,
                span,
                decoded,
            }) => Ok(SelectedValue {
                kind: scalar_value_kind(kind),
                span,
                decoded,
            }),
            Some(Event::ObjectStart { .. }) | Some(Event::ArrayStart { .. }) => {
                let object = matches!(self.current, Some(Event::ObjectStart { .. }));
                let span = self.scanner.skip_open_container(&mut self.decoded, mode)?;
                let offset = span.offset + span.len - 1;
                self.current = Some(if object {
                    Event::ObjectEnd { offset }
                } else {
                    Event::ArrayEnd { offset }
                });
                Ok(SelectedValue {
                    kind: if object {
                        ValueKind::Object
                    } else {
                        ValueKind::Array
                    },
                    span,
                    decoded: None,
                })
            }
            _ => Err(cursor_error(
                ErrorCode::TypeMismatch,
                self.state().byte_offset,
                "subtree skip requires a scalar or container start event",
            )),
        }
    }
    /// Validated boundary skip while the witness retains the immutable borrow.
    pub fn skip_validated(&mut self, validation: &BorrowedValidation<'a>) -> Result<SelectedValue> {
        let object = match self.current {
            Some(Event::ObjectStart { .. }) => true,
            Some(Event::ArrayStart { .. }) => false,
            Some(Event::Scalar {
                kind,
                span,
                decoded,
            }) => {
                return Ok(SelectedValue {
                    kind: scalar_value_kind(kind),
                    span,
                    decoded,
                })
            }
            _ => {
                return Err(cursor_error(
                    ErrorCode::TypeMismatch,
                    self.state().byte_offset,
                    "validated skip requires a current value",
                ))
            }
        };
        let span = validation.skip_open_container(&mut self.scanner, &mut self.decoded)?;
        let offset = span.offset + span.len - 1;
        self.current = Some(if object {
            Event::ObjectEnd { offset }
        } else {
            Event::ArrayEnd { offset }
        });
        Ok(SelectedValue {
            kind: if object {
                ValueKind::Object
            } else {
                ValueKind::Array
            },
            span,
            decoded: None,
        })
    }
}

pub(crate) const fn scalar_value_kind(kind: ScalarKind) -> ValueKind {
    match kind {
        ScalarKind::String => ValueKind::String,
        ScalarKind::Number => ValueKind::Number,
        ScalarKind::True | ScalarKind::False => ValueKind::Bool,
        ScalarKind::Null => ValueKind::Null,
    }
}
