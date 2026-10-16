use jmax_cursor::{
    resume, select, select_borrowed, skip, walk, walk_borrowed, ErrorCode, Event, Limits, Payload,
    Result, ScanStep, Segment, SelectorEdge, SelectorNode, SelectorPlan, SkipMode, ValueSkipMode,
};

fn plan() -> Result<SelectorPlan> {
    SelectorPlan::new(
        vec![
            SelectorNode {
                edges: vec![SelectorEdge {
                    segment: Segment::Key("wanted".into()),
                    target: 1,
                }],
                slots: vec![],
            },
            SelectorNode {
                edges: vec![],
                slots: vec![0],
            },
        ],
        1,
    )
}

#[test]
fn borrowed_selection_retains_original_raw_slice() -> Result<()> {
    let input = br#"{"skip":[{"text":"\n"}],"wanted":"h\u00e9"}"#.to_vec();
    let values = select_borrowed(&input, &plan()?, SkipMode::Strict, Limits::strict_default())?;
    assert_eq!(values.input().as_ptr(), input.as_ptr());
    assert_eq!(values.raw(0), Some(br#""h\u00e9""#.as_slice()));
    assert_eq!(values.string(0), Some("hé"));
    assert!(values.fully_validated());
    assert!(!values.decoded_bytes().contains(&b'\n'));
    let owned = select::extract(
        Payload::from_slice(&input),
        &plan()?,
        SkipMode::Strict,
        Limits::strict_default(),
    )?;
    assert_eq!(owned.raw(0), values.raw(0));
    assert_eq!(owned.string(0), values.string(0));
    assert!(owned.fully_validated());
    Ok(())
}

#[test]
fn query_skips_lexically_invalid_unselected_interiors_honestly() -> Result<()> {
    let input = br#"{"skip":{"bad":truX},"wanted":7}"#;
    let values = select_borrowed(input, &plan()?, SkipMode::Query, Limits::strict_default())?;
    assert_eq!(values.raw(0), Some(b"7".as_slice()));
    assert!(!values.fully_validated());
    assert!(select_borrowed(input, &plan()?, SkipMode::Strict, Limits::strict_default()).is_err());
    let values = select::extract(
        Payload::from_slice(input),
        &plan()?,
        SkipMode::Query,
        Limits::strict_default(),
    )?;
    assert_eq!(values.raw(0), Some(b"7".as_slice()));
    assert!(!values.fully_validated());
    Ok(())
}

#[test]
fn query_missing_paths_still_never_claim_skipped_validation() -> Result<()> {
    let input = br#"{"skip":{"bad":truX}}"#;
    let values = select_borrowed(input, &plan()?, SkipMode::Query, Limits::strict_default())?;
    assert_eq!(values.raw(0), None);
    assert!(!values.fully_validated());
    Ok(())
}

#[test]
fn strict_skip_rejects_duplicates_and_checks_discarded_decoded_budget() -> Result<()> {
    let input = br#"{"skip":{"a":0,"\u0061":1},"wanted":7}"#;
    let error =
        select_borrowed(input, &plan()?, SkipMode::Strict, Limits::strict_default()).unwrap_err();
    assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
    let input = br#"{"skip":["\n","\t"],"wanted":"\r"}"#;
    let mut limits = Limits::strict_default();
    limits.max_decoded_bytes = 2;
    assert_eq!(
        select_borrowed(input, &plan()?, SkipMode::Strict, limits)
            .unwrap_err()
            .code,
        ErrorCode::DecodedArenaOverflow
    );
    Ok(())
}

#[test]
fn borrowed_walk_and_validated_skip() -> Result<()> {
    let input = br#"[{"a":[1,2]},3]"#;
    let validation = jmax_syntax::validate_borrowed_with_capability(
        input,
        Limits::strict_default(),
        &mut jmax_abi::syntax::Scratch::default(),
    )?;
    let mut cursor = walk_borrowed(input, Limits::strict_default(), SkipMode::Strict)?;
    assert!(matches!(
        cursor.next()?,
        ScanStep::Emit(Event::ArrayStart { .. })
    ));
    let value = cursor.skip_validated(&validation)?;
    assert_eq!(value.span.slice(input), Some(input.as_slice()));
    assert!(matches!(cursor.next()?, ScanStep::Finished));
    assert!(cursor.fully_validated());
    let mut cursor = walk_borrowed(b"{]", Limits::strict_default(), SkipMode::Query)?;
    cursor.next()?;
    assert!(cursor.skip_current(ValueSkipMode::QueryOnly).is_err());
    Ok(())
}

#[test]
fn finishing_after_query_skip_revalidates_the_skipped_input() -> Result<()> {
    let input = br#"[{"bad":truX},7]"#;
    let mut cursor = walk::from_payload(
        Payload::from_slice(input),
        Limits::strict_default(),
        SkipMode::Query,
    )?;
    walk::next(&mut cursor)?;
    assert!(matches!(
        skip::subtree(&mut cursor, SkipMode::Query)?,
        jmax_cursor::SkipStep::Complete {
            validated: false,
            ..
        }
    ));
    assert!(resume::finish(cursor).is_err());
    Ok(())
}

#[test]
fn incremental_skip_still_suspends_and_resumes() -> Result<()> {
    let mut cursor = jmax_cursor::Cursor::new(Limits::strict_default(), SkipMode::Strict);
    walk::push(&mut cursor, b"[0,")?;
    assert!(matches!(
        walk::next(&mut cursor)?,
        ScanStep::Emit(Event::ArrayStart { .. })
    ));
    assert_eq!(
        skip::subtree(&mut cursor, SkipMode::Strict)?,
        jmax_cursor::SkipStep::NeedMore
    );
    walk::push(&mut cursor, b"1]")?;
    walk::end_input(&mut cursor)?;
    assert!(matches!(
        skip::subtree(&mut cursor, SkipMode::Strict)?,
        jmax_cursor::SkipStep::Complete {
            validated: true,
            ..
        }
    ));
    assert!(matches!(walk::next(&mut cursor)?, ScanStep::Finished));
    Ok(())
}
