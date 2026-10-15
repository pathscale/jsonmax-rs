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

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [TokenKind; 11] = [
        TokenKind::LBrace,
        TokenKind::RBrace,
        TokenKind::LBracket,
        TokenKind::RBracket,
        TokenKind::Colon,
        TokenKind::Comma,
        TokenKind::String,
        TokenKind::Number,
        TokenKind::True,
        TokenKind::False,
        TokenKind::Null,
    ];
    const PHASES: [FramePhase; 10] = [
        FramePhase::RootStart,
        FramePhase::RootDone,
        FramePhase::ObjectStart,
        FramePhase::ObjectKeyOrEnd,
        FramePhase::ObjectColon,
        FramePhase::ObjectValue,
        FramePhase::ObjectCommaOrEnd,
        FramePhase::ArrayStart,
        FramePhase::ArrayValueOrEnd,
        FramePhase::ArrayCommaOrEnd,
    ];

    fn token_at(kind: TokenKind, offset: u32) -> Token {
        let len = match kind {
            TokenKind::String => 3,
            TokenKind::True | TokenKind::Null => 4,
            TokenKind::False => 5,
            _ => 1,
        };
        Token {
            kind,
            span: Span32::new(offset, len),
            decoded: if kind == TokenKind::String {
                Some(StrRef {
                    span: Span32::new(offset + 1, 1),
                    source: SpanSource::RawPayload,
                    scalar_len: 1,
                })
            } else {
                None
            },
        }
    }

    fn assert_error(phase: FramePhase, token: Token, code: ErrorCode) {
        let error = transition(phase, token).unwrap_err();
        assert_eq!(error.code, code, "phase={phase:?}, token={token:?}");
        assert_eq!(error.source, ErrorSource::Syntax);
        assert_eq!(error.byte_offset, token.span.offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn value_start_classification_is_exact() {
        let expected = [true, false, true, false, false, false, true, true, true, true, true];
        for (kind, expected) in KINDS.into_iter().zip(expected) {
            assert_eq!(is_value_start(kind), expected, "kind={kind:?}");
        }
    }

    #[test]
    fn all_phase_token_pairs_match_the_frozen_acceptance_table() {
        // Columns follow KINDS; rows follow PHASES. This independent table also
        // covers missing separators, mismatched closes, and non-string keys.
        let accepted = [
            [true, false, true, false, false, false, true, true, true, true, true],
            [false; 11],
            [false, true, false, false, false, false, true, false, false, false, false],
            [false, false, false, false, false, false, true, false, false, false, false],
            [false, false, false, false, true, false, false, false, false, false, false],
            [true, false, true, false, false, false, true, true, true, true, true],
            [false, true, false, false, false, true, false, false, false, false, false],
            [true, false, true, true, false, false, true, true, true, true, true],
            [true, false, true, false, false, false, true, true, true, true, true],
            [false, false, false, true, false, true, false, false, false, false, false],
        ];
        for (phase, row) in PHASES.into_iter().zip(accepted) {
            for (kind, accepts) in KINDS.into_iter().zip(row) {
                let token = token_at(kind, 37);
                if accepts {
                    assert!(transition(phase, token).is_ok(), "phase={phase:?}, kind={kind:?}");
                } else {
                    let code = if phase == FramePhase::RootDone {
                        ErrorCode::TrailingContent
                    } else {
                        ErrorCode::InvalidSyntax
                    };
                    assert_error(phase, token, code);
                }
            }
        }
    }

    #[test]
    fn every_value_advances_its_parent_and_preserves_its_event() {
        let contexts = [
            (FramePhase::RootStart, FramePhase::RootDone),
            (FramePhase::ObjectValue, FramePhase::ObjectCommaOrEnd),
            (FramePhase::ArrayStart, FramePhase::ArrayCommaOrEnd),
            (FramePhase::ArrayValueOrEnd, FramePhase::ArrayCommaOrEnd),
        ];
        let values = [
            (TokenKind::LBrace, FrameAction::PushObject, Event::ObjectStart { offset: 19 }),
            (TokenKind::LBracket, FrameAction::PushArray, Event::ArrayStart { offset: 19 }),
            (TokenKind::String, FrameAction::None, Event::Scalar {
                kind: ScalarKind::String,
                span: Span32::new(19, 3),
                decoded: Some(StrRef {
                    span: Span32::new(20, 1),
                    source: SpanSource::RawPayload,
                    scalar_len: 1,
                }),
            }),
            (TokenKind::Number, FrameAction::None, Event::Scalar {
                kind: ScalarKind::Number, span: Span32::new(19, 1), decoded: None,
            }),
            (TokenKind::True, FrameAction::None, Event::Scalar {
                kind: ScalarKind::True, span: Span32::new(19, 4), decoded: None,
            }),
            (TokenKind::False, FrameAction::None, Event::Scalar {
                kind: ScalarKind::False, span: Span32::new(19, 5), decoded: None,
            }),
            (TokenKind::Null, FrameAction::None, Event::Scalar {
                kind: ScalarKind::Null, span: Span32::new(19, 4), decoded: None,
            }),
        ];
        for (phase, next_phase) in contexts {
            for (kind, action, event) in values {
                assert_eq!(
                    transition(phase, token_at(kind, 19)).unwrap(),
                    Transition { next_phase, action, event: Some(event) },
                );
            }
        }
    }

    #[test]
    fn keys_preserve_cooked_references_from_either_owner() {
        let raw = token_at(TokenKind::String, 81);
        // A surrogate pair can decode to one four-byte UTF-8 scalar.
        let escaped = Token {
            kind: TokenKind::String,
            span: Span32::new(81, 14),
            decoded: Some(StrRef {
                span: Span32::new(900, 4),
                source: SpanSource::DecodedArena,
                scalar_len: 1,
            }),
        };
        for phase in [FramePhase::ObjectStart, FramePhase::ObjectKeyOrEnd] {
            for token in [raw, escaped] {
                assert_eq!(transition(phase, token).unwrap(), Transition {
                    next_phase: FramePhase::ObjectColon,
                    action: FrameAction::None,
                    event: Some(Event::Key { str_ref: token.decoded.unwrap() }),
                });
            }
        }
        assert_eq!(transition(FramePhase::RootStart, escaped).unwrap().event, Some(Event::Scalar {
            kind: ScalarKind::String, span: escaped.span, decoded: escaped.decoded,
        }));
    }

    #[test]
    fn punctuation_is_silent_and_closes_preserve_the_departing_phase() {
        for (phase, kind, next_phase) in [
            (FramePhase::ObjectColon, TokenKind::Colon, FramePhase::ObjectValue),
            (FramePhase::ObjectCommaOrEnd, TokenKind::Comma, FramePhase::ObjectKeyOrEnd),
            (FramePhase::ArrayCommaOrEnd, TokenKind::Comma, FramePhase::ArrayValueOrEnd),
        ] {
            assert_eq!(transition(phase, token_at(kind, 0)).unwrap(), Transition {
                next_phase, action: FrameAction::None, event: None,
            });
        }
        for (phase, kind, event) in [
            (FramePhase::ObjectStart, TokenKind::RBrace, Event::ObjectEnd { offset: 67 }),
            (FramePhase::ObjectCommaOrEnd, TokenKind::RBrace, Event::ObjectEnd { offset: 67 }),
            (FramePhase::ArrayStart, TokenKind::RBracket, Event::ArrayEnd { offset: 67 }),
            (FramePhase::ArrayCommaOrEnd, TokenKind::RBracket, Event::ArrayEnd { offset: 67 }),
        ] {
            assert_eq!(transition(phase, token_at(kind, 67)).unwrap(), Transition {
                next_phase: phase, action: FrameAction::Pop, event: Some(event),
            });
        }
    }

    #[test]
    fn comma_requires_another_key_or_value() {
        for (before_comma, close) in [
            (FramePhase::ObjectCommaOrEnd, TokenKind::RBrace),
            (FramePhase::ArrayCommaOrEnd, TokenKind::RBracket),
        ] {
            let after_comma = transition(before_comma, token_at(TokenKind::Comma, 10)).unwrap();
            assert_error(after_comma.next_phase, token_at(close, 11), ErrorCode::InvalidSyntax);
            assert_error(
                after_comma.next_phase,
                token_at(TokenKind::Comma, 11),
                ErrorCode::InvalidSyntax,
            );
        }
    }

    #[test]
    fn nested_container_sequence_keeps_parent_phases() {
        // Token sequence for {"x":[{},null]}. Explicit saved phases model the
        // caller's frame stack without depending on later-wave modules.
        let root = transition(FramePhase::RootStart, token_at(TokenKind::LBrace, 0)).unwrap();
        let key = transition(FramePhase::ObjectStart, token_at(TokenKind::String, 1)).unwrap();
        let colon = transition(key.next_phase, token_at(TokenKind::Colon, 4)).unwrap();
        let object = transition(colon.next_phase, token_at(TokenKind::LBracket, 5)).unwrap();
        let array = transition(FramePhase::ArrayStart, token_at(TokenKind::LBrace, 6)).unwrap();
        let empty = transition(FramePhase::ObjectStart, token_at(TokenKind::RBrace, 7)).unwrap();
        assert_eq!(empty.next_phase, FramePhase::ObjectStart);
        assert_eq!(empty.action, FrameAction::Pop);
        let comma = transition(array.next_phase, token_at(TokenKind::Comma, 8)).unwrap();
        let null = transition(comma.next_phase, token_at(TokenKind::Null, 9)).unwrap();
        let end_array = transition(null.next_phase, token_at(TokenKind::RBracket, 13)).unwrap();
        let end_object = transition(object.next_phase, token_at(TokenKind::RBrace, 14)).unwrap();
        assert_eq!(root.next_phase, FramePhase::RootDone);
        assert_eq!(object.next_phase, FramePhase::ObjectCommaOrEnd);
        assert_eq!(array.next_phase, FramePhase::ArrayCommaOrEnd);
        assert_eq!(end_array.action, FrameAction::Pop);
        assert_eq!(end_object.action, FrameAction::Pop);
        assert_error(root.next_phase, token_at(TokenKind::Null, 15), ErrorCode::TrailingContent);
    }

    #[test]
    fn decoded_presence_is_enforced_in_every_phase_before_syntax() {
        for phase in PHASES {
            for kind in KINDS {
                let mut token = token_at(kind, 23);
                token.decoded = if kind == TokenKind::String {
                    None
                } else {
                    Some(StrRef {
                        span: Span32::new(0, 0),
                        source: SpanSource::DecodedArena,
                        scalar_len: 0,
                    })
                };
                assert_error(phase, token, ErrorCode::InvalidToken);
            }
        }
    }

    #[test]
    fn invalid_token_lengths_are_rejected_before_phase_errors() {
        for kind in KINDS {
            let invalid_lengths: &[u32] = match kind {
                TokenKind::String => &[0, 1],
                TokenKind::Number => &[0],
                TokenKind::True | TokenKind::Null => &[0, 1, 3, 5],
                TokenKind::False => &[0, 1, 4, 6],
                _ => &[0, 2, 4],
            };
            for &len in invalid_lengths {
                let mut token = token_at(kind, 31);
                token.span.len = len;
                for phase in PHASES {
                    assert_error(phase, token, ErrorCode::InvalidToken);
                }
            }
        }
    }

    #[test]
    fn span_ends_must_fit_u32_and_the_maximum_end_is_accepted() {
        for kind in KINDS {
            let mut token = token_at(kind, 0);
            token.span.offset = u32::MAX;
            assert_error(FramePhase::RootStart, token, ErrorCode::InvalidToken);
        }
        let token = token_at(TokenKind::LBrace, u32::MAX - 1);
        assert_eq!(transition(FramePhase::RootStart, token).unwrap().event,
            Some(Event::ObjectStart { offset: u32::MAX - 1 }));
        let token = token_at(TokenKind::String, u32::MAX - 3);
        assert_eq!(transition(FramePhase::RootStart, token).unwrap().event, Some(Event::Scalar {
            kind: ScalarKind::String, span: token.span, decoded: token.decoded,
        }));
        let token = Token {
            kind: TokenKind::Number,
            span: Span32::new(0, u32::MAX),
            decoded: None,
        };
        assert_eq!(transition(FramePhase::RootStart, token).unwrap().event, Some(Event::Scalar {
            kind: ScalarKind::Number, span: token.span, decoded: None,
        }));
    }

    #[test]
    fn empty_and_multibyte_strings_preserve_owner_and_scalar_count() {
        for (content_len, scalar_len) in [(0, 0), (1, 1), (2, 1), (3, 1), (4, 1), (5, 2)] {
            for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
                let reference = StrRef {
                    span: Span32::new(if source == SpanSource::RawPayload { 41 } else { 600 }, content_len),
                    source,
                    scalar_len,
                };
                let token = Token {
                    kind: TokenKind::String,
                    span: Span32::new(40, content_len + 2),
                    decoded: Some(reference),
                };
                assert_eq!(transition(FramePhase::ArrayStart, token).unwrap().event, Some(Event::Scalar {
                    kind: ScalarKind::String, span: token.span, decoded: Some(reference),
                }));
            }
        }
        let token = Token {
            kind: TokenKind::String,
            span: Span32::new(0, 2),
            decoded: Some(StrRef {
                span: Span32::new(u32::MAX, 0),
                source: SpanSource::DecodedArena,
                scalar_len: 0,
            }),
        };
        assert!(transition(FramePhase::ObjectStart, token).is_ok());
    }

    #[test]
    fn inconsistent_string_references_are_invalid_tokens() {
        let references = [
            // Overflowing arena range.
            StrRef { span: Span32::new(u32::MAX, 1), source: SpanSource::DecodedArena, scalar_len: 1 },
            // Decoding cannot expand the JSON content's byte length.
            StrRef { span: Span32::new(0, 5), source: SpanSource::DecodedArena, scalar_len: 5 },
            // Impossible UTF-8 scalar counts, including multiplication extremes.
            StrRef { span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 0 },
            StrRef { span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 2 },
            StrRef { span: Span32::new(0, 0), source: SpanSource::DecodedArena, scalar_len: 1 },
            StrRef { span: Span32::new(0, 4), source: SpanSource::DecodedArena, scalar_len: u32::MAX },
            // Raw ranges must exclude both quotes and cover all raw content.
            StrRef { span: Span32::new(50, 4), source: SpanSource::RawPayload, scalar_len: 4 },
            StrRef { span: Span32::new(52, 4), source: SpanSource::RawPayload, scalar_len: 4 },
            StrRef { span: Span32::new(51, 3), source: SpanSource::RawPayload, scalar_len: 3 },
        ];
        for reference in references {
            let token = Token {
                kind: TokenKind::String,
                span: Span32::new(50, 6),
                decoded: Some(reference),
            };
            for phase in PHASES {
                assert_error(phase, token, ErrorCode::InvalidToken);
            }
        }
        let token = Token {
            kind: TokenKind::String,
            span: Span32::new(0, 7),
            decoded: Some(StrRef {
                span: Span32::new(100, 5),
                source: SpanSource::DecodedArena,
                scalar_len: 1,
            }),
        };
        assert_error(FramePhase::RootStart, token, ErrorCode::InvalidToken);
    }

    #[test]
    fn large_cooked_counts_do_not_overflow_and_arena_offsets_are_independent() {
        let token = Token {
            kind: TokenKind::String,
            span: Span32::new(0, u32::MAX),
            decoded: Some(StrRef {
                span: Span32::new(2, u32::MAX - 2),
                source: SpanSource::DecodedArena,
                scalar_len: u32::MAX - 2,
            }),
        };
        let first = transition(FramePhase::RootStart, token).unwrap();
        let second = transition(FramePhase::RootStart, token).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.event, Some(Event::Scalar {
            kind: ScalarKind::String, span: token.span, decoded: token.decoded,
        }));
        let token = Token {
            kind: TokenKind::String,
            span: Span32::new(7, 8),
            decoded: Some(StrRef {
                span: Span32::new(u32::MAX - 1, 1),
                source: SpanSource::DecodedArena,
                scalar_len: 1,
            }),
        };
        assert!(transition(FramePhase::RootStart, token).is_ok());
    }
}
