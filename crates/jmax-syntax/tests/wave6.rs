use jmax_abi::syntax::{DuplicatePolicyOwner, SkipMode};
use jmax_syntax::{
    BlockCursor, ErrorCode, Event, Limits, Payload, Result, ScanStep, Scratch, Span32, SpanSource,
    SyntaxState, feed, finish, validate_borrowed, validate_borrowed_with_capability,
};

fn cursor(input: &[u8]) -> Result<BlockCursor<'_>> {
    BlockCursor::new(
        input,
        Limits::strict_default(),
        None,
        DuplicatePolicyOwner::Syntax,
    )
}

#[test]
fn borrowed_ascii_and_unicode_never_append() -> Result<()> {
    for content in ["", "plain ASCII", "héllo 世界 🦀"] {
        for padding in [0, 1, 62, 63, 64, 65, 127] {
            let input = format!("{}\"{}\"", " ".repeat(padding), content);
            let mut scanner = cursor(input.as_bytes())?;
            let mut decoded = b"prefix".to_vec();
            assert_eq!(scanner.skip_whitespace()?, Some(b'"'));
            let meta = scanner.scan_string_meta(&mut decoded)?;
            assert_eq!(
                meta.raw_content.slice(input.as_bytes()),
                Some(content.as_bytes())
            );
            assert_eq!(meta.cooked.source, SpanSource::RawPayload);
            assert_eq!(meta.cooked.scalar_len as usize, content.chars().count());
            assert_eq!(meta.is_ascii, content.is_ascii());
            assert!(meta.is_unescaped);
            assert_eq!(decoded, b"prefix");
            scanner.finish_eof()?;
        }
    }
    Ok(())
}

#[test]
fn escaped_strings_cross_blocks_and_rollback_transactionally() -> Result<()> {
    for padding in [0, 59, 60, 61, 62, 63, 64, 125] {
        let input = format!(
            "{}\"ordinary \\n \\uD83E\\uDD80 é \\\" end\"",
            " ".repeat(padding)
        );
        let mut scanner = cursor(input.as_bytes())?;
        let mut decoded = b"prefix".to_vec();
        scanner.skip_whitespace()?;
        let meta = scanner.scan_string_meta(&mut decoded)?;
        assert_eq!(
            meta.cooked.span.slice(&decoded),
            Some("ordinary \n 🦀 é \" end".as_bytes())
        );
        assert_eq!(meta.cooked.source, SpanSource::DecodedArena);
        assert!(!meta.is_unescaped);
        assert!(!meta.is_ascii);
    }
    for input in [
        b"\"ok\\nthen\\q\"".as_slice(),
        b"\"ok\\n\\uD800\"",
        b"\"ok\\n\x01\"",
        b"\"ok\\n\xff\"",
    ] {
        let mut scanner = cursor(input)?;
        let mut decoded = b"prefix".to_vec();
        let error = scanner.scan_string_meta(&mut decoded).unwrap_err();
        assert_eq!(decoded, b"prefix");
        assert_eq!(scanner.skip_whitespace().unwrap_err(), error);
    }
    Ok(())
}

#[test]
fn dense_escaped_strings_decode_and_discard_at_every_block_alignment() -> Result<()> {
    let raw = r#"x\n\"\u0061\uD83E\uDD80é"#;
    let cooked = "x\n\"a🦀é";
    for padding in 0..64 {
        let input = format!("\"{}{}\"", "a".repeat(padding), raw.repeat(40));
        let expected = format!("{}{}", "a".repeat(padding), cooked.repeat(40));
        let mut scanner = cursor(input.as_bytes())?;
        let mut decoded = b"prefix:".to_vec();
        let meta = scanner.scan_string_meta(&mut decoded)?;
        assert_eq!(meta.cooked.span.slice(&decoded), Some(expected.as_bytes()));
        assert_eq!(meta.cooked.scalar_len as usize, expected.chars().count());
        scanner.finish_eof()?;

        let mut scanner = cursor(input.as_bytes())?;
        let mut decoded = Vec::new();
        scanner.skip_value(&mut decoded, 0, SkipMode::StrictValidate)?;
        assert_eq!(decoded.capacity(), 0);
        scanner.finish_eof()?;
    }
    Ok(())
}

#[test]
fn controls_are_reported_at_the_exact_byte() -> Result<()> {
    let mut input = vec![b' '; 63];
    input.push(b'"');
    input.extend_from_slice(&[b'a', b'b', 0x01, b'c', b'"']);
    let mut scanner = cursor(&input)?;
    scanner.skip_whitespace()?;
    let error = scanner.scan_string_meta(&mut Vec::new()).unwrap_err();
    assert_eq!(error.code, ErrorCode::ControlCharInString);
    assert_eq!(error.byte_offset, 66);
    Ok(())
}

#[test]
fn key_hashes_use_cooked_bytes() -> Result<()> {
    let input = br#""a\u0062c""#;
    let mut scanner = cursor(input)?;
    let mut decoded = Vec::new();
    let key = scanner.scan_key_meta(&mut decoded, true)?;
    assert_eq!(
        key.swar_head,
        jmax_abi::syntax::KeyMeta::compute_swar_head(b"abc")
    );
    assert_eq!(
        key.cooked_hash,
        jmax_abi::syntax::KeyMeta::compute_fnv1a64(b"abc")
    );
    Ok(())
}

#[test]
fn duplicate_owner_is_explicit() -> Result<()> {
    let input = br#"{"a":0,"\u0061":1}"#;
    let mut scanner = cursor(input)?;
    let mut decoded = Vec::new();
    let error = loop {
        match scanner.next_event(&mut decoded) {
            Ok(ScanStep::Emit(_)) => {}
            Err(error) => break error,
            _ => panic!("duplicate input unexpectedly completed"),
        }
    };
    assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
    assert_eq!(error.byte_offset, 7);
    let mut scanner = BlockCursor::new(
        input,
        Limits::strict_default(),
        None,
        DuplicatePolicyOwner::SchemaVm,
    )?;
    while matches!(scanner.next_event(&mut Vec::new())?, ScanStep::Emit(_)) {}
    assert!(!scanner.fully_validated());
    assert!(
        BlockCursor::new(
            input,
            Limits::strict_default(),
            None,
            DuplicatePolicyOwner::Disabled
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn query_strict_and_capability_skips_have_distinct_contracts() -> Result<()> {
    let input = br#"[{"bad":truX,"text":"}][{"},[]] suffix"#;
    let mut scanner = cursor(input)?;
    let span = scanner.skip_value(&mut Vec::new(), 0, SkipMode::QueryOnly)?;
    assert_eq!(
        span.slice(input),
        Some(br#"[{"bad":truX,"text":"}][{"},[]]"#.as_slice())
    );
    assert!(!scanner.fully_validated());
    let mut scanner = cursor(input)?;
    assert!(
        scanner
            .skip_value(&mut Vec::new(), 0, SkipMode::StrictValidate)
            .is_err()
    );
    for input in [b"{]".as_slice(), b"[{]}]", b"[{}]]", b"[}{}]"] {
        let mut scanner = cursor(input)?;
        let result = scanner.skip_value(&mut Vec::new(), 0, SkipMode::QueryOnly);
        if input == b"[{}]]" {
            assert_eq!(result?, Span32::new(0, 4));
            assert!(scanner.finish_eof().is_err());
        } else {
            assert!(result.is_err());
        }
    }
    let valid = br#"[{"text":"}][{"},[]]"#;
    let validation = validate_borrowed_with_capability(
        valid,
        Limits::strict_default(),
        &mut Scratch::default(),
    )?;
    let mut scanner = cursor(valid)?;
    let span = validation.skip_value(&mut scanner, &mut Vec::new(), 0)?;
    assert_eq!(span, Span32::new(0, valid.len() as u32));
    let different = valid.to_vec();
    let mut scanner = cursor(&different)?;
    assert!(
        validation
            .skip_value(&mut scanner, &mut Vec::new(), 0)
            .is_err()
    );
    let mut changed = Limits::strict_default();
    changed.max_depth -= 1;
    let mut scanner = BlockCursor::new(valid, changed, None, DuplicatePolicyOwner::Syntax)?;
    assert!(
        validation
            .skip_value(&mut scanner, &mut Vec::new(), 0)
            .is_err()
    );
    Ok(())
}

#[test]
fn strict_discard_rejects_each_malformed_interior_class() -> Result<()> {
    for input in [
        b"{]".as_slice(),
        b"[\"a\x01b\"]",
        b"[01]",
        br#"{"a":1,"\u0061":2}"#,
    ] {
        let mut scanner = cursor(input)?;
        assert!(
            scanner
                .skip_value(&mut Vec::new(), 0, SkipMode::StrictValidate)
                .is_err(),
            "strict discard accepted {input:?}"
        );
    }
    Ok(())
}

#[test]
fn strict_discard_preserves_global_decoded_budget() -> Result<()> {
    let input = br#"["\n","\t","\r"]"#;
    let mut limits = Limits::strict_default();
    limits.max_decoded_bytes = 2;
    let error = validate_borrowed(input, limits, None, &mut Scratch::default()).unwrap_err();
    assert_eq!(error.code, ErrorCode::DecodedArenaOverflow);
    limits.max_decoded_bytes = 3;
    let mut scratch = Scratch::default();
    let proof = validate_borrowed(input, limits, None, &mut scratch)?;
    assert_eq!(proof.payload_len(), input.len() as u32);
    assert_eq!(proof.decoded_len(), 0);
    assert!(scratch.decoded_buf.is_empty());
    Ok(())
}

#[test]
fn atoms_and_resource_limits() -> Result<()> {
    for input in [
        b"01".as_slice(),
        b"1e",
        b"trueX",
        b"null[",
        b"false:",
        b"1[",
        b"[1,]",
        b"{}{}",
    ] {
        assert!(
            validate_borrowed(
                input,
                Limits::strict_default(),
                None,
                &mut Scratch::default()
            )
            .is_err()
        );
    }
    let mut limits = Limits::strict_default();
    limits.max_number_digits = 2;
    let mut scanner = BlockCursor::new(b"12.3", limits, None, DuplicatePolicyOwner::Syntax)?;
    let error = scanner.scan_number_span().unwrap_err();
    assert_eq!(error.code, ErrorCode::ConstraintViolation);
    assert_eq!(error.byte_offset, 3);
    limits = Limits::strict_default();
    limits.max_depth = 1;
    assert!(validate_borrowed(b"[[]]", limits, None, &mut Scratch::default()).is_err());
    limits = Limits::strict_default();
    limits.max_fields_per_object = 1;
    assert!(validate_borrowed(br#"{"a":0,"b":1}"#, limits, None, &mut Scratch::default()).is_err());
    limits = Limits::strict_default();
    limits.max_array_items = 1;
    assert!(validate_borrowed(b"[0,1]", limits, None, &mut Scratch::default()).is_err());
    Ok(())
}

#[test]
fn mask_proven_integer_path_retains_number_grammar_boundaries_and_policies() -> Result<()> {
    for spelling in ["0", "1", "184467440737095516160", "-0", "-1", "-987654321"] {
        for padding in [0, 62, 63, 64, 125] {
            for suffix in ["", " ", ",", "]", "}"] {
                let input = format!("{}{spelling}{suffix}", " ".repeat(padding));
                let mut scanner = cursor(input.as_bytes())?;
                scanner.skip_whitespace()?;
                assert_eq!(
                    scanner.scan_number_span()?,
                    Span32::new(padding as u32, spelling.len() as u32),
                );
            }
        }
    }
    for input in [
        "00", "01", "-00", "-01", "12[", "12{", "12:", "12\"", "12x", "12é",
    ] {
        assert!(
            cursor(input.as_bytes())?.scan_number_span().is_err(),
            "{input}"
        );
    }
    let policy = jmax_abi::number::NumericPolicy::strict_default();
    for input in [b"-0".as_slice(), b"-0 "] {
        let mut scanner = BlockCursor::new(
            input,
            Limits::strict_default(),
            Some(policy),
            DuplicatePolicyOwner::Syntax,
        )?;
        assert_eq!(
            scanner.scan_number_span().unwrap_err().code,
            ErrorCode::ConstraintViolation
        );
    }
    let mut limits = Limits::strict_default();
    limits.max_number_digits = 2;
    let mut scanner = BlockCursor::new(b"-123", limits, None, DuplicatePolicyOwner::Syntax)?;
    let error = scanner.scan_number_span().unwrap_err();
    assert_eq!(
        (error.code, error.byte_offset),
        (ErrorCode::ConstraintViolation, 3)
    );
    Ok(())
}

fn collect(state: &mut SyntaxState, events: &mut Vec<Event>) -> Result<()> {
    loop {
        match feed::next(state)? {
            ScanStep::Emit(event) => events.push(event),
            ScanStep::NeedMore | ScanStep::Finished => return Ok(()),
        }
    }
}
#[test]
fn complete_and_every_incremental_split_produce_identical_events() -> Result<()> {
    let input = br#" {"a":[true,false,null,-12.25e+2,"raw","\n\uD83E\uDD80"],"b":{}} "#;
    let mut complete =
        SyntaxState::from_payload(Payload::from_slice(input), Limits::strict_default())?;
    let mut expected = Vec::new();
    collect(&mut complete, &mut expected)?;
    assert!(complete.fully_validated());
    for split in 0..=input.len() {
        let mut state = SyntaxState::new(Limits::strict_default());
        let mut events = Vec::new();
        feed::push(&mut state, &input[..split])?;
        collect(&mut state, &mut events)?;
        feed::push(&mut state, &input[split..])?;
        finish::end_input(&mut state)?;
        collect(&mut state, &mut events)?;
        assert_eq!(events, expected, "split {split}");
        assert_eq!(state.decoded_bytes(), complete.decoded_bytes());
        assert!(state.fully_validated());
    }
    Ok(())
}
