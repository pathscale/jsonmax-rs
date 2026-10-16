//! Allocation-free transitions for JSON root, object, and array phases.
//!
//! The caller owns lexical validation and the frame stack. A container start
//! advances its parent's phase; a close reports the departing frame's phase.

use super::*;

/// Whether a token can begin a JSON value, independently of its metadata.
pub fn is_value_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::LBrace
            | TokenKind::LBracket
            | TokenKind::String
            | TokenKind::Number
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Null
    )
}

/// Selects a transition without allocating or changing parser state.
///
/// Invalid token metadata takes precedence over phase errors. This checks span
/// arithmetic and metadata consistency only: the caller must validate token
/// bytes, UTF-8, escapes, and the actual decoded scalar count. No backing buffer
/// is accessed and no validation proof is issued.
pub fn transition(phase: FramePhase, token: Token) -> Result<Transition> {
    validate_metadata(token)?;

    let next_phase = match (phase, token.kind) {
        (FramePhase::RootDone, _) => {
            return Err(syntax_error(
                ErrorCode::TrailingContent,
                token.span.offset,
                "token after the root value",
            ));
        }
        (FramePhase::ObjectStart | FramePhase::ObjectKeyOrEnd, TokenKind::String) => {
            // Matching the reference avoids an unchecked extraction even though
            // metadata validation has already established its presence.
            if let Some(str_ref) = token.decoded {
                return Ok(Transition {
                    next_phase: FramePhase::ObjectColon,
                    action: FrameAction::None,
                    event: Some(Event::Key { str_ref }),
                });
            }
            return Err(invalid_metadata(token));
        }
        (FramePhase::ObjectColon, TokenKind::Colon) => {
            return Ok(Transition {
                next_phase: FramePhase::ObjectValue,
                action: FrameAction::None,
                event: None,
            });
        }
        (FramePhase::ObjectCommaOrEnd, TokenKind::Comma) => {
            return Ok(Transition {
                next_phase: FramePhase::ObjectKeyOrEnd,
                action: FrameAction::None,
                event: None,
            });
        }
        (FramePhase::ArrayCommaOrEnd, TokenKind::Comma) => {
            return Ok(Transition {
                next_phase: FramePhase::ArrayValueOrEnd,
                action: FrameAction::None,
                event: None,
            });
        }
        (FramePhase::ObjectStart | FramePhase::ObjectCommaOrEnd, TokenKind::RBrace) => {
            return Ok(Transition {
                next_phase: phase,
                action: FrameAction::Pop,
                event: Some(Event::ObjectEnd { offset: token.span.offset }),
            });
        }
        (FramePhase::ArrayStart | FramePhase::ArrayCommaOrEnd, TokenKind::RBracket) => {
            return Ok(Transition {
                next_phase: phase,
                action: FrameAction::Pop,
                event: Some(Event::ArrayEnd { offset: token.span.offset }),
            });
        }
        (FramePhase::RootStart, kind) if is_value_start(kind) => FramePhase::RootDone,
        (FramePhase::ObjectValue, kind) if is_value_start(kind) => {
            FramePhase::ObjectCommaOrEnd
        }
        (FramePhase::ArrayStart | FramePhase::ArrayValueOrEnd, kind)
            if is_value_start(kind) =>
        {
            FramePhase::ArrayCommaOrEnd
        }
        _ => return Err(unexpected_token(token)),
    };

    let (action, event) = match token.kind {
        TokenKind::LBrace => (
            FrameAction::PushObject,
            Event::ObjectStart { offset: token.span.offset },
        ),
        TokenKind::LBracket => (
            FrameAction::PushArray,
            Event::ArrayStart { offset: token.span.offset },
        ),
        kind => {
            let kind = match kind {
                TokenKind::String => ScalarKind::String,
                TokenKind::Number => ScalarKind::Number,
                TokenKind::True => ScalarKind::True,
                TokenKind::False => ScalarKind::False,
                TokenKind::Null => ScalarKind::Null,
                _ => return Err(unexpected_token(token)),
            };
            (
                FrameAction::None,
                Event::Scalar { kind, span: token.span, decoded: token.decoded },
            )
        }
    };
    Ok(Transition { next_phase, action, event: Some(event) })
}

fn invalid_metadata(token: Token) -> Error {
    syntax_error(ErrorCode::InvalidToken, token.span.offset, "invalid token metadata")
}

fn unexpected_token(token: Token) -> Error {
    syntax_error(
        ErrorCode::InvalidSyntax,
        token.span.offset,
        "unexpected token in the current JSON phase",
    )
}

fn validate_metadata(token: Token) -> Result<()> {
    if token.span.end().is_none() {
        return Err(invalid_metadata(token));
    }

    let valid_length = match token.kind {
        TokenKind::LBrace
        | TokenKind::RBrace
        | TokenKind::LBracket
        | TokenKind::RBracket
        | TokenKind::Colon
        | TokenKind::Comma => token.span.len == 1,
        TokenKind::String => token.span.len >= 2,
        TokenKind::Number => token.span.len != 0,
        TokenKind::True | TokenKind::Null => token.span.len == 4,
        TokenKind::False => token.span.len == 5,
    };
    if !valid_length {
        return Err(invalid_metadata(token));
    }

    match (token.kind, token.decoded) {
        (TokenKind::String, Some(reference)) => {
            let content_len = token.span.len - 2;
            // UTF-8 uses one to four bytes per scalar. Widen before multiplying
            // so an arbitrary public scalar count cannot overflow arithmetic.
            let scalar_bytes = u64::from(reference.scalar_len) * 4;
            if reference.span.end().is_none()
                || reference.span.len > content_len
                || reference.scalar_len > reference.span.len
                || u64::from(reference.span.len) > scalar_bytes
            {
                return Err(invalid_metadata(token));
            }
            if reference.source == SpanSource::RawPayload {
                // The checked token end and its minimum length make +1 safe.
                // An unescaped raw reference covers exactly the quoted content.
                if reference.span.offset != token.span.offset + 1
                    || reference.span.len != content_len
                {
                    return Err(invalid_metadata(token));
                }
            }
            Ok(())
        }
        (TokenKind::String, None) | (_, Some(_)) => Err(invalid_metadata(token)),
        (_, None) => Ok(()),
    }
}
