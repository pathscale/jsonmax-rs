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
    Ok(SyntaxOutput {
        payload: Payload::from_vec(state.input),
        decoded: state.decoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffered(bytes: &[u8], limits: Limits) -> SyntaxState {
        let mut state = SyntaxState::new(limits);
        feed::push(&mut state, bytes).unwrap();
        state
    }

    fn drain(state: &mut SyntaxState, events: &mut Vec<Event>) -> Result<ScanStep> {
        loop {
            match feed::next(state)? {
                ScanStep::Emit(event) => events.push(event),
                step => return Ok(step),
            }
        }
    }

    fn assert_complete(state: &SyntaxState, input: &[u8]) {
        assert!(state.is_final());
        assert!(state.is_finished());
        assert!(state.failure().is_none());
        assert_eq!(state.raw_bytes(), input);
        assert_eq!(state.byte_offset as usize, input.len());
        assert_eq!(state.root_phase, FramePhase::RootDone);
        assert!(state.frames.is_empty());
        assert!(state.pending.is_none());
        assert_eq!(state.state().depth, 0);
        assert_eq!(state.state().phase, Some(FramePhase::RootDone));
    }

    fn assert_error(error: &Error, code: ErrorCode, source: ErrorSource, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn reject(bytes: &[u8], limits: Limits, code: ErrorCode, source: ErrorSource, offset: u32) {
        let mut state = buffered(bytes, limits);
        let error = finish(&mut state).unwrap_err();
        assert_error(&error, code, source, offset);
        assert!(state.is_final());
        assert!(!state.is_finished());
        assert_eq!(state.failure(), Some(&error));
        let cursor = state.byte_offset;
        let arena = state.decoded.clone();
        assert_eq!(end_input(&mut state), Err(error.clone()));
        assert_eq!(finish(&mut state), Err(error.clone()));
        assert_eq!(feed::next(&mut state), Err(error.clone()));
        assert_eq!(feed::push(&mut state, b"null"), Err(error.clone()));
        assert_eq!(state.raw_bytes(), bytes);
        assert_eq!(state.decoded, arena);
        assert_eq!(state.byte_offset, cursor);
        assert_eq!(into_output(state).unwrap_err(), error);
    }

    fn resolve(output: &SyntaxOutput, reference: StrRef) -> &str {
        let owner = match reference.source {
            SpanSource::RawPayload => output.payload.as_slice(),
            SpanSource::DecodedArena => &output.decoded,
        };
        core::str::from_utf8(reference.span.slice(owner).unwrap()).unwrap()
    }

    #[test]
    fn ending_empty_or_invalid_input_only_marks_eof() {
        for bytes in [b"".as_slice(), b"?", b"[", b"\"\\q\""] {
            let mut state = buffered(bytes, Limits::strict_default());
            let pointer = state.input.as_ptr();
            for _ in 0..3 {
                end_input(&mut state).unwrap();
                assert!(state.is_final());
                assert!(!state.is_finished());
                assert!(state.failure().is_none());
                assert_eq!(state.raw_bytes(), bytes);
                assert_eq!(state.input.as_ptr(), pointer);
                assert_eq!(state.byte_offset, 0);
                assert_eq!(state.root_phase, FramePhase::RootStart);
                assert!(state.frames.is_empty());
                assert!(state.pending.is_none());
                assert!(state.decoded.is_empty());
            }
            assert!(finish(&mut state).is_err());
        }
    }

    #[test]
    fn ending_preserves_pending_tokens_and_open_frame_state() {
        let mut state = buffered(br#"["\u0061","\uD83D"#, Limits::strict_default());
        let mut events = Vec::new();
        assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
        assert_eq!(events.len(), 2);
        assert_eq!(state.decoded_bytes(), b"a");
        let pending = state.pending;
        let cursor = state.byte_offset;
        let phase = state.frames[0].phase();
        let count = state.frames[0].count();
        end_input(&mut state).unwrap();
        end_input(&mut state).unwrap();
        assert_eq!(state.pending, pending);
        assert_eq!(state.byte_offset, cursor);
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.frames[0].phase(), phase);
        assert_eq!(state.frames[0].count(), count);
        assert_eq!(state.decoded_bytes(), b"a");
        let error = finish(&mut state).unwrap_err();
        assert_error(&error, ErrorCode::UnexpectedEof, ErrorSource::Lexer, state.input.len() as u32);
        assert_eq!(state.decoded_bytes(), b"a");
    }

    #[test]
    fn end_input_preserves_every_event_with_exact_spans_and_offsets() {
        let document = b"{\"a\":[0,true,\"\\n\"]} \r\n\t";
        let raw_key = StrRef {
            span: Span32::new(2, 1), source: SpanSource::RawPayload, scalar_len: 1,
        };
        let cooked = StrRef {
            span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 1,
        };
        let expected = [
            Event::ObjectStart { offset: 0 },
            Event::Key { str_ref: raw_key },
            Event::ArrayStart { offset: 5 },
            Event::Scalar { kind: ScalarKind::Number, span: Span32::new(6, 1), decoded: None },
            Event::Scalar { kind: ScalarKind::True, span: Span32::new(8, 4), decoded: None },
            Event::Scalar { kind: ScalarKind::String, span: Span32::new(13, 4), decoded: Some(cooked) },
            Event::ArrayEnd { offset: 17 },
            Event::ObjectEnd { offset: 18 },
        ];
        let mut state = buffered(document, Limits::strict_default());
        end_input(&mut state).unwrap();
        for event in expected {
            end_input(&mut state).unwrap();
            assert_eq!(feed::next(&mut state), Ok(ScanStep::Emit(event)));
            assert!(!state.is_finished());
        }
        assert_eq!(feed::next(&mut state), Ok(ScanStep::Finished));
        assert_complete(&state, document);
        let output = into_output(state).unwrap();
        assert_eq!(resolve(&output, raw_key), "a");
        assert_eq!(resolve(&output, cooked), "\n");
        assert_eq!(output.payload.as_slice(), document);
    }

    #[test]
    fn final_atoms_wait_for_eof_then_emit_once() {
        for (document, kind) in [
            ("0", ScalarKind::Number), ("-0", ScalarKind::Number),
            ("123.45E-67", ScalarKind::Number), ("1e999999", ScalarKind::Number),
            ("true", ScalarKind::True), ("false", ScalarKind::False),
            ("null", ScalarKind::Null),
        ] {
            let mut state = buffered(document.as_bytes(), Limits::strict_default());
            assert_eq!(feed::next(&mut state), Ok(ScanStep::NeedMore));
            assert_eq!(state.byte_offset, 0);
            assert!(state.pending.is_some());
            end_input(&mut state).unwrap();
            assert_eq!(feed::next(&mut state), Ok(ScanStep::Emit(Event::Scalar {
                kind, span: Span32::new(0, document.len() as u32), decoded: None,
            })));
            finish(&mut state).unwrap();
            assert_complete(&state, document.as_bytes());
            assert_eq!(feed::next(&mut state), Ok(ScanStep::Finished));
        }
    }

    #[test]
    fn finish_drains_unread_events_and_is_idempotent() {
        let document = br#" {"a":[null,{"b":"\u0063"}],"d":false} "#;
        for consumed in 0..=3 {
            let mut state = buffered(document, Limits::strict_default());
            for _ in 0..consumed {
                assert!(matches!(feed::next(&mut state), Ok(ScanStep::Emit(_))));
            }
            finish(&mut state).unwrap();
            assert_complete(&state, document);
            assert_eq!(state.decoded_bytes(), b"c");
            let pointers = (state.input.as_ptr(), state.decoded.as_ptr());
            for _ in 0..3 {
                end_input(&mut state).unwrap();
                finish(&mut state).unwrap();
                assert_eq!(feed::next(&mut state), Ok(ScanStep::Finished));
                assert_complete(&state, document);
                assert_eq!(state.decoded_bytes(), b"c");
                assert_eq!((state.input.as_ptr(), state.decoded.as_ptr()), pointers);
            }
        }
    }

    #[test]
    fn complete_root_without_eof_still_needs_more() {
        for document in [b"[] \r\n\t".as_slice(), b"{}", b"\"x\"", b"null "] {
            let mut state = buffered(document, Limits::strict_default());
            assert_eq!(drain(&mut state, &mut Vec::new()), Ok(ScanStep::NeedMore));
            assert!(!state.is_final());
            assert!(!state.is_finished());
            assert_eq!(state.root_phase, FramePhase::RootDone);
            finish(&mut state).unwrap();
            assert_complete(&state, document);
        }
    }

    #[test]
    fn valid_documents_match_at_every_split_and_single_byte_chunks() {
        let documents: &[&[u8]] = &[
            b"null", b"true", b"false", b"0", b"-123.45e+67", b"[]", b"{}",
            b"\"\"", b" \r\n\t[0,true,false,null,{},[]] \t\r\n",
            br#"{"a":["raw","\n","\u0061","\uD83D\uDE00"]}"#,
            "[\"é😀\",\"é\\t😀\"]".as_bytes(),
            br#""\"\\\/\b\f\n\r\t\u0000""#,
        ];
        for &document in documents {
            let mut baseline = buffered(document, Limits::strict_default());
            end_input(&mut baseline).unwrap();
            let mut expected = Vec::new();
            assert_eq!(drain(&mut baseline, &mut expected), Ok(ScanStep::Finished));
            let expected_arena = baseline.decoded.clone();
            for split in 0..=document.len() {
                let mut state = buffered(&document[..split], Limits::strict_default());
                let mut events = Vec::new();
                assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
                feed::push(&mut state, b"").unwrap();
                feed::push(&mut state, &document[split..]).unwrap();
                end_input(&mut state).unwrap();
                assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::Finished));
                assert_eq!(events, expected, "split {split} in {document:?}");
                assert_eq!(state.decoded, expected_arena);
                assert_complete(&state, document);
                let output = into_output(state).unwrap();
                assert_eq!(output.payload.as_slice(), document);
                assert_eq!(output.decoded, expected_arena);
            }
            let mut state = SyntaxState::new(Limits::strict_default());
            let mut events = Vec::new();
            for byte in document {
                feed::push(&mut state, core::slice::from_ref(byte)).unwrap();
                assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
            }
            end_input(&mut state).unwrap();
            assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::Finished));
            assert_eq!(events, expected);
            assert_eq!(state.decoded, expected_arena);
            assert_complete(&state, document);
        }
    }

    #[test]
    fn eof_reports_every_unfinished_container_phase_at_input_end() {
        for document in [
            "", " \r\n\t", "[", "[0", "[0,", "{", "{\"a\"", "{\"a\":",
            "{\"a\":0", "{\"a\":0,", "{\"a\":[{}", "[{\"a\":null}",
        ] {
            reject(document.as_bytes(), Limits::strict_default(), ErrorCode::UnexpectedEof,
                ErrorSource::Syntax, document.len() as u32);
        }
        let mut state = buffered(b"[", Limits::strict_default());
        assert_eq!(feed::next(&mut state), Ok(ScanStep::Emit(Event::ArrayStart { offset: 0 })));
        assert_eq!(state.root_phase, FramePhase::RootDone);
        assert_eq!(state.frames.len(), 1);
        assert_error(&finish(&mut state).unwrap_err(), ErrorCode::UnexpectedEof, ErrorSource::Syntax, 1);
    }

    #[test]
    fn truncated_atoms_and_escapes_report_lexical_eof() {
        for literal in [b"true".as_slice(), b"false", b"null"] {
            for length in 1..literal.len() {
                reject(&literal[..length], Limits::strict_default(), ErrorCode::UnexpectedEof,
                    ErrorSource::Lexer, length as u32);
            }
        }
        for document in ["-", "1.", "1e", "1e+", "1e-", "\"\\", r#""\u"#,
            r#""\u123"#, r#""\uD800"#, r#""\uD800\"#, r#""\uD800\uDC"#]
        {
            reject(document.as_bytes(), Limits::strict_default(), ErrorCode::UnexpectedEof,
                ErrorSource::Lexer, document.len() as u32);
        }
    }

    #[test]
    fn unclosed_strings_report_the_opening_quote_including_nested_strings() {
        for (document, offset) in [
            ("\"", 0), ("\"plain", 0), ("\"é😀", 0), (r#""\u0061"#, 0),
            (" \n[\"abc", 3), ("{\"key\":\"value", 7),
        ] {
            reject(document.as_bytes(), Limits::strict_default(), ErrorCode::UnclosedString,
                ErrorSource::Lexer, offset);
        }
    }

    #[test]
    fn determinable_lexical_failures_take_precedence_over_eof() {
        let vectors: &[(&[u8], ErrorCode, ErrorSource, u32)] = &[
            (br#""\q"#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 2),
            (br#""\u12x"#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 5),
            (br#""\uDC00"#, ErrorCode::LoneSurrogate, ErrorSource::Lexer, 1),
            (b"\"bad\n", ErrorCode::ControlCharInString, ErrorSource::Lexer, 4),
            (b"\"\xff", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 1),
            (b"\"\xe2\x82", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 1),
            (b"01e", ErrorCode::InvalidNumber, ErrorSource::Lexer, 1),
            (b"trux", ErrorCode::InvalidToken, ErrorSource::Lexer, 3),
            (b"1e]", ErrorCode::InvalidNumber, ErrorSource::Lexer, 2),
            (b"tru ", ErrorCode::InvalidToken, ErrorSource::Lexer, 3),
            (br#""\u12""#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 5),
        ];
        for &(document, code, source, offset) in vectors {
            reject(document, Limits::strict_default(), code, source, offset);
        }
    }

    #[test]
    fn trailing_content_is_reported_before_lexing_the_suffix() {
        let roots: &[&[u8]] = &[b"[]", b"{}", b"\"x\"", b"0", b"true", b"false", b"null"];
        let suffixes: &[&[u8]] = &[
            b"0", b"null", b"[]", b"{}", b"\"", b"?", b"//comment", b"/*x*/",
            b",", b":", b"]", b"}", b"\x00", b"\x0b", b"\x0c", b"\xff", b"\xef\xbb\xbf",
        ];
        for &root in roots {
            for &suffix in suffixes {
                let mut document = root.to_vec();
                document.extend_from_slice(b" \t\r\n");
                let offset = document.len() as u32;
                document.extend_from_slice(suffix);
                reject(&document, Limits::strict_default(), ErrorCode::TrailingContent,
                    ErrorSource::Syntax, offset);
            }
        }
        for document in [b"[]?".as_slice(), b"{}?", b"\"\"?"] {
            reject(document, Limits::strict_default(), ErrorCode::TrailingContent, ErrorSource::Syntax, 2);
        }
        for (document, code, offset) in [
            ("truefalse", ErrorCode::InvalidToken, 4),
            ("nullx", ErrorCode::InvalidToken, 4),
            ("1true", ErrorCode::InvalidNumber, 1),
        ] {
            reject(document.as_bytes(), Limits::strict_default(), code, ErrorSource::Lexer, offset);
        }
    }

    #[test]
    fn finish_rejects_trailing_commas_mismatched_delimiters_and_missing_separators() {
        for (document, offset) in [
            ("[0,]", 3), ("{\"a\":0,}", 7), ("[}", 1), ("{]", 1),
            ("[0 1]", 3), ("{\"a\" 0}", 5), ("{\"a\":}", 5),
        ] {
            reject(document.as_bytes(), Limits::strict_default(), ErrorCode::InvalidSyntax,
                ErrorSource::Syntax, offset);
        }
    }

    #[test]
    fn failures_are_identical_at_every_chunk_split() {
        let documents: &[&[u8]] = &[
            b"", b" \n", b"[", b"[0,", b"{\"x\":", b"tru", b"1e+", b"\"abc",
            br#""\uD800\uDC"#, br#""\q""#, b"\"\xed\xa0\x80\"", b"01", b"truex",
            b"[0,]", b"{} \xff", br#"{"a":0,"\u0061":1}"#,
        ];
        for &document in documents {
            let expected = into_output(buffered(document, Limits::strict_default())).unwrap_err();
            for split in 0..=document.len() {
                let mut state = buffered(&document[..split], Limits::strict_default());
                let actual = match drain(&mut state, &mut Vec::new()) {
                    Err(error) => error,
                    Ok(ScanStep::NeedMore) => {
                        feed::push(&mut state, &document[split..]).unwrap();
                        finish(&mut state).unwrap_err()
                    }
                    step => panic!("unexpected pre-EOF result: {step:?}"),
                };
                assert_eq!(actual, expected, "split {split} in {document:?}");
                assert_eq!(end_input(&mut state), Err(expected.clone()));
                assert_eq!(finish(&mut state), Err(expected.clone()));
                assert_eq!(into_output(state).unwrap_err(), expected);
            }
        }
    }

    #[test]
    fn finalization_honors_inclusive_limits_and_zero_boundaries() {
        let mut limits = Limits::strict_default();
        limits.max_depth = 0;
        assert!(into_output(buffered(b"null", limits)).is_ok());
        reject(b"[]", limits, ErrorCode::DepthLimitExceeded, ErrorSource::Syntax, 0);
        limits.max_depth = 1;
        assert!(into_output(buffered(b"[]", limits)).is_ok());
        reject(b"[[]]", limits, ErrorCode::DepthLimitExceeded, ErrorSource::Syntax, 1);
        limits.max_array_items = 0;
        assert!(into_output(buffered(b"[]", limits)).is_ok());
        reject(b"[0]", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        limits.max_array_items = 1;
        assert!(into_output(buffered(b"[0]", limits)).is_ok());
        reject(b"[0,1]", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 3);
        limits.max_fields_per_object = 0;
        assert!(into_output(buffered(b"{}", limits)).is_ok());
        reject(b"{\"a\":0}", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        limits.max_fields_per_object = 1;
        assert!(into_output(buffered(b"{\"a\":0}", limits)).is_ok());
        reject(b"{\"a\":0,\"b\":1}", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 7);
        limits.max_number_digits = 0;
        reject(b"0", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 0);
        limits.max_number_digits = 4;
        assert!(into_output(buffered(b"-1.2e+34", limits)).is_ok());
        limits.max_number_digits = 3;
        reject(b"-1.2e+34", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 7);
        limits.max_string_bytes = 0;
        assert!(into_output(buffered(b"\"\"", limits)).is_ok());
        reject(b"\"a\"", limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        limits.max_string_bytes = 6;
        limits.max_decoded_bytes = 1;
        assert_eq!(into_output(buffered(br#""\u0061""#, limits)).unwrap().decoded, b"a");
        limits.max_string_bytes = 5;
        reject(br#""\u0061""#, limits, ErrorCode::ConstraintViolation, ErrorSource::Syntax, 6);
        limits.max_string_bytes = 6;
        limits.max_decoded_bytes = 0;
        assert!(into_output(buffered(b"\"raw\"", limits)).is_ok());
        reject(br#""\u0061""#, limits, ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 1);
    }

    #[test]
    fn decoded_arena_limit_and_duplicate_key_policy_survive_draining() {
        let mut limits = Limits::strict_default();
        limits.max_decoded_bytes = 2;
        let document = br#"["\n","\t"]"#;
        assert_eq!(into_output(buffered(document, limits)).unwrap().decoded, b"\n\t");
        limits.max_decoded_bytes = 1;
        reject(document, limits, ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 7);
        let duplicate = br#"{"a":0,"\u0061":1}"#;
        reject(duplicate, Limits::strict_default(), ErrorCode::DuplicateObjectKey, ErrorSource::Syntax, 7);
        let output = into_output(buffered(duplicate, Limits::permissive_default())).unwrap();
        assert_eq!(output.payload.as_slice(), duplicate);
        assert_eq!(output.decoded, b"a");
        assert!(into_output(buffered(br#"{"unknown":0}"#, Limits::strict_default())).is_ok());
    }

    #[test]
    fn payload_growth_failure_is_returned_before_marking_eof() {
        let mut limits = Limits::strict_default();
        limits.max_payload_bytes = 4;
        let mut state = buffered(b"null", limits);
        let error = feed::push(&mut state, b" ").unwrap_err();
        assert_error(&error, ErrorCode::PayloadTooLarge, ErrorSource::Syntax, 4);
        assert_eq!(end_input(&mut state), Err(error.clone()));
        assert_eq!(finish(&mut state), Err(error.clone()));
        assert!(!state.is_final());
        assert_eq!(state.raw_bytes(), b"null");
        assert_eq!(into_output(state).unwrap_err(), error);
        assert!(into_output(buffered(b"null", limits)).is_ok());
        limits.max_payload_bytes = 0;
        reject(b"", limits, ErrorCode::UnexpectedEof, ErrorSource::Syntax, 0);
    }

    #[test]
    fn prior_failures_preserve_all_metadata_and_take_precedence_over_flags() {
        for final_input in [false, true] {
            for finished in [false, true] {
                let mut state = buffered(b"null", Limits::strict_default());
                let error = Error::new(ErrorCode::BudgetExhausted, ErrorSource::Cursor,
                    u32::MAX, "original failure").with_schema(21, 34);
                state.fail(error.clone());
                state.final_input = final_input;
                state.finished = finished;
                assert_eq!(end_input(&mut state), Err(error.clone()));
                assert_eq!(finish(&mut state), Err(error.clone()));
                assert_eq!(state.is_final(), final_input);
                assert_eq!(state.is_finished(), finished);
                assert_eq!(state.byte_offset, 0);
                assert_eq!(state.raw_bytes(), b"null");
                assert_eq!(into_output(state).unwrap_err(), error);
            }
        }
    }

    #[test]
    fn pushes_after_eof_or_finish_poison_subsequent_finalization() {
        for fully_finished in [false, true] {
            for chunk in [b"".as_slice(), b" ", b"null"] {
                let mut state = buffered(b"null", Limits::strict_default());
                if fully_finished {
                    finish(&mut state).unwrap();
                } else {
                    end_input(&mut state).unwrap();
                }
                let error = feed::push(&mut state, chunk).unwrap_err();
                assert_error(&error, ErrorCode::InvalidSyntax, ErrorSource::Syntax, 4);
                assert_eq!(state.raw_bytes(), b"null");
                assert_eq!(state.is_finished(), fully_finished);
                assert_eq!(finish(&mut state), Err(error.clone()));
                assert_eq!(end_input(&mut state), Err(error.clone()));
                assert_eq!(into_output(state).unwrap_err(), error);
            }
        }
    }

    #[test]
    fn into_output_preserves_emitted_references_while_finishing_unread_strings() {
        let mut state = buffered(br#"["raw","\u0061""#, Limits::strict_default());
        let mut events = Vec::new();
        assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
        let references: Vec<StrRef> = events.iter().filter_map(|event| match event {
            Event::Scalar { decoded: Some(reference), .. } => Some(*reference),
            _ => None,
        }).collect();
        assert_eq!(references.len(), 2);
        assert_eq!(references[0].source, SpanSource::RawPayload);
        assert_eq!(references[1].source, SpanSource::DecodedArena);
        let mut expected_payload = state.raw_bytes().to_vec();
        let suffix = br#", "\uD83D\uDE00", "\n"] "#;
        expected_payload.extend_from_slice(suffix);
        feed::push(&mut state, suffix).unwrap();
        let output = into_output(state).unwrap();
        assert_eq!(output.payload.as_slice(), expected_payload);
        assert_eq!(output.decoded, "a😀\n".as_bytes());
        assert_eq!(resolve(&output, references[0]), "raw");
        assert_eq!(resolve(&output, references[1]), "a");
        assert_eq!(references[0].scalar_len, 3);
        assert_eq!(references[1].scalar_len, 1);
    }

    #[test]
    fn into_output_moves_finalized_storage_without_redecoding() {
        let document = br#" ["\u0000","\uD83D\uDE00","raw"] "#;
        let mut state = buffered(document, Limits::strict_default());
        finish(&mut state).unwrap();
        let raw_pointer = state.input.as_ptr();
        let decoded_pointer = state.decoded.as_ptr();
        let decoded_capacity = state.decoded.capacity();
        let output = into_output(state).unwrap();
        assert_eq!(output.payload.as_slice(), document);
        assert_eq!(output.payload.as_slice().as_ptr(), raw_pointer);
        assert_eq!(output.decoded, "\0😀".as_bytes());
        assert_eq!(output.decoded.as_ptr(), decoded_pointer);
        assert_eq!(output.decoded.capacity(), decoded_capacity);
    }

    #[test]
    fn feed_internal_failures_are_propagated_and_remain_sticky() {
        let mut state = SyntaxState::new(Limits::strict_default());
        state.byte_offset = u32::MAX;
        let error = finish(&mut state).unwrap_err();
        assert_error(&error, ErrorCode::InternalError, ErrorSource::Syntax, u32::MAX);
        assert_eq!(state.failure(), Some(&error));
        assert_eq!(finish(&mut state), Err(error.clone()));
        assert_eq!(into_output(state).unwrap_err(), error);
    }
}
