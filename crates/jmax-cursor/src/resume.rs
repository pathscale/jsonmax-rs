//! Ownership transfer for suspended, completed, and partially consumed cursors.
//!
//! Suspension preserves the parser itself, including unfinished tokens and skips.
//! Complete output validates through EOF; partial query output retains all fed
//! bytes without interpreting an unconsumed suffix or issuing a schema proof.

use super::*;

/// Moves every part of a cursor into an owned continuation without advancing it.
///
/// This does not allocate, copy buffers, mark EOF, or perform background work.
/// Input chunks have already been copied by `walk::push`, so the continuation
/// does not borrow the caller's storage.
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
/// All fed input and all decoded arena bytes are copied with fallible reserves.
/// The validation flag reflects actual syntax completion, not EOF declaration,
/// a complete root event, or successful completion of a single subtree.
pub fn into_partial(cursor: Cursor) -> Result<CursorOutput> {
    if let Some(error) = cursor.syntax.failure() {
        return Err(error.clone());
    }
    let at = cursor.state().byte_offset;
    if cursor.mode != SkipMode::Query {
        return Err(cursor_error(
            ErrorCode::ConstraintViolation, at,
            "partial output requires query cursor mode",
        ));
    }
    if cursor.skipping.is_some() {
        return Err(cursor_error(
            ErrorCode::TransportIncomplete, at,
            "partial output requires completion of the pending subtree skip",
        ));
    }

    let mut input = Vec::new();
    let mut decoded = Vec::new();
    reserve_bytes(&mut input, cursor.raw_bytes().len(), at)?;
    reserve_bytes(&mut decoded, cursor.decoded_bytes().len(), at)?;
    input.extend_from_slice(cursor.raw_bytes());
    decoded.extend_from_slice(cursor.decoded_bytes());
    Ok(CursorOutput {
        payload: Payload::from_vec(input),
        decoded,
        fully_validated: cursor.syntax.is_finished(),
    })
}

fn reserve_bytes(storage: &mut Vec<u8>, additional: usize, at: u32) -> Result<()> {
    storage.try_reserve_exact(additional).map_err(|_| {
        cursor_error(ErrorCode::BudgetExhausted, at, "partial output allocation failed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn fed(bytes: &[u8], mode: SkipMode) -> Cursor {
        let mut cursor = Cursor::new(Limits::strict_default(), mode);
        walk::push(&mut cursor, bytes).unwrap();
        cursor
    }

    fn emit(cursor: &mut Cursor) -> Event {
        match walk::next(cursor).unwrap() {
            ScanStep::Emit(event) => event,
            step => panic!("expected an event, got {step:?}"),
        }
    }

    fn drain_chunk(cursor: &mut Cursor, events: &mut Vec<Event>) {
        for _ in 0..=cursor.raw_bytes().len() + 1 {
            match walk::next(cursor).unwrap() {
                ScanStep::Emit(event) => events.push(event),
                ScanStep::NeedMore => return,
                ScanStep::Finished => panic!("nonfinal input was marked finished"),
            }
        }
        panic!("chunk traversal did not terminate");
    }

    fn drain_final(cursor: &mut Cursor, events: &mut Vec<Event>) {
        walk::end_input(cursor).unwrap();
        for _ in 0..=cursor.raw_bytes().len() + 1 {
            match walk::next(cursor).unwrap() {
                ScanStep::Emit(event) => events.push(event),
                ScanStep::Finished => return,
                ScanStep::NeedMore => panic!("final input requested another chunk"),
            }
        }
        panic!("final traversal did not terminate");
    }

    fn resolve(output: &CursorOutput, reference: StrRef) -> Option<&str> {
        let bytes = match reference.source {
            SpanSource::RawPayload => output.payload.as_slice(),
            SpanSource::DecodedArena => &output.decoded,
        };
        core::str::from_utf8(reference.span.slice(bytes)?).ok()
    }

    fn reference(event: Event) -> StrRef {
        match event {
            Event::Key { str_ref } => str_ref,
            Event::Scalar { kind: ScalarKind::String, decoded: Some(value), .. } => value,
            _ => panic!("expected a string reference"),
        }
    }

    // Inspect all observable state, including buffer addresses, around the move.
    // Subsequent split-vector tests also exercise the private pending token and
    // frame contents through their observable continuation behavior.
    fn round_trip(cursor: Cursor) -> Cursor {
        let state = cursor.state();
        let current = cursor.current();
        let skipping = cursor.skipping;
        let mode = cursor.mode();
        let limits = cursor.syntax.limits();
        let final_input = cursor.syntax.is_final();
        let finished = cursor.syntax.is_finished();
        let failure = cursor.syntax.failure().cloned();
        let raw = cursor.raw_bytes().to_vec();
        let decoded = cursor.decoded_bytes().to_vec();
        let raw_pointer = cursor.raw_bytes().as_ptr();
        let decoded_pointer = cursor.decoded_bytes().as_ptr();
        let cursor = resume(suspend(cursor));
        assert_eq!(cursor.state().byte_offset, state.byte_offset);
        assert_eq!(cursor.state().depth, state.depth);
        assert_eq!(cursor.state().phase, state.phase);
        assert_eq!(cursor.state().finished, state.finished);
        assert_eq!(cursor.current(), current);
        assert_eq!(cursor.skipping, skipping);
        assert_eq!(cursor.mode(), mode);
        assert_eq!(cursor.syntax.limits(), limits);
        assert_eq!(cursor.syntax.is_final(), final_input);
        assert_eq!(cursor.syntax.is_finished(), finished);
        assert_eq!(cursor.syntax.failure(), failure.as_ref());
        assert_eq!(cursor.raw_bytes(), raw);
        assert_eq!(cursor.decoded_bytes(), decoded);
        assert_eq!(cursor.raw_bytes().as_ptr(), raw_pointer);
        assert_eq!(cursor.decoded_bytes().as_ptr(), decoded_pointer);
        cursor
    }

    fn syntax_failure(bytes: &[u8], limits: Limits) -> Error {
        let mut syntax = SyntaxState::new(limits);
        if let Err(error) = jmax_syntax::feed::push(&mut syntax, bytes) {
            return error;
        }
        jmax_syntax::finish::end_input(&mut syntax).unwrap();
        for _ in 0..=bytes.len() + 1 {
            match jmax_syntax::feed::next(&mut syntax) {
                Err(error) => return error,
                Ok(ScanStep::Emit(_)) => {}
                Ok(step) => panic!("invalid syntax unexpectedly returned {step:?}"),
            }
        }
        panic!("invalid syntax traversal did not terminate");
    }

    #[test]
    fn public_signatures_and_owned_continuations_match_the_contract() {
        let _: fn(Cursor) -> SuspendedCursor = suspend;
        let _: fn(SuspendedCursor) -> Cursor = resume;
        let _: fn(Cursor) -> Result<CursorOutput> = finish;
        let _: fn(Cursor) -> Result<CursorOutput> = into_partial;
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Cursor>();
        send_sync::<SuspendedCursor>();
        send_sync::<CursorOutput>();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let cursor = round_trip(Cursor::new(Limits::strict_default(), mode));
            assert_eq!(cursor.current(), None);
            assert_eq!(cursor.state().byte_offset, 0);
            assert!(cursor.raw_bytes().is_empty());
        }
    }

    #[test]
    fn every_split_preserves_exact_events_and_cooked_references() {
        let bytes = br#"{"a":[true,false,null,-12.5e+2,"x"],"b":"\u00e9"}"#;
        let raw = |offset, len| StrRef {
            span: Span32::new(offset, len), source: SpanSource::RawPayload, scalar_len: len,
        };
        let scalar = |kind, offset, len| Event::Scalar {
            kind, span: Span32::new(offset, len), decoded: None,
        };
        let cooked = StrRef {
            span: Span32::new(0, 2), source: SpanSource::DecodedArena, scalar_len: 1,
        };
        let expected = vec![
            Event::ObjectStart { offset: 0 },
            Event::Key { str_ref: raw(2, 1) },
            Event::ArrayStart { offset: 5 },
            scalar(ScalarKind::True, 6, 4),
            scalar(ScalarKind::False, 11, 5),
            scalar(ScalarKind::Null, 17, 4),
            scalar(ScalarKind::Number, 22, 8),
            Event::Scalar {
                kind: ScalarKind::String, span: Span32::new(31, 3), decoded: Some(raw(32, 1)),
            },
            Event::ArrayEnd { offset: 34 },
            Event::Key { str_ref: raw(37, 1) },
            Event::Scalar {
                kind: ScalarKind::String, span: Span32::new(40, 8), decoded: Some(cooked),
            },
            Event::ObjectEnd { offset: 48 },
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for split in 0..=bytes.len() {
                let mut cursor = fed(&bytes[..split], mode);
                let mut events = Vec::new();
                drain_chunk(&mut cursor, &mut events);
                cursor = round_trip(cursor);
                walk::push(&mut cursor, &bytes[split..]).unwrap();
                drain_final(&mut cursor, &mut events);
                assert_eq!(events, expected, "split {split}, mode {mode:?}");
                let output = finish(round_trip(cursor)).unwrap();
                assert_eq!(output.payload.as_slice(), bytes);
                assert_eq!(output.decoded, "é".as_bytes());
                assert_eq!(resolve(&output, cooked), Some("é"));
                assert_eq!(resolve(&output, raw(32, 1)), Some("x"));
                assert!(output.fully_validated);
            }
        }
    }

    #[test]
    fn finish_handles_every_split_and_pending_token_kind() {
        let vectors: &[&[u8]] = &[
            b"0", b"-0.25e+10", b"1e999999", b"true", b"false", b"null",
            br#""""#, br#""a\"b\\c\n\uD83D\uDE00""#,
            "\"é😀\"".as_bytes(), b"[]", b"{}",
            br#"{"\u0061":[{"b":"\t"},[false,null,12]]}"#,
            b" \r\n\t[1] \r\n\t",
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &bytes in vectors {
                let baseline = finish(fed(bytes, mode)).unwrap();
                for split in 0..=bytes.len() {
                    let mut cursor = fed(&bytes[..split], mode);
                    drain_chunk(&mut cursor, &mut Vec::new());
                    cursor = round_trip(cursor);
                    walk::push(&mut cursor, &bytes[split..]).unwrap();
                    let output = finish(cursor).unwrap();
                    assert_eq!(output.payload.as_slice(), bytes, "split {split}");
                    assert_eq!(output.decoded, baseline.decoded, "split {split}");
                    assert!(output.fully_validated);
                }
            }
        }
    }

    #[test]
    fn one_byte_chunks_can_be_overwritten_and_dropped_after_each_push() {
        let bytes = br#"["plain","a\n\uD83D\uDE00",-12.3e+4,true,null]"#;
        let mut cursor = Cursor::new(Limits::strict_default(), SkipMode::Query);
        let mut events = Vec::new();
        for &byte in bytes {
            let mut chunk = vec![byte];
            walk::push(&mut cursor, &chunk).unwrap();
            chunk[0] = 0xff;
            drop(chunk);
            drain_chunk(&mut cursor, &mut events);
            cursor = round_trip(cursor);
        }
        let output = finish(cursor).unwrap();
        assert_eq!(output.payload.as_slice(), bytes);
        assert_eq!(output.decoded, "a\n😀".as_bytes());
        let strings: Vec<_> = events.into_iter().filter_map(|event| match event {
            Event::Scalar { decoded: Some(value), .. } => Some(value),
            _ => None,
        }).collect();
        assert_eq!(strings.len(), 2);
        assert_eq!(resolve(&output, strings[0]), Some("plain"));
        assert_eq!(resolve(&output, strings[1]), Some("a\n😀"));
        assert_eq!(strings[1].scalar_len, 3);
    }

    #[test]
    fn finish_transfers_existing_buffers_after_successful_traversal() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = fed(br#"["\n","raw"]  "#, mode);
            let mut events = Vec::new();
            drain_final(&mut cursor, &mut events);
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
            let input_pointer = cursor.raw_bytes().as_ptr();
            let arena_pointer = cursor.decoded_bytes().as_ptr();
            let output = finish(round_trip(cursor)).unwrap();
            assert_eq!(output.payload.as_slice().as_ptr(), input_pointer);
            assert_eq!(output.decoded.as_ptr(), arena_pointer);
            assert_eq!(output.payload.as_slice(), br#"["\n","raw"]  "#);
            assert_eq!(output.decoded, b"\n");
            assert!(output.fully_validated);
        }
    }

    #[test]
    fn finish_resumes_skips_with_the_stored_mode_at_every_boundary() {
        let bytes = br#"[{"x":["\uD83D\uDE00",12]},true]  "#;
        for cursor_mode in [SkipMode::Strict, SkipMode::Query] {
            for skip_mode in [SkipMode::Strict, SkipMode::Query] {
                for split in 1..=bytes.len() {
                    let mut cursor = fed(&bytes[..split], cursor_mode);
                    assert_eq!(emit(&mut cursor), Event::ArrayStart { offset: 0 });
                    match skip::subtree(&mut cursor, skip_mode).unwrap() {
                        SkipStep::NeedMore => {
                            assert_eq!(cursor.skipping.unwrap().mode, skip_mode);
                        }
                        SkipStep::Complete { value, validated } => {
                            assert_eq!(value.span.slice(bytes), Some(&bytes[..bytes.len() - 2]));
                            assert_eq!(validated, skip_mode == SkipMode::Strict);
                            assert!(cursor.skipping.is_none());
                        }
                    }
                    cursor = round_trip(cursor);
                    walk::push(&mut cursor, &bytes[split..]).unwrap();
                    let output = finish(cursor).unwrap();
                    assert_eq!(output.payload.as_slice(), bytes);
                    assert_eq!(output.decoded, "😀".as_bytes());
                    assert!(output.fully_validated);
                }
            }
        }
    }

    #[test]
    fn resumed_skip_keeps_nesting_and_recovers_from_walk_misuse() {
        let mut cursor = fed(b"[[", SkipMode::Query);
        emit(&mut cursor);
        assert_eq!(skip::subtree(&mut cursor, SkipMode::Strict).unwrap(), SkipStep::NeedMore);
        assert_eq!(cursor.skipping.unwrap().depth, 2);
        let mut cursor = round_trip(cursor);
        let error = walk::next(&mut cursor).unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::InvalidSyntax, ErrorSource::Cursor, 2));
        assert!(cursor.syntax.failure().is_none());
        walk::push(&mut cursor, b"0],1] ").unwrap();
        let mismatch = skip::subtree(&mut cursor, SkipMode::Query).unwrap_err();
        assert_eq!(mismatch.code, ErrorCode::ConstraintViolation);
        assert_eq!(cursor.state().byte_offset, 2);
        let output = finish(round_trip(cursor)).unwrap();
        assert_eq!(output.payload.as_slice(), b"[[0],1] ");
        assert!(output.fully_validated);
    }

    #[test]
    fn active_child_skip_does_not_hide_invalid_parent_suffix() {
        for cursor_mode in [SkipMode::Strict, SkipMode::Query] {
            for skip_mode in [SkipMode::Strict, SkipMode::Query] {
                let mut cursor = fed(b"[[", cursor_mode);
                emit(&mut cursor);
                emit(&mut cursor);
                assert_eq!(skip::subtree(&mut cursor, skip_mode).unwrap(), SkipStep::NeedMore);
                walk::push(&mut cursor, b"],]").unwrap();
                let error = finish(round_trip(cursor)).unwrap_err();
                assert_eq!(error, syntax_failure(b"[[],]", Limits::strict_default()));
                assert_eq!((error.code, error.source, error.byte_offset),
                    (ErrorCode::InvalidSyntax, ErrorSource::Syntax, 4));
            }
        }
    }

    #[test]
    fn finish_reports_exact_eof_and_malformed_input_errors() {
        let vectors: &[(&[u8], ErrorCode, ErrorSource, u32)] = &[
            (b"", ErrorCode::UnexpectedEof, ErrorSource::Syntax, 0),
            (b" \t\r\n", ErrorCode::UnexpectedEof, ErrorSource::Syntax, 4),
            (b"[", ErrorCode::UnexpectedEof, ErrorSource::Syntax, 1),
            (b"{\"a\":", ErrorCode::UnexpectedEof, ErrorSource::Syntax, 5),
            (b"tru", ErrorCode::UnexpectedEof, ErrorSource::Lexer, 3),
            (b"1e+", ErrorCode::UnexpectedEof, ErrorSource::Lexer, 3),
            (b"\"abc", ErrorCode::UnclosedString, ErrorSource::Lexer, 0),
            (b"[1,]", ErrorCode::InvalidSyntax, ErrorSource::Syntax, 3),
            (b"null true", ErrorCode::TrailingContent, ErrorSource::Syntax, 5),
            (b"01", ErrorCode::InvalidNumber, ErrorSource::Lexer, 1),
            (b"truex", ErrorCode::InvalidToken, ErrorSource::Lexer, 4),
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &(bytes, code, source, offset) in vectors {
                let error = finish(round_trip(fed(bytes, mode))).unwrap_err();
                assert_eq!((error.code, error.source, error.byte_offset), (code, source, offset));
                assert_eq!(error, syntax_failure(bytes, Limits::strict_default()));
            }
        }
    }

    #[test]
    fn incomplete_skip_at_eof_preserves_the_syntax_error() {
        for bytes in [b"[".as_slice(), b"[[1", b"[\"abc"] {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let mut cursor = fed(bytes, mode);
                emit(&mut cursor);
                assert_eq!(skip::subtree(&mut cursor, mode).unwrap(), SkipStep::NeedMore);
                let error = finish(round_trip(cursor)).unwrap_err();
                assert_eq!(error, syntax_failure(bytes, Limits::strict_default()));
            }
        }
    }

    #[test]
    fn lexical_and_duplicate_errors_propagate_unchanged() {
        let vectors: &[&[u8]] = &[
            br#"["\q"]"#, br#"["\uD800"]"#, b"[\"\xff\"]", b"[\"a\nb\"]",
            br#"{"a":1,"\u0061":2}"#, b"[1e-]", b"{]",
        ];
        for &bytes in vectors {
            let expected = syntax_failure(bytes, Limits::strict_default());
            for mode in [SkipMode::Strict, SkipMode::Query] {
                assert_eq!(finish(round_trip(fed(bytes, mode))).unwrap_err(), expected);
            }
            let mut cursor = fed(bytes, SkipMode::Query);
            emit(&mut cursor);
            let error = skip::subtree(&mut cursor, SkipMode::Query).unwrap_err();
            assert_eq!(error, expected);
            assert!(cursor.skipping.is_some());
            assert_eq!(into_partial(round_trip(cursor)).unwrap_err(), expected);
        }
    }

    #[test]
    fn sticky_failure_survives_eof_suspension_and_output_attempts() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for partial in [false, true] {
                let mut cursor = fed(b"null", mode);
                walk::end_input(&mut cursor).unwrap();
                let error = walk::push(&mut cursor, b"").unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidSyntax);
                let mut cursor = round_trip(cursor);
                assert_eq!(walk::end_input(&mut cursor).unwrap_err(), error);
                assert_eq!(walk::next(&mut cursor).unwrap_err(), error);
                let result = if partial { into_partial(cursor) } else { finish(cursor) };
                assert_eq!(result.unwrap_err(), error);
            }
        }
    }

    #[test]
    fn partial_rejects_strict_cursors_before_and_after_completion() {
        for completed in [false, true] {
            let mut cursor = fed(b"[]", SkipMode::Strict);
            if completed {
                drain_final(&mut cursor, &mut Vec::new());
            }
            let at = cursor.state().byte_offset;
            let error = into_partial(round_trip(cursor)).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset),
                (ErrorCode::ConstraintViolation, ErrorSource::Cursor, at));
        }
    }

    #[test]
    fn partial_rejects_pending_skips_even_after_the_missing_bytes_arrive() {
        for append in [false, true] {
            for eof in [false, true] {
                let mut cursor = fed(b"[", SkipMode::Query);
                emit(&mut cursor);
                assert_eq!(skip::subtree(&mut cursor, SkipMode::Strict).unwrap(), SkipStep::NeedMore);
                if append {
                    walk::push(&mut cursor, b"]").unwrap();
                }
                if eof {
                    walk::end_input(&mut cursor).unwrap();
                }
                let error = into_partial(round_trip(cursor)).unwrap_err();
                assert_eq!((error.code, error.source, error.byte_offset),
                    (ErrorCode::TransportIncomplete, ErrorSource::Cursor, 1));
            }
        }
    }

    #[test]
    fn partial_never_parses_or_requires_a_complete_document() {
        let vectors: &[&[u8]] = &[b"", b" \r\n", b"[", b"tru", b"1e+", b"\"abc", b"\xff"];
        for &bytes in vectors {
            for eof in [false, true] {
                let mut cursor = fed(bytes, SkipMode::Query);
                if eof {
                    walk::end_input(&mut cursor).unwrap();
                }
                let output = into_partial(round_trip(cursor)).unwrap();
                assert_eq!(output.payload.as_slice(), bytes);
                assert!(output.decoded.is_empty());
                assert!(!output.fully_validated);
            }
        }
        for bytes in [b"tru".as_slice(), b"1e+", b"\"abc"] {
            let mut cursor = fed(bytes, SkipMode::Query);
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::NeedMore);
            let output = into_partial(round_trip(cursor)).unwrap();
            assert_eq!(output.payload.as_slice(), bytes);
            assert!(!output.fully_validated);
        }
    }

    #[test]
    fn partial_copies_all_owners_and_retains_unconsumed_invalid_suffix() {
        let bytes = br#"["plain","\n\u00e9", invalid]"#;
        let mut cursor = fed(bytes, SkipMode::Query);
        emit(&mut cursor);
        let raw = reference(emit(&mut cursor));
        let cooked = reference(emit(&mut cursor));
        let input_pointer = cursor.raw_bytes().as_ptr();
        let arena_pointer = cursor.decoded_bytes().as_ptr();
        let output = into_partial(round_trip(cursor)).unwrap();
        assert_eq!(output.payload.as_slice(), bytes);
        assert_eq!(output.decoded, "\né".as_bytes());
        assert_ne!(output.payload.as_slice().as_ptr(), input_pointer);
        assert_ne!(output.decoded.as_ptr(), arena_pointer);
        assert_eq!(resolve(&output, raw), Some("plain"));
        assert_eq!(resolve(&output, cooked), Some("\né"));
        assert!(!output.fully_validated);
        let cloned = output.clone();
        drop(output);
        assert_eq!(resolve(&cloned, cooked), Some("\né"));
        assert_eq!(cloned.payload.as_slice(), bytes);
        assert!(!cloned.fully_validated);
        assert_eq!(finish(fed(bytes, SkipMode::Query)).unwrap_err().code, ErrorCode::InvalidToken);
    }

    #[test]
    fn partial_validation_flag_requires_the_finished_step() {
        for phase in 0..5 {
            let mut cursor = fed(b"[] \t", SkipMode::Query);
            emit(&mut cursor); // RootDone with an open frame is not completion.
            if phase >= 1 {
                emit(&mut cursor); // Closing the root is not EOF validation.
            }
            if phase >= 2 {
                assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::NeedMore);
            }
            if phase >= 3 {
                walk::end_input(&mut cursor).unwrap();
            }
            if phase >= 4 {
                assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
            }
            let output = into_partial(round_trip(cursor)).unwrap();
            assert_eq!(output.fully_validated, phase == 4);
            assert_eq!(output.payload.as_slice(), b"[] \t");
        }
        let mut cursor = fed(b"[] trailing", SkipMode::Query);
        emit(&mut cursor);
        assert!(matches!(skip::subtree(&mut cursor, SkipMode::Strict).unwrap(),
            SkipStep::Complete { validated: true, .. }));
        walk::end_input(&mut cursor).unwrap();
        let output = into_partial(cursor).unwrap();
        assert!(!output.fully_validated);
        assert_eq!(output.payload.as_slice(), b"[] trailing");
    }

    #[test]
    fn suspension_retains_inclusive_limits_and_duplicate_key_history() {
        let limits = Limits {
            max_payload_bytes: 64, max_decoded_bytes: 1, max_depth: 1,
            max_fields_per_object: 1, max_string_bytes: 2, max_number_digits: 2,
            max_array_items: 1, ..Limits::strict_default()
        };
        let valid: &[&[u8]] = &[b"[12]", br#"{"a":0}"#, br#""\n""#, br#""ab""#];
        for &bytes in valid {
            let mut cursor = Cursor::new(limits, SkipMode::Query);
            walk::push(&mut cursor, bytes).unwrap();
            assert!(finish(round_trip(cursor)).unwrap().fully_validated);
        }
        let invalid: &[(&[u8], ErrorCode)] = &[
            (b"[[]]", ErrorCode::DepthLimitExceeded),
            (b"[1,2]", ErrorCode::ConstraintViolation),
            (b"123", ErrorCode::ConstraintViolation),
            (br#""abc""#, ErrorCode::ConstraintViolation),
            (br#"{"a":0,"b":0}"#, ErrorCode::ConstraintViolation),
        ];
        for &(bytes, code) in invalid {
            let mut cursor = Cursor::new(limits, SkipMode::Query);
            walk::push(&mut cursor, bytes).unwrap();
            let error = finish(round_trip(cursor)).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error, syntax_failure(bytes, limits));
        }
        let arena_limits = Limits { max_decoded_bytes: 1, ..Limits::strict_default() };
        let mut cursor = Cursor::new(arena_limits, SkipMode::Query);
        walk::push(&mut cursor, br#"["\n","\t"]"#).unwrap();
        emit(&mut cursor);
        emit(&mut cursor);
        let error = finish(round_trip(cursor)).unwrap_err();
        assert_eq!(error.code, ErrorCode::DecodedArenaOverflow);
        assert_eq!(error, syntax_failure(br#"["\n","\t"]"#, arena_limits));

        let mut cursor = fed(br#"{"a":1,"#, SkipMode::Query);
        drain_chunk(&mut cursor, &mut Vec::new());
        let mut cursor = round_trip(cursor);
        walk::push(&mut cursor, br#""\u0061":2}"#).unwrap();
        let error = finish(cursor).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
        assert_eq!(error, syntax_failure(br#"{"a":1,"\u0061":2}"#, Limits::strict_default()));
    }

    #[test]
    fn zero_depth_and_exact_payload_limit_survive_suspension() {
        let limits = Limits { max_depth: 0, max_payload_bytes: 4, ..Limits::strict_default() };
        let mut cursor = Cursor::new(limits, SkipMode::Strict);
        walk::push(&mut cursor, b"null").unwrap();
        assert!(finish(round_trip(cursor)).unwrap().fully_validated);
        let mut cursor = Cursor::new(limits, SkipMode::Query);
        walk::push(&mut cursor, b"[]").unwrap();
        assert_eq!(finish(round_trip(cursor)).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        let mut cursor = Cursor::new(limits, SkipMode::Query);
        walk::push(&mut cursor, b"null").unwrap();
        let mut cursor = round_trip(cursor);
        let error = walk::push(&mut cursor, b" ").unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::PayloadTooLarge, ErrorSource::Syntax, 4));
        assert_eq!(cursor.raw_bytes(), b"null");
        assert_eq!(into_partial(cursor).unwrap_err(), error);
    }

    #[test]
    fn allocation_capacity_overflow_is_a_deterministic_cursor_error() {
        for at in [0, 17, u32::MAX] {
            let mut storage = vec![1, 2, 3];
            let capacity = storage.capacity();
            let error = reserve_bytes(&mut storage, usize::MAX, at).unwrap_err();
            assert_eq!(error, cursor_error(
                ErrorCode::BudgetExhausted, at, "partial output allocation failed",
            ));
            assert_eq!(storage, [1, 2, 3]);
            assert_eq!(storage.capacity(), capacity);
            reserve_bytes(&mut storage, 0, at).unwrap();
        }
    }

    #[test]
    fn selection_spans_agree_with_resumed_owned_output() {
        let bytes = br#"{"a":["\n",null],"b":"raw"}"#;
        let plan = SelectorPlan::new(vec![
            SelectorNode {
                edges: vec![SelectorEdge { segment: Segment::Key(String::from("a")), target: 1 }],
                slots: vec![0],
            },
            SelectorNode {
                edges: vec![SelectorEdge { segment: Segment::Index(0), target: 2 }],
                slots: vec![1],
            },
            SelectorNode { edges: vec![], slots: vec![2] },
        ], 3).unwrap();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let selected = select::extract(
                Payload::from_slice(bytes), &plan, mode, Limits::strict_default(),
            ).unwrap();
            assert_eq!(selected.raw(0), Some(bytes.as_slice()));
            assert_eq!(selected.raw(1), Some(br#"["\n",null]"#.as_slice()));
            assert_eq!(selected.raw(2), Some(br#""\n""#.as_slice()));
            assert_eq!(selected.string(2), Some("\n"));
            assert_eq!(selected.fully_validated(), mode == SkipMode::Strict);
            let output = finish(round_trip(fed(bytes, mode))).unwrap();
            for (slot, value) in selected.slots().iter().enumerate() {
                let value = value.unwrap();
                assert_eq!(value.span.slice(output.payload.as_slice()), selected.raw(slot as u32));
                if let Some(reference) = value.decoded {
                    assert_eq!(resolve(&output, reference), selected.string(slot as u32));
                }
            }
            drop(selected);
            assert_eq!(output.payload.as_slice(), bytes);
            assert_eq!(output.decoded, b"\n");
            assert!(output.fully_validated);
        }
    }
}
