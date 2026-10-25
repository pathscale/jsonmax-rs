//! Resumable subtree consumption over the canonical syntax event stream.
//!
//! Strict mode validates every interior token. Complete-input query mode uses
//! structural masks and bracket tags without interior lexical validation.
//! Incremental mode retains resumable token validation. No subtree operation
//! establishes whole-document validity or issues a validation proof.

use super::*;

/// Consumes the value whose scalar or opening event is already current.
///
/// Scalars complete without advancing. Containers consume through their matching
/// closing event, leaving that event current. At a chunk boundary, progress stays
/// in the cursor; feed more input and call again with the same mode. The mode is
/// local to this operation and need not equal [`Cursor::mode`].
///
/// Missing, key, and closing events return `TypeMismatch` without advancing.
/// Changing the mode of a pending skip returns `ConstraintViolation` without
/// advancing. Existing syntax failures take precedence and propagate unchanged.
/// Strict completion validates this subtree under the cursor's syntax limits,
/// including its duplicate-key policy, but makes no claim about the suffix.
pub fn subtree(cursor: &mut Cursor, mode: SkipMode) -> Result<SkipStep> {
    if let Some(error) = cursor.syntax.failure() {
        return Err(error.clone());
    }

    let mut progress = match cursor.skipping {
        Some(progress) => {
            if progress.mode != mode {
                return Err(cursor_error(
                    ErrorCode::ConstraintViolation,
                    cursor.state().byte_offset,
                    "cannot change the mode of a pending subtree skip",
                ));
            }
            progress
        }
        None => {
            let (start, kind) = match cursor.current {
                Some(Event::Scalar {
                    kind,
                    span,
                    decoded,
                }) => {
                    if span.is_empty() || span.slice(cursor.raw_bytes()).is_none() {
                        return Err(cursor_error(
                            ErrorCode::InternalError,
                            cursor.state().byte_offset,
                            "scalar event has an invalid raw span",
                        ));
                    }
                    let kind = match kind {
                        ScalarKind::String => ValueKind::String,
                        ScalarKind::Number => ValueKind::Number,
                        ScalarKind::True | ScalarKind::False => ValueKind::Bool,
                        ScalarKind::Null => ValueKind::Null,
                    };
                    return Ok(SkipStep::Complete {
                        value: SelectedValue {
                            kind,
                            span,
                            decoded,
                        },
                        validated: mode == SkipMode::Strict,
                    });
                }
                Some(Event::ObjectStart { offset }) => (offset, ValueKind::Object),
                Some(Event::ArrayStart { offset }) => (offset, ValueKind::Array),
                _ => {
                    return Err(cursor_error(
                        ErrorCode::TypeMismatch,
                        cursor.state().byte_offset,
                        "subtree skip requires a scalar or container start event",
                    ));
                }
            };
            SkipProgress {
                start,
                kind,
                depth: 1,
                mode,
            }
        }
    };

    // Complete owners can skip without forwarding interior events. Incremental
    // streams retain the exact resumable NeedMore/event behavior below.
    if cursor.skipping.is_none() && cursor.payload.is_some() {
        let skip_mode = match mode {
            SkipMode::Query => ValueSkipMode::QueryOnly,
            SkipMode::Strict => ValueSkipMode::StrictValidate,
        };
        let span = cursor.syntax.skip_open_container(skip_mode)?;
        let offset = span.offset + span.len - 1;
        cursor.current = Some(match progress.kind {
            ValueKind::Object => Event::ObjectEnd { offset },
            _ => Event::ArrayEnd { offset },
        });
        return Ok(SkipStep::Complete {
            value: SelectedValue {
                kind: progress.kind,
                span,
                decoded: None,
            },
            validated: mode == SkipMode::Strict,
        });
    }

    if progress.depth == 0 || !matches!(progress.kind, ValueKind::Object | ValueKind::Array) {
        return Err(cursor_error(
            ErrorCode::InternalError,
            cursor.state().byte_offset,
            "pending subtree skip has invalid container progress",
        ));
    }
    cursor.skipping = Some(progress);

    loop {
        // Ordinary walk::next intentionally rejects active skip progress.
        // Advance the same block-mask parser directly over its retained owner
        // so all token, duplicate-key, and frame checks apply without copying.
        let event = match jmax_syntax::feed::next(&mut cursor.syntax)? {
            ScanStep::NeedMore => {
                if cursor.syntax.is_final() {
                    return Err(cursor_error(
                        ErrorCode::UnexpectedEof,
                        cursor.state().byte_offset,
                        "input ended inside a skipped subtree",
                    ));
                }
                return Ok(SkipStep::NeedMore);
            }
            ScanStep::Finished => {
                return Err(cursor_error(
                    ErrorCode::InternalError,
                    cursor.state().byte_offset,
                    "syntax finished with an unmatched skipped container",
                ));
            }
            ScanStep::Emit(event) => event,
        };
        cursor.current = Some(event);

        match event {
            Event::ObjectStart { .. } | Event::ArrayStart { .. } => {
                progress.depth = progress.depth.checked_add(1).ok_or_else(|| {
                    cursor_error(
                        ErrorCode::DepthLimitExceeded,
                        cursor.state().byte_offset,
                        "skipped subtree depth exceeds its representation",
                    )
                })?;
            }
            Event::ObjectEnd { offset } | Event::ArrayEnd { offset } => {
                progress.depth = progress.depth.checked_sub(1).ok_or_else(|| {
                    cursor_error(
                        ErrorCode::InternalError,
                        cursor.state().byte_offset,
                        "skipped subtree depth underflow",
                    )
                })?;
                if progress.depth == 0 {
                    let matching_end = matches!(
                        (progress.kind, event),
                        (ValueKind::Object, Event::ObjectEnd { .. })
                            | (ValueKind::Array, Event::ArrayEnd { .. })
                    );
                    if !matching_end {
                        return Err(cursor_error(
                            ErrorCode::InternalError,
                            cursor.state().byte_offset,
                            "skipped subtree ended with a different container kind",
                        ));
                    }
                    let span = container_span(progress.start, offset, cursor.state().byte_offset)?;
                    if span.slice(cursor.raw_bytes()).is_none() {
                        return Err(cursor_error(
                            ErrorCode::InternalError,
                            cursor.state().byte_offset,
                            "skipped subtree span exceeds retained input",
                        ));
                    }
                    cursor.skipping = None;
                    return Ok(SkipStep::Complete {
                        value: SelectedValue {
                            kind: progress.kind,
                            span,
                            decoded: None,
                        },
                        validated: progress.mode == SkipMode::Strict,
                    });
                }
            }
            Event::Key { .. } | Event::Scalar { .. } => {}
        }
        // Commit after every event, including keys and scalars, so NeedMore and
        // parser errors retain the last successfully consumed nesting depth.
        cursor.skipping = Some(progress);
    }
}

fn container_span(start: u32, close: u32, error_offset: u32) -> Result<Span32> {
    let end = close.checked_add(1).ok_or_else(|| {
        cursor_error(
            ErrorCode::PayloadTooLarge,
            error_offset,
            "skipped subtree end exceeds the span representation",
        )
    })?;
    let len = end
        .checked_sub(start)
        .filter(|&len| len >= 2)
        .ok_or_else(|| {
            cursor_error(
                ErrorCode::InternalError,
                error_offset,
                "skipped subtree delimiters are out of order",
            )
        })?;
    Ok(Span32::new(start, len))
}

/// Query-only boundary discovery. This result never carries a proof.
pub fn skip_query(
    cursor: &mut BlockCursor<'_>,
    decoded: &mut Vec<u8>,
    depth: u16,
) -> Result<UnvalidatedSpan> {
    let token_kind = match cursor.skip_whitespace()? {
        Some(b'{') => jmax_abi::syntax::TokenKind::LBrace,
        Some(b'[') => jmax_abi::syntax::TokenKind::LBracket,
        Some(b'"') => jmax_abi::syntax::TokenKind::String,
        Some(b't') => jmax_abi::syntax::TokenKind::True,
        Some(b'f') => jmax_abi::syntax::TokenKind::False,
        Some(b'n') => jmax_abi::syntax::TokenKind::Null,
        _ => jmax_abi::syntax::TokenKind::Number,
    } as u8;
    let span = cursor.skip_value(decoded, depth, ValueSkipMode::QueryOnly)?;
    Ok(UnvalidatedSpan { span, token_kind })
}
/// Full syntax/UTF-8/number/duplicate/limit validation without a DOM or captures.
/// Schema constraints remain the responsibility of the schema consumer.
/// The cursor reuses its strict-skip frames/tables for every subtree. Construct
/// it with `BlockCursor::with_scratch` to retain these capacities across inputs.
#[inline]
pub fn skip_strict(
    cursor: &mut BlockCursor<'_>,
    decoded: &mut Vec<u8>,
    depth: u16,
) -> Result<Span32> {
    cursor.skip_value(decoded, depth, ValueSkipMode::StrictValidate)
}
/// Fast skip authorized by a lifetime-bound syntax-validation witness.
pub fn skip_validated<'a>(
    cursor: &mut BlockCursor<'a>,
    decoded: &mut Vec<u8>,
    depth: u16,
    validation: &BorrowedValidation<'a>,
) -> Result<Span32> {
    validation.skip_value(cursor, decoded, depth)
}
