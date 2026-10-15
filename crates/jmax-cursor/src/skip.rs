//! Resumable subtree consumption over the canonical syntax event stream.
//!
//! Both modes validate consumed tokens using the syntax parser. Query results
//! deliberately retain a conservative validation flag. Completing a subtree
//! does not consume its suffix or establish whole-document validity.

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
                Some(Event::Scalar { kind, span, decoded }) => {
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
                        value: SelectedValue { kind, span, decoded },
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
            SkipProgress { start, kind, depth: 1, mode }
        }
    };

    if progress.depth == 0
        || !matches!(progress.kind, ValueKind::Object | ValueKind::Array)
    {
        return Err(cursor_error(
            ErrorCode::InternalError,
            cursor.state().byte_offset,
            "pending subtree skip has invalid container progress",
        ));
    }
    cursor.skipping = Some(progress);

    loop {
        // Ordinary walk::next intentionally rejects active skip progress.
        // Advance the same parser directly so all token and frame checks apply.
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
                        value: SelectedValue { kind: progress.kind, span, decoded: None },
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
    let len = end.checked_sub(start).filter(|&len| len >= 2).ok_or_else(|| {
        cursor_error(
            ErrorCode::InternalError,
            error_offset,
            "skipped subtree delimiters are out of order",
        )
    })?;
    Ok(Span32::new(start, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(bytes: &[u8], limits: Limits, mode: SkipMode) -> Cursor {
        walk::from_payload(Payload::from_slice(bytes), limits, mode).unwrap()
    }

    fn emit(cursor: &mut Cursor) -> Event {
        match walk::next(cursor).unwrap() {
            ScanStep::Emit(event) => event,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    fn selected(step: SkipStep, mode: SkipMode) -> SelectedValue {
        match step {
            SkipStep::Complete { value, validated } => {
                assert_eq!(validated, mode == SkipMode::Strict);
                value
            }
            SkipStep::NeedMore => panic!("expected a complete subtree"),
        }
    }

    fn assert_same_state(actual: CursorState, expected: CursorState) {
        assert_eq!(actual.byte_offset, expected.byte_offset);
        assert_eq!(actual.depth, expected.depth);
        assert_eq!(actual.phase, expected.phase);
        assert_eq!(actual.finished, expected.finished);
    }

    fn syntax_failure(bytes: &[u8], limits: Limits) -> Error {
        let mut syntax = SyntaxState::new(limits);
        jmax_syntax::feed::push(&mut syntax, bytes).unwrap();
        jmax_syntax::finish::end_input(&mut syntax).unwrap();
        for _ in 0..=bytes.len() + 1 {
            match jmax_syntax::feed::next(&mut syntax) {
                Err(error) => return error,
                Ok(ScanStep::Emit(_)) => {}
                Ok(other) => panic!("malformed input returned {other:?}"),
            }
        }
        panic!("syntax traversal failed to terminate");
    }

    #[test]
    fn scalar_kinds_preserve_raw_tokens_and_do_not_advance() {
        let cases: &[(&[u8], ValueKind)] = &[
            (b"null", ValueKind::Null),
            (b"true", ValueKind::Bool),
            (b"false", ValueKind::Bool),
            (b"-0.25e+10", ValueKind::Number),
            (b"123456789012345678901234567890", ValueKind::Number),
            (b"1e999999", ValueKind::Number),
            (br#""""#, ValueKind::String),
            (br#""a\n\uD83D\uDE00""#, ValueKind::String),
        ];
        for cursor_mode in [SkipMode::Strict, SkipMode::Query] {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                for &(bytes, kind) in cases {
                    let mut cursor = complete(bytes, Limits::strict_default(), cursor_mode);
                    let event = emit(&mut cursor);
                    let (span, decoded) = match event {
                        Event::Scalar { span, decoded, .. } => (span, decoded),
                        _ => panic!("expected scalar"),
                    };
                    let before = cursor.state();
                    let arena = cursor.decoded_bytes().to_vec();
                    let expected = SelectedValue { kind, span, decoded };
                    for _ in 0..3 {
                        assert_eq!(selected(subtree(&mut cursor, mode).unwrap(), mode), expected);
                        assert_same_state(cursor.state(), before.clone());
                        assert_eq!(cursor.current(), Some(event));
                        assert_eq!(cursor.skipping, None);
                        assert_eq!(cursor.decoded_bytes(), arena.as_slice());
                        assert_eq!(span.slice(cursor.raw_bytes()), Some(bytes));
                    }
                    assert_eq!(cursor.mode(), cursor_mode);
                    assert!(!cursor.state().finished);
                    assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
                }
            }
        }
    }

    #[test]
    fn string_results_keep_quotes_and_resolve_both_storage_sources() {
        let cases: &[(&[u8], &str, SpanSource)] = &[
            (br#""""#, "", SpanSource::RawPayload),
            ("\"é😀\"".as_bytes(), "é😀", SpanSource::RawPayload),
            (br#""a\n\uD83D\uDE00""#, "a\n😀", SpanSource::DecodedArena),
            (br#""\"\\\/\b\f\n\r\t""#, "\"\\/\u{8}\u{c}\n\r\t", SpanSource::DecodedArena),
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &(bytes, cooked, source) in cases {
                let mut cursor = complete(bytes, Limits::strict_default(), mode);
                emit(&mut cursor);
                let value = selected(subtree(&mut cursor, mode).unwrap(), mode);
                let reference = value.decoded.unwrap();
                assert_eq!(value.kind, ValueKind::String);
                assert_eq!(value.span.slice(cursor.raw_bytes()), Some(bytes));
                assert_eq!(reference.source, source);
                assert_eq!(reference.scalar_len as usize, cooked.chars().count());
                assert_eq!(cursor.resolve_str(reference), Some(cooked));
            }
        }
    }

    #[test]
    fn containers_include_delimiters_and_leave_the_closing_event_current() {
        let cases: &[(&[u8], ValueKind)] = &[
            (b"[]", ValueKind::Array),
            (b"{}", ValueKind::Object),
            (br#"[{},[],[true,false,null,-12.5],"[}]\"\\"]"#, ValueKind::Array),
            (br#"{"a":[{"b":"\u005d"}],"c":{}}"#, ValueKind::Object),
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &(bytes, kind) in cases {
                let mut input = b" \n\t".to_vec();
                input.extend_from_slice(bytes);
                input.extend_from_slice(b" \r\n");
                let mut cursor = complete(&input, Limits::strict_default(), mode);
                emit(&mut cursor);
                let value = selected(subtree(&mut cursor, mode).unwrap(), mode);
                assert_eq!(value, SelectedValue {
                    kind, span: Span32::new(3, bytes.len() as u32), decoded: None,
                });
                assert_eq!(value.span.slice(cursor.raw_bytes()), Some(bytes));
                let close = 3 + bytes.len() as u32 - 1;
                let event = match kind {
                    ValueKind::Object => Event::ObjectEnd { offset: close },
                    ValueKind::Array => Event::ArrayEnd { offset: close },
                    _ => panic!("expected container kind"),
                };
                assert_eq!(cursor.current(), Some(event));
                assert_eq!(cursor.state().byte_offset, close + 1);
                assert_eq!(cursor.state().depth, 0);
                assert!(!cursor.state().finished);
                assert_eq!(cursor.skipping, None);
                let before = cursor.state();
                let error = subtree(&mut cursor, mode).unwrap_err();
                assert_eq!(error.code, ErrorCode::TypeMismatch);
                assert_eq!(error.source, ErrorSource::Cursor);
                assert_eq!(error.byte_offset, close + 1);
                assert_same_state(cursor.state(), before);
                assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
            }
        }
    }

    #[test]
    fn nested_subtree_stops_before_its_sibling() {
        let bytes = br#"{"take":[{"x":1},2],"keep":3}"#;
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = complete(bytes, Limits::strict_default(), mode);
            assert!(matches!(emit(&mut cursor), Event::ObjectStart { .. }));
            assert!(matches!(emit(&mut cursor), Event::Key { .. }));
            assert_eq!(emit(&mut cursor), Event::ArrayStart { offset: 8 });
            let value = selected(subtree(&mut cursor, mode).unwrap(), mode);
            assert_eq!(value.span, Span32::new(8, 11));
            assert_eq!(value.span.slice(cursor.raw_bytes()), Some(&b"[{\"x\":1},2]"[..]));
            assert_eq!(cursor.state().depth, 1);
            assert_eq!(cursor.current(), Some(Event::ArrayEnd { offset: 18 }));
            match emit(&mut cursor) {
                Event::Key { str_ref } => assert_eq!(cursor.resolve_str(str_ref), Some("keep")),
                _ => panic!("sibling key was consumed by skip"),
            }
            assert!(matches!(emit(&mut cursor), Event::Scalar { kind: ScalarKind::Number, .. }));
            assert!(matches!(emit(&mut cursor), Event::ObjectEnd { .. }));
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
        }
    }

    #[test]
    fn absent_keys_and_end_events_are_recoverable_type_errors() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = complete(br#"{"a":[]}"#, Limits::strict_default(), mode);
            for stage in 0..4 {
                match stage {
                    0 => assert_eq!(cursor.current(), None),
                    1 => {
                        emit(&mut cursor);
                        assert!(matches!(emit(&mut cursor), Event::Key { .. }));
                    }
                    2 => {
                        emit(&mut cursor);
                        assert!(matches!(emit(&mut cursor), Event::ArrayEnd { .. }));
                    }
                    _ => assert!(matches!(emit(&mut cursor), Event::ObjectEnd { .. })),
                }
                let before = cursor.state();
                let current = cursor.current();
                let error = subtree(&mut cursor, mode).unwrap_err();
                assert_eq!(error.code, ErrorCode::TypeMismatch);
                assert_eq!(error.source, ErrorSource::Cursor);
                assert_eq!(error.byte_offset, before.byte_offset);
                assert_same_state(cursor.state(), before);
                assert_eq!(cursor.current(), current);
                assert_eq!(cursor.skipping, None);
                assert!(cursor.syntax.failure().is_none());
            }
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
        }
    }

    #[test]
    fn every_two_chunk_split_preserves_subtree_and_decoding() {
        let bytes = r#"[{"é\u0061":["a\\\"b","\uD83D\uDE00",-1.25e-3,true,false,null,{},[]]}]"#.as_bytes();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut baseline = complete(bytes, Limits::strict_default(), mode);
            emit(&mut baseline);
            let expected = subtree(&mut baseline, mode).unwrap();
            for split in 0..=bytes.len() {
                let mut cursor = Cursor::new(Limits::strict_default(), mode);
                walk::push(&mut cursor, &bytes[..split]).unwrap();
                let first = walk::next(&mut cursor).unwrap();
                let early = match first {
                    ScanStep::Emit(Event::ArrayStart { offset: 0 }) => {
                        Some(subtree(&mut cursor, mode).unwrap())
                    }
                    ScanStep::NeedMore => {
                        assert_eq!(split, 0);
                        None
                    }
                    _ => panic!("unexpected first event"),
                };
                if early == Some(SkipStep::NeedMore) {
                    let progress = cursor.skipping;
                    let before = cursor.state();
                    let current = cursor.current();
                    let arena = cursor.decoded_bytes().to_vec();
                    assert_eq!(subtree(&mut cursor, mode).unwrap(), SkipStep::NeedMore);
                    assert_same_state(cursor.state(), before);
                    assert_eq!(cursor.skipping, progress);
                    assert_eq!(cursor.current(), current);
                    assert_eq!(cursor.decoded_bytes(), arena.as_slice());
                }
                walk::push(&mut cursor, &bytes[split..]).unwrap();
                walk::end_input(&mut cursor).unwrap();
                let actual = match early {
                    Some(step @ SkipStep::Complete { .. }) => step,
                    _ => {
                        if early.is_none() {
                            emit(&mut cursor);
                        }
                        subtree(&mut cursor, mode).unwrap()
                    }
                };
                assert_eq!(actual, expected, "split at byte {split}");
                assert_eq!(cursor.decoded_bytes(), baseline.decoded_bytes());
                assert_eq!(cursor.current(), baseline.current());
                assert_eq!(cursor.skipping, None);
                assert_eq!(cursor.raw_bytes(), bytes);
                assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
            }
        }
    }

    #[test]
    fn bytewise_input_tracks_depth_across_escapes_and_owned_chunks() {
        let bytes = r#"[[{"é":"\uD83D\uDE00\\\"}]"}],-1.2e+3]"#.as_bytes();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = Cursor::new(Limits::strict_default(), mode);
            walk::push(&mut cursor, &bytes[..1]).unwrap();
            emit(&mut cursor);
            assert_eq!(subtree(&mut cursor, mode).unwrap(), SkipStep::NeedMore);
            for (index, &byte) in bytes.iter().enumerate().skip(1) {
                let mut chunk = [byte];
                walk::push(&mut cursor, &chunk).unwrap();
                chunk[0] = b'!';
                assert_eq!(chunk, [b'!']);
                walk::push(&mut cursor, &[]).unwrap();
                let step = subtree(&mut cursor, mode).unwrap();
                if index + 1 == bytes.len() {
                    let value = selected(step, mode);
                    assert_eq!(value.span.slice(cursor.raw_bytes()), Some(bytes));
                    assert_eq!(cursor.skipping, None);
                } else {
                    assert_eq!(step, SkipStep::NeedMore);
                    let progress = cursor.skipping.unwrap();
                    assert_eq!(progress.depth, cursor.state().depth);
                    assert_eq!(progress.start, 0);
                    assert_eq!(progress.mode, mode);
                }
            }
            assert!(!cursor.syntax.is_final());
            assert!(!cursor.syntax.is_finished());
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::NeedMore);
            walk::end_input(&mut cursor).unwrap();
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
        }
    }

    #[test]
    fn mode_mismatch_and_walk_during_skip_do_not_mutate_progress() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let other = if mode == SkipMode::Strict { SkipMode::Query } else { SkipMode::Strict };
            let mut cursor = Cursor::new(Limits::strict_default(), other);
            walk::push(&mut cursor, b"[[0,").unwrap();
            emit(&mut cursor);
            assert_eq!(subtree(&mut cursor, mode).unwrap(), SkipStep::NeedMore);
            assert_eq!(cursor.skipping.unwrap().depth, 2);
            let progress = cursor.skipping;
            let before = cursor.state();
            let current = cursor.current();
            // The mismatch must still be non-consuming when more input is ready.
            walk::push(&mut cursor, b"1],2]").unwrap();
            walk::end_input(&mut cursor).unwrap();
            for _ in 0..2 {
                let error = subtree(&mut cursor, other).unwrap_err();
                assert_eq!(error.code, ErrorCode::ConstraintViolation);
                assert_eq!(error.source, ErrorSource::Cursor);
                assert_eq!(error.byte_offset, before.byte_offset);
                assert_eq!(walk::next(&mut cursor).unwrap_err().code, ErrorCode::InvalidSyntax);
                assert_same_state(cursor.state(), before.clone());
                assert_eq!(cursor.current(), current);
                assert_eq!(cursor.skipping, progress);
                assert!(cursor.syntax.failure().is_none());
            }
            let value = selected(subtree(&mut cursor, mode).unwrap(), mode);
            assert_eq!(value.span.slice(cursor.raw_bytes()), Some(&b"[[0,1],2]"[..]));
            assert_eq!(cursor.mode(), other);
            assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
        }
    }

    #[test]
    fn malformed_consumed_bytes_propagate_identical_sticky_errors() {
        let cases: &[&[u8]] = &[
            b"[", b"{\"a\":", b"[[0]", b"[tru", b"[1e", b"[0,]",
            b"{\"a\":0,}", b"{0:1}", b"[}", b"[truex]", b"[01]", b"[+1]",
            b"[\"unterminated", b"[\"\\q\"]", b"[\"\\uD800\"]", b"[\"\\uDC00\"]",
            b"[\"\x01\"]", b"[\"\xff\"]", br#"{"a":0,"\u0061":1}"#,
            br#"[{"ok":0},{"x":1,"x":2}]"#,
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &bytes in cases {
                let limits = Limits::strict_default();
                let expected = syntax_failure(bytes, limits);
                let mut cursor = complete(bytes, limits, mode);
                emit(&mut cursor);
                let error = subtree(&mut cursor, mode).unwrap_err();
                assert_eq!(error, expected);
                assert!(cursor.skipping.is_some());
                assert!(!cursor.state().finished);
                let current = cursor.current();
                let progress = cursor.skipping;
                let before = cursor.state();
                for retry_mode in [SkipMode::Strict, SkipMode::Query] {
                    assert_eq!(subtree(&mut cursor, retry_mode).unwrap_err(), error);
                }
                assert_eq!(walk::next(&mut cursor).unwrap_err(), error);
                assert_eq!(walk::push(&mut cursor, b" ").unwrap_err(), error);
                assert_eq!(walk::end_input(&mut cursor).unwrap_err(), error);
                assert_same_state(cursor.state(), before);
                assert_eq!(cursor.current(), current);
                assert_eq!(cursor.skipping, progress);
            }
        }
    }

    #[test]
    fn eof_after_suspension_reports_unexpected_eof_at_input_end() {
        for bytes in [&b"["[..], &b"[{\"a\":"[..], &b"[[0]"[..]] {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let mut cursor = Cursor::new(Limits::strict_default(), mode);
                walk::push(&mut cursor, bytes).unwrap();
                emit(&mut cursor);
                assert_eq!(subtree(&mut cursor, mode).unwrap(), SkipStep::NeedMore);
                let current = cursor.current();
                walk::end_input(&mut cursor).unwrap();
                let error = subtree(&mut cursor, mode).unwrap_err();
                assert_eq!(error.code, ErrorCode::UnexpectedEof);
                assert_eq!(error.source, ErrorSource::Syntax);
                assert_eq!(error.byte_offset as usize, bytes.len());
                assert_eq!(cursor.current(), current);
                assert_eq!(subtree(&mut cursor, mode).unwrap_err(), error);
            }
        }
    }

    #[test]
    fn subtree_completion_never_validates_or_consumes_a_bad_suffix() {
        for bytes in [&b"[] false"[..], &b"{}\xff"[..], &b"[[],bad]"[..]] {
            for mode in [SkipMode::Strict, SkipMode::Query] {
                let mut cursor = complete(bytes, Limits::strict_default(), mode);
                emit(&mut cursor);
                let start = if bytes == b"[[],bad]" {
                    emit(&mut cursor);
                    1
                } else {
                    0
                };
                let value = selected(subtree(&mut cursor, mode).unwrap(), mode);
                assert_eq!(value.span, Span32::new(start, 2));
                assert_eq!(cursor.state().byte_offset, start + 2);
                assert!(!cursor.state().finished);
                assert!(cursor.syntax.failure().is_none());
                assert!(walk::next(&mut cursor).is_err());
            }
        }
    }

    #[test]
    fn inclusive_syntax_limits_apply_inside_skipped_containers() {
        let defaults = Limits::strict_default();
        let cases: &[(Limits, &[u8], &[u8], ErrorCode)] = &[
            (Limits { max_depth: 2, ..defaults }, b"[[]]", b"[[[]]]", ErrorCode::DepthLimitExceeded),
            (Limits { max_array_items: 1, ..defaults }, b"[0]", b"[0,1]", ErrorCode::ConstraintViolation),
            (Limits { max_array_items: 0, ..defaults }, b"[]", b"[0]", ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 1, ..defaults }, br#"{"a":0}"#, br#"{"a":0,"b":1}"#, ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 0, ..defaults }, b"{}", br#"{"a":0}"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 1, ..defaults }, br#"["a"]"#, br#"["ab"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 0, ..defaults }, br#"[""]"#, br#"["a"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_number_digits: 2, ..defaults }, b"[1e2]", b"[1e23]", ErrorCode::ConstraintViolation),
            (Limits { max_decoded_bytes: 1, ..defaults }, br#"["\n"]"#, br#"["\n","\t"]"#, ErrorCode::DecodedArenaOverflow),
        ];
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for &(limits, valid, invalid, code) in cases {
                let mut cursor = complete(valid, limits, mode);
                emit(&mut cursor);
                selected(subtree(&mut cursor, mode).unwrap(), mode);
                let mut cursor = complete(invalid, limits, mode);
                emit(&mut cursor);
                let error = subtree(&mut cursor, mode).unwrap_err();
                assert_eq!(error.code, code);
                assert_eq!(error, syntax_failure(invalid, limits));
            }
        }
    }

    #[test]
    fn duplicate_keys_follow_decoded_identity_and_configured_policy() {
        let bytes = br#"{"a":0,"\u0061":1}"#;
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut strict = complete(bytes, Limits::strict_default(), mode);
            emit(&mut strict);
            assert_eq!(subtree(&mut strict, mode).unwrap_err().code, ErrorCode::DuplicateObjectKey);
            let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
            let mut permissive = complete(bytes, limits, mode);
            emit(&mut permissive);
            let value = selected(subtree(&mut permissive, mode).unwrap(), mode);
            assert_eq!(value.span.slice(permissive.raw_bytes()), Some(&bytes[..]));
            let mut scoped = complete(br#"[{"a":1},{"a":2}]"#, Limits::strict_default(), mode);
            emit(&mut scoped);
            selected(subtree(&mut scoped, mode).unwrap(), mode);
        }
    }

    #[test]
    fn a_failed_feed_takes_precedence_over_scalar_and_pending_skip_results() {
        let limits = Limits { max_payload_bytes: 2, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            for bytes in [&b"0 "[..], &b"[["[..]] {
                let mut cursor = Cursor::new(limits, mode);
                walk::push(&mut cursor, bytes).unwrap();
                emit(&mut cursor);
                if bytes == b"[[" {
                    assert_eq!(subtree(&mut cursor, mode).unwrap(), SkipStep::NeedMore);
                }
                let current = cursor.current();
                let progress = cursor.skipping;
                let error = walk::push(&mut cursor, b" ").unwrap_err();
                assert_eq!(error.code, ErrorCode::PayloadTooLarge);
                for retry_mode in [SkipMode::Strict, SkipMode::Query] {
                    assert_eq!(subtree(&mut cursor, retry_mode).unwrap_err(), error);
                }
                assert_eq!(cursor.current(), current);
                assert_eq!(cursor.skipping, progress);
                assert_eq!(cursor.raw_bytes(), bytes);
            }
        }
    }

    #[test]
    fn deep_nesting_uses_iterative_progress() {
        let depth = 1024usize;
        let mut bytes = Vec::new();
        bytes.resize(depth, b'[');
        bytes.resize(depth * 2, b']');
        let limits = Limits { max_depth: depth as u16, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let mut cursor = complete(&bytes, limits, mode);
            emit(&mut cursor);
            let value = selected(subtree(&mut cursor, mode).unwrap(), mode);
            assert_eq!(value.span, Span32::new(0, bytes.len() as u32));
            assert_eq!(cursor.state().depth, 0);
            assert_eq!(cursor.skipping, None);
        }
    }

    #[test]
    fn checked_container_spans_cover_u32_boundaries() {
        assert_eq!(container_span(0, 1, 2).unwrap(), Span32::new(0, 2));
        assert_eq!(container_span(0, u32::MAX - 1, 7).unwrap(), Span32::new(0, u32::MAX));
        assert_eq!(container_span(u32::MAX - 2, u32::MAX - 1, 7).unwrap(), Span32::new(u32::MAX - 2, 2));
        let error = container_span(0, u32::MAX, 7).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!(error.byte_offset, 7);
        for (start, close) in [(0, 0), (2, 1), (3, 1), (u32::MAX, u32::MAX - 1)] {
            let error = container_span(start, close, 9).unwrap_err();
            assert_eq!(error.code, ErrorCode::InternalError);
            assert_eq!(error.byte_offset, 9);
        }
    }

    #[test]
    fn impossible_finished_parser_and_depth_overflow_are_errors() {
        let mode = SkipMode::Strict;
        let mut cursor = complete(b"[]", Limits::strict_default(), mode);
        emit(&mut cursor);
        emit(&mut cursor);
        assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
        let progress = SkipProgress { start: 0, kind: ValueKind::Array, depth: 1, mode };
        cursor.skipping = Some(progress);
        let current = cursor.current();
        assert_eq!(subtree(&mut cursor, mode).unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(cursor.current(), current);
        assert_eq!(cursor.skipping, Some(progress));

        let mut cursor = complete(b"[[]]", Limits::strict_default(), mode);
        emit(&mut cursor);
        cursor.skipping = Some(SkipProgress { depth: u16::MAX, ..progress });
        let error = subtree(&mut cursor, mode).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!(cursor.current(), Some(Event::ArrayStart { offset: 1 }));
        assert_eq!(cursor.skipping.unwrap().depth, u16::MAX);
    }

    #[test]
    fn invalid_internal_progress_is_rejected_without_advancing() {
        for (kind, depth) in [(ValueKind::Array, 0), (ValueKind::Number, 1)] {
            let mut cursor = complete(b"[]", Limits::strict_default(), SkipMode::Strict);
            emit(&mut cursor);
            let progress = SkipProgress { start: 0, kind, depth, mode: SkipMode::Strict };
            cursor.skipping = Some(progress);
            let before = cursor.state();
            let current = cursor.current();
            assert_eq!(subtree(&mut cursor, SkipMode::Strict).unwrap_err().code, ErrorCode::InternalError);
            assert_same_state(cursor.state(), before);
            assert_eq!(cursor.current(), current);
            assert_eq!(cursor.skipping, Some(progress));
        }
    }
}
