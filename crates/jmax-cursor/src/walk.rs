//! Forward, tapeless traversal over the syntax parser's owned input.
//!
//! Feeding bytes and declaring EOF do not consume events or establish validity.
//! Only advancing through [`ScanStep::Finished`] validates the complete input.

use super::*;

/// Copies a complete payload into a new cursor and declares end of input.
///
/// The cursor retains its own bytes after the supplied owner is dropped. No
/// events are consumed: even empty or malformed input is diagnosed by traversal,
/// unless copying the payload itself exceeds a limit or fails to allocate.
pub fn from_payload(payload: Payload, limits: Limits, mode: SkipMode) -> Result<Cursor> {
    let mut cursor = Cursor::new(limits, mode);
    push(&mut cursor, payload.as_slice())?;
    end_input(&mut cursor)?;
    Ok(cursor)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(kind: ScalarKind, offset: u32, len: u32) -> Event {
        Event::Scalar { kind, span: Span32::new(offset, len), decoded: None }
    }

    fn raw_string(offset: u32, len: u32, scalar_len: u32) -> StrRef {
        StrRef { span: Span32::new(offset, len), source: SpanSource::RawPayload, scalar_len }
    }

    fn complete(bytes: &[u8], mode: SkipMode) -> Cursor {
        from_payload(Payload::from_slice(bytes), Limits::strict_default(), mode).unwrap()
    }

    // Bound the loop so a regression that replays an event fails deterministically.
    fn drain(cursor: &mut Cursor) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        for _ in 0..=cursor.raw_bytes().len() + 1 {
            let previous = cursor.current();
            match next(cursor)? {
                ScanStep::Emit(event) => {
                    assert_eq!(cursor.current(), Some(event));
                    events.push(event);
                }
                ScanStep::Finished => {
                    assert_eq!(cursor.current(), previous);
                    assert!(cursor.state().finished);
                    return Ok(events);
                }
                ScanStep::NeedMore => panic!("final input unexpectedly needs another chunk"),
            }
        }
        panic!("event traversal did not terminate");
    }

    fn drain_chunk(cursor: &mut Cursor, events: &mut Vec<Event>) {
        for _ in 0..=cursor.raw_bytes().len() + 1 {
            let previous = cursor.current();
            match next(cursor).unwrap() {
                ScanStep::Emit(event) => {
                    assert_eq!(cursor.current(), Some(event));
                    events.push(event);
                }
                ScanStep::NeedMore => {
                    assert_eq!(cursor.current(), previous);
                    assert!(!cursor.state().finished);
                    return;
                }
                ScanStep::Finished => panic!("chunk exhaustion was mistaken for EOF"),
            }
        }
        panic!("chunk traversal did not terminate");
    }

    fn assert_same_state(actual: CursorState, expected: CursorState) {
        assert_eq!(actual.byte_offset, expected.byte_offset);
        assert_eq!(actual.depth, expected.depth);
        assert_eq!(actual.phase, expected.phase);
        assert_eq!(actual.finished, expected.finished);
    }

    #[test]
    fn construction_copies_both_payload_variants_without_consuming_events() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for payload in [Payload::from_slice(b"[0]"), Payload::from_vec(b"[0]".to_vec())] {
                let owner = payload.clone();
                let mut cursor = from_payload(payload, Limits::strict_default(), mode).unwrap();
                assert_eq!(cursor.raw_bytes(), owner.as_slice());
                assert_ne!(cursor.raw_bytes().as_ptr(), owner.as_slice().as_ptr());
                drop(owner);
                assert_eq!(cursor.mode(), mode);
                assert_eq!(cursor.current(), None);
                assert_same_state(cursor.state(), Cursor::new(Limits::strict_default(), mode).state());
                assert!(cursor.syntax.is_final());
                assert!(!cursor.syntax.is_finished());
                assert!(cursor.decoded_bytes().is_empty());
                assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(Event::ArrayStart { offset: 0 }));
            }
        }
    }

    #[test]
    fn nested_document_has_exact_order_offsets_spans_and_balanced_events() {
        let bytes = br#"{"a":[true,false,null,-12.5e+2,"x"],"b":{}}"#;
        let expected = [
            Event::ObjectStart { offset: 0 },
            Event::Key { str_ref: raw_string(2, 1, 1) },
            Event::ArrayStart { offset: 5 },
            scalar(ScalarKind::True, 6, 4),
            scalar(ScalarKind::False, 11, 5),
            scalar(ScalarKind::Null, 17, 4),
            scalar(ScalarKind::Number, 22, 8),
            Event::Scalar {
                kind: ScalarKind::String,
                span: Span32::new(31, 3),
                decoded: Some(raw_string(32, 1, 1)),
            },
            Event::ArrayEnd { offset: 34 },
            Event::Key { str_ref: raw_string(37, 1, 1) },
            Event::ObjectStart { offset: 40 },
            Event::ObjectEnd { offset: 41 },
            Event::ObjectEnd { offset: 42 },
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = complete(bytes, mode);
            for event in expected {
                assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(event));
                assert_eq!(cursor.current(), Some(event));
                assert!(!cursor.state().finished);
            }
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::Finished);
            assert_eq!(cursor.state().depth, 0);
            assert_eq!(cursor.state().byte_offset as usize, bytes.len());
            assert_eq!(cursor.current(), expected.last().copied());
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::Finished);
        }
    }

    #[test]
    fn all_scalar_roots_and_empty_containers_are_supported() {
        let cases: &[(&[u8], &[Event])] = &[
            (b"null", &[scalar(ScalarKind::Null, 0, 4)]),
            (b"true", &[scalar(ScalarKind::True, 0, 4)]),
            (b"false", &[scalar(ScalarKind::False, 0, 5)]),
            (b"-0.25e+10", &[scalar(ScalarKind::Number, 0, 9)]),
            (b"\"\"", &[Event::Scalar {
                kind: ScalarKind::String,
                span: Span32::new(0, 2),
                decoded: Some(raw_string(1, 0, 0)),
            }]),
            (b"[]", &[Event::ArrayStart { offset: 0 }, Event::ArrayEnd { offset: 1 }]),
            (b"{}", &[Event::ObjectStart { offset: 0 }, Event::ObjectEnd { offset: 1 }]),
        ];
        for &(bytes, expected) in cases {
            let mut cursor = complete(bytes, SkipMode::Strict);
            assert_eq!(drain(&mut cursor).unwrap().as_slice(), expected);
            assert_eq!(cursor.current(), expected.last().copied());
        }
    }

    #[test]
    fn every_two_chunk_split_matches_complete_traversal() {
        let bytes = r#" {"é\u0061":["a\\\"b","\uD83D\uDE00",-1.25e-3,true,false,null,{},[]]} "#.as_bytes();
        let mut baseline = complete(bytes, SkipMode::Strict);
        let expected = drain(&mut baseline).unwrap();
        let decoded = baseline.decoded_bytes().to_vec();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for split in 0..=bytes.len() {
                let mut cursor = Cursor::new(Limits::strict_default(), mode);
                let mut events = Vec::new();
                push(&mut cursor, &bytes[..split]).unwrap();
                drain_chunk(&mut cursor, &mut events);
                let state = cursor.state();
                let current = cursor.current();
                push(&mut cursor, &bytes[split..]).unwrap();
                assert_same_state(cursor.state(), state);
                assert_eq!(cursor.current(), current);
                end_input(&mut cursor).unwrap();
                events.extend(drain(&mut cursor).unwrap());
                assert_eq!(events, expected, "split at byte {split}");
                assert_eq!(cursor.raw_bytes(), bytes);
                assert_eq!(cursor.decoded_bytes(), decoded.as_slice());
            }
        }
    }

    #[test]
    fn bytewise_feeding_owns_chunks_and_preserves_string_references() {
        let bytes = r#"["é","\uD83D\uDE00","\n"]"#.as_bytes();
        let mut cursor = Cursor::new(Limits::strict_default(), SkipMode::Strict);
        let mut events = Vec::new();
        for &byte in bytes {
            let mut chunk = [byte];
            push(&mut cursor, &chunk).unwrap();
            chunk[0] = b'!';
            assert_eq!(chunk, [b'!']);
            drain_chunk(&mut cursor, &mut events);
            push(&mut cursor, &[]).unwrap();
        }
        end_input(&mut cursor).unwrap();
        events.extend(drain(&mut cursor).unwrap());
        assert_eq!(events.len(), 5);
        let mut strings = Vec::new();
        for event in events {
            if let Event::Scalar { kind: ScalarKind::String, span, decoded: Some(reference) } = event {
                let raw = span.slice(cursor.raw_bytes()).unwrap();
                assert_eq!(raw.first(), Some(&b'"'));
                assert_eq!(raw.last(), Some(&b'"'));
                assert_eq!(reference.scalar_len, 1);
                strings.push(cursor.resolve_str(reference).unwrap());
            }
        }
        assert_eq!(strings.as_slice(), &["é", "😀", "\n"]);
        assert_eq!(cursor.raw_bytes(), bytes);
    }

    #[test]
    fn incomplete_string_preserves_current_and_decodes_only_once() {
        let mut cursor = Cursor::new(Limits::strict_default(), SkipMode::Strict);
        push(&mut cursor, b"[\"\\uD83D").unwrap();
        let opening = Event::ArrayStart { offset: 0 };
        assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(opening));
        for _ in 0..3 {
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::NeedMore);
            assert_eq!(cursor.current(), Some(opening));
            assert_eq!(cursor.state().byte_offset, 1);
            assert!(cursor.decoded_bytes().is_empty());
        }
        push(&mut cursor, b"\\uDE00\"").unwrap();
        let reference = StrRef {
            span: Span32::new(0, 4),
            source: SpanSource::DecodedArena,
            scalar_len: 1,
        };
        let string = Event::Scalar {
            kind: ScalarKind::String,
            span: Span32::new(1, 14),
            decoded: Some(reference),
        };
        assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(string));
        for _ in 0..3 {
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::NeedMore);
            assert_eq!(cursor.current(), Some(string));
            assert_eq!(cursor.decoded_bytes(), "😀".as_bytes());
            assert_eq!(cursor.resolve_str(reference), Some("😀"));
        }
        push(&mut cursor, b"]").unwrap();
        end_input(&mut cursor).unwrap();
        assert_eq!(drain(&mut cursor).unwrap(), [Event::ArrayEnd { offset: 15 }]);
        assert_eq!(cursor.resolve_str(reference), Some("😀"));
    }

    #[test]
    fn need_more_and_finished_preserve_the_last_event() {
        let mut cursor = Cursor::new(Limits::strict_default(), SkipMode::Strict);
        assert_eq!(next(&mut cursor).unwrap(), ScanStep::NeedMore);
        assert_eq!(cursor.current(), None);
        push(&mut cursor, b"[] \t\r\n").unwrap();
        assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(Event::ArrayStart { offset: 0 }));
        assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(Event::ArrayEnd { offset: 1 }));
        for _ in 0..3 {
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::NeedMore);
            assert_eq!(cursor.current(), Some(Event::ArrayEnd { offset: 1 }));
        }
        let state = cursor.state();
        end_input(&mut cursor).unwrap();
        end_input(&mut cursor).unwrap();
        assert_same_state(cursor.state(), state);
        for _ in 0..3 {
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::Finished);
            assert_eq!(cursor.current(), Some(Event::ArrayEnd { offset: 1 }));
        }
        end_input(&mut cursor).unwrap();
    }

    #[test]
    fn numbers_and_keywords_wait_for_a_boundary_or_eof() {
        for (bytes, kind) in [
            (&b"12"[..], ScalarKind::Number),
            (&b"true"[..], ScalarKind::True),
            (&b"false"[..], ScalarKind::False),
            (&b"null"[..], ScalarKind::Null),
        ] {
            for use_eof in [false, true] {
                let mut cursor = Cursor::new(Limits::strict_default(), SkipMode::Strict);
                push(&mut cursor, bytes).unwrap();
                assert_eq!(next(&mut cursor).unwrap(), ScanStep::NeedMore);
                assert_eq!(cursor.current(), None);
                assert_eq!(cursor.state().byte_offset, 0);
                if use_eof {
                    end_input(&mut cursor).unwrap();
                } else {
                    push(&mut cursor, b" ").unwrap();
                }
                assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(scalar(kind, 0, bytes.len() as u32)));
                end_input(&mut cursor).unwrap();
                assert_eq!(next(&mut cursor).unwrap(), ScanStep::Finished);
            }
        }
    }

    #[test]
    fn empty_and_whitespace_input_fail_only_when_advanced() {
        for bytes in [&b""[..], &b" \t\r\n"[..]] {
            let mut cursor = complete(bytes, SkipMode::Strict);
            assert_eq!(cursor.current(), None);
            end_input(&mut cursor).unwrap();
            let error = next(&mut cursor).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnexpectedEof);
            assert_eq!(error.source, ErrorSource::Syntax);
            assert_eq!(error.byte_offset as usize, bytes.len());
            assert_eq!(cursor.current(), None);
            assert_eq!(next(&mut cursor).unwrap_err(), error);
        }
    }

    #[test]
    fn malformed_input_propagates_exact_syntax_failures_in_both_modes() {
        let cases: &[&[u8]] = &[
            b"[", b"{\"a\":", b"[0,]", b"{\"a\":0,}", b"{0:1}",
            b"[}", b"null false", b"tru", b"truex", b"01", b"1e",
            b"\"unterminated", b"\"\\q\"", b"\"\\uD800\"", b"\"\x01\"",
            b"\"\xff\"", br#"{"a":0,"\u0061":1}"#,
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &bytes in cases {
                let mut cursor = complete(bytes, mode);
                let mut syntax = SyntaxState::new(Limits::strict_default());
                jmax_syntax::feed::push(&mut syntax, bytes).unwrap();
                jmax_syntax::finish::end_input(&mut syntax).unwrap();
                let mut failed = false;
                for _ in 0..=bytes.len() + 1 {
                    let previous = cursor.current();
                    let expected = jmax_syntax::feed::next(&mut syntax);
                    let actual = next(&mut cursor);
                    assert_eq!(actual, expected);
                    match actual {
                        Err(error) => {
                            assert_eq!(cursor.current(), previous);
                            assert_eq!(next(&mut cursor).unwrap_err(), error);
                            assert_eq!(push(&mut cursor, b" ").unwrap_err(), error);
                            assert_eq!(end_input(&mut cursor).unwrap_err(), error);
                            assert_eq!(cursor.raw_bytes(), bytes);
                            failed = true;
                            break;
                        }
                        Ok(ScanStep::Emit(_)) => {}
                        Ok(_) => panic!("malformed final input was not rejected"),
                    }
                }
                assert!(failed, "malformed input did not reach an error");
            }
        }
    }

    #[test]
    fn eof_and_trailing_content_errors_have_absolute_offsets() {
        let cases: &[(&[u8], ErrorCode, u32)] = &[
            (b"[", ErrorCode::UnexpectedEof, 1),
            (b"{\"a\":", ErrorCode::UnexpectedEof, 5),
            (b"  \"x", ErrorCode::UnclosedString, 2),
            (b"null \nfalse", ErrorCode::TrailingContent, 6),
            (b"[0,]", ErrorCode::InvalidSyntax, 3),
        ];
        for &(bytes, code, offset) in cases {
            let mut cursor = complete(bytes, SkipMode::Query);
            let error = drain(&mut cursor).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.byte_offset, offset);
            assert!(!cursor.state().finished);
        }
    }

    #[test]
    fn payload_limit_is_inclusive_and_failed_push_appends_nothing() {
        let limits = Limits { max_payload_bytes: 4, ..Limits::strict_default() };
        let mut exact = from_payload(Payload::from_slice(b"null"), limits, SkipMode::Strict).unwrap();
        assert_eq!(drain(&mut exact).unwrap(), [scalar(ScalarKind::Null, 0, 4)]);
        let error = from_payload(Payload::from_slice(b"null "), limits, SkipMode::Strict).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Syntax);
        assert_eq!(error.byte_offset, 0);

        let mut cursor = Cursor::new(limits, SkipMode::Strict);
        push(&mut cursor, b"null").unwrap();
        let before = cursor.state();
        let error = push(&mut cursor, b" ").unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.byte_offset, 4);
        assert_eq!(cursor.raw_bytes(), b"null");
        assert_same_state(cursor.state(), before);
        assert_eq!(cursor.current(), None);
        assert_eq!(next(&mut cursor).unwrap_err(), error);
        assert_eq!(push(&mut cursor, &[]).unwrap_err(), error);
        assert_eq!(end_input(&mut cursor).unwrap_err(), error);

        let zero = Limits { max_payload_bytes: 0, ..limits };
        assert!(from_payload(Payload::from_slice(b""), zero, SkipMode::Strict).is_ok());
        assert_eq!(from_payload(Payload::from_slice(b"0"), zero, SkipMode::Strict).unwrap_err().code,
            ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn depth_and_container_count_limits_are_delegated() {
        let zero_depth = Limits { max_depth: 0, ..Limits::strict_default() };
        let mut scalar_cursor = from_payload(Payload::from_slice(b"0"), zero_depth, SkipMode::Strict).unwrap();
        assert!(drain(&mut scalar_cursor).is_ok());
        let mut container = from_payload(Payload::from_slice(b"[]"), zero_depth, SkipMode::Strict).unwrap();
        let error = next(&mut container).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(container.current(), None);

        let limits = Limits { max_depth: 1, max_array_items: 1, max_fields_per_object: 1,
            ..Limits::strict_default() };
        for bytes in [&b"[]"[..], &b"[0]"[..], &b"{}"[..], &b"{\"a\":0}"[..]] {
            let mut cursor = from_payload(Payload::from_slice(bytes), limits, SkipMode::Strict).unwrap();
            assert!(drain(&mut cursor).is_ok());
        }
        for (bytes, code) in [
            (&b"[[]]"[..], ErrorCode::DepthLimitExceeded),
            (&b"[0,1]"[..], ErrorCode::ConstraintViolation),
            (&b"{\"a\":0,\"b\":1}"[..], ErrorCode::ConstraintViolation),
        ] {
            let mut cursor = from_payload(Payload::from_slice(bytes), limits, SkipMode::Query).unwrap();
            let error = drain(&mut cursor).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.source, ErrorSource::Syntax);
        }
    }

    #[test]
    fn pushing_after_eof_is_sticky_even_for_empty_chunks() {
        for finished in [false, true] {
            for chunk in [&b""[..], &b" "[..]] {
                let mut cursor = complete(b"null", SkipMode::Strict);
                if finished {
                    drain(&mut cursor).unwrap();
                }
                let before = cursor.state();
                let current = cursor.current();
                let error = push(&mut cursor, chunk).unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidSyntax);
                assert_eq!(error.source, ErrorSource::Syntax);
                assert_eq!(error.byte_offset, 4);
                assert_same_state(cursor.state(), before);
                assert_eq!(cursor.current(), current);
                assert_eq!(cursor.raw_bytes(), b"null");
                assert_eq!(next(&mut cursor).unwrap_err(), error);
                assert_eq!(end_input(&mut cursor).unwrap_err(), error);
            }
        }
    }

    #[test]
    fn suspended_skip_blocks_walk_without_mutation_and_allows_feeding() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = Cursor::new(Limits::strict_default(), mode);
            push(&mut cursor, b"[").unwrap();
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::Emit(Event::ArrayStart { offset: 0 }));
            // Install the frozen skip carry directly to isolate this wave from
            // the later subtree implementation.
            let progress = SkipProgress { start: 0, kind: ValueKind::Array, depth: 1, mode };
            cursor.skipping = Some(progress);
            let state = cursor.state();
            let current = cursor.current();
            for _ in 0..2 {
                let error = next(&mut cursor).unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidSyntax);
                assert_eq!(error.source, ErrorSource::Cursor);
                assert_eq!(error.byte_offset, state.byte_offset);
                assert_same_state(cursor.state(), state.clone());
                assert_eq!(cursor.current(), current);
                assert_eq!(cursor.skipping, Some(progress));
                assert_eq!(cursor.raw_bytes(), b"[");
                assert!(cursor.decoded_bytes().is_empty());
                assert!(cursor.syntax.failure().is_none());
            }
            push(&mut cursor, b"]").unwrap();
            end_input(&mut cursor).unwrap();
            assert_eq!(next(&mut cursor).unwrap_err().source, ErrorSource::Cursor);
            assert_eq!(cursor.skipping, Some(progress));
            // Model completion by the skip owner, which advances syntax directly.
            let closing = Event::ArrayEnd { offset: 1 };
            assert_eq!(jmax_syntax::feed::next(&mut cursor.syntax).unwrap(), ScanStep::Emit(closing));
            cursor.current = Some(closing);
            cursor.skipping = None;
            assert_eq!(next(&mut cursor).unwrap(), ScanStep::Finished);
            assert_eq!(cursor.current(), Some(closing));
        }
    }

    #[test]
    fn syntax_failure_during_suspended_skip_is_not_masked() {
        let limits = Limits { max_payload_bytes: 1, ..Limits::strict_default() };
        let mut cursor = Cursor::new(limits, SkipMode::Strict);
        push(&mut cursor, b"[").unwrap();
        next(&mut cursor).unwrap();
        let progress = SkipProgress { start: 0, kind: ValueKind::Array, depth: 1, mode: SkipMode::Strict };
        cursor.skipping = Some(progress);
        let error = push(&mut cursor, b"]").unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Syntax);
        let state = cursor.state();
        assert_eq!(next(&mut cursor).unwrap_err(), error);
        assert_eq!(end_input(&mut cursor).unwrap_err(), error);
        assert_same_state(cursor.state(), state);
        assert_eq!(cursor.skipping, Some(progress));
        assert_eq!(cursor.current(), Some(Event::ArrayStart { offset: 0 }));
        assert_eq!(cursor.raw_bytes(), b"[");
    }
}
