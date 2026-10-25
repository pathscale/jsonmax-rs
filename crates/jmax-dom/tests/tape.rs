use jmax_dom::{
    BorrowedDocument, ByteSink, DomScratch, Edit, EditAction, Error, ErrorCode, ErrorSource,
    Limits, NodeId, NodeKind, Payload, Result, Span32, SpanSource, StrRef, TAPE_AUX_LEN_MASK,
    TAPE_FLAG_EXTENDED, TAPE_FLAG_HAS_KEY, TAPE_FLAG_KEY_EXTENDED, TAPE_TAG_KEY, build, navigate,
    rebuild,
};

fn required<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(|| {
        Error::new(
            ErrorCode::InternalError,
            ErrorSource::Dom,
            0,
            "test expected a navigation result",
        )
    })
}

#[test]
fn compact_record_offsets_and_sibling_jumps() -> Result<()> {
    let raw = b"[null,false,true,12,\"abc\",[],{}]";
    let doc = build::parse_borrowed(raw, Limits::strict_default())?;
    assert_eq!(doc.raw().as_ptr(), raw.as_ptr());
    assert_eq!(doc.root(), NodeId(0));
    assert_eq!(doc.node_count(), 8);
    assert_eq!(doc.container_count(), 3);
    assert_eq!(doc.tape().len(), 19);
    assert!(doc.decoded().is_empty());
    let array = doc.root_node().array()?;
    assert_eq!(array.len(), 7);
    let ids: Vec<_> = array.iter().map(|node| node.id().0).collect();
    assert_eq!(ids, [3, 5, 7, 9, 11, 13, 16]);
    for child in array.iter() {
        assert_eq!(required(child.parent())?.id(), NodeId(0));
    }
    assert_eq!(required(array.get(4))?.string()?, "abc");
    assert!(array.get(usize::MAX).is_none());
    assert!(doc.node(NodeId(1)).is_none());
    assert!(doc.node(NodeId(2)).is_none());
    assert!(doc.node(NodeId(u32::MAX)).is_none());
    assert_eq!(doc.nodes().count(), doc.node_count());
    Ok(())
}

#[test]
fn extended_keys_strings_and_nested_container_boundaries() -> Result<()> {
    let raw = r#"{"a":[1,{"\u0062":"é"}],"c":null}"#;
    let doc = build::parse_borrowed(raw.as_bytes(), Limits::strict_default())?;
    let object = doc.root_node().object()?;
    assert_eq!(object.len(), 2);
    let a = required(object.get("a"))?;
    let c = required(object.get("c"))?;
    assert_eq!(a.id(), NodeId(4));
    assert_eq!(c.id(), NodeId(20));
    assert_eq!(doc.tape().len(), 22);
    assert_eq!(doc.decoded(), b"b");
    assert!(a.header().has_flag(TAPE_FLAG_HAS_KEY));
    assert!(!a.header().has_flag(TAPE_FLAG_KEY_EXTENDED));
    let nested = required(a.array()?.get(1))?;
    let b = required(nested.object()?.get("b"))?;
    assert_eq!(b.id(), NodeId(15));
    assert!(b.header().has_flag(TAPE_FLAG_KEY_EXTENDED));
    assert!(b.header().has_flag(TAPE_FLAG_EXTENDED));
    assert_eq!(doc.tape()[12].tag(), TAPE_TAG_KEY);
    assert_eq!(doc.tape()[12].record_words(), 3);
    assert_eq!(b.string()?, "é");
    let reference = required(b.string_ref())?;
    assert_eq!(reference.source, SpanSource::RawPayload);
    assert_eq!(reference.scalar_len, 1);
    assert_eq!(
        required(b.key_span())?.slice(doc.raw()),
        Some(&b"\"\\u0062\""[..])
    );
    assert!(doc.node(NodeId(12)).is_none());
    assert!(doc.node(NodeId(13)).is_none());
    assert!(doc.node(NodeId(16)).is_none());
    assert_eq!(
        navigate::raw_span(nested).slice(doc.raw()),
        Some("{\"\\u0062\":\"é\"}".as_bytes())
    );
    Ok(())
}

#[test]
fn string_compact_boundary() -> Result<()> {
    let limits = Limits::permissive_default();
    let mut raw = Vec::new();
    raw.push(b'"');
    raw.resize(TAPE_AUX_LEN_MASK as usize + 1, b'x');
    raw.push(b'"');
    let compact = build::parse_borrowed(&raw, limits)?;
    assert_eq!(compact.tape()[0].record_words(), 2);
    assert_eq!(
        compact.root_node().string()?.len(),
        TAPE_AUX_LEN_MASK as usize
    );
    raw.insert(1, b'x');
    let extended = build::parse_borrowed(&raw, limits)?;
    assert_eq!(extended.tape()[0].record_words(), 4);
    assert_eq!(extended.root_node().raw()?, raw);
    Ok(())
}

#[test]
fn malformed_grammar_and_decoded_duplicates_are_rejected() {
    for raw in [
        "",
        "[1,]",
        "{\"x\":1,}",
        "[1 2]",
        "{\"x\" 1}",
        "{]",
        "[}",
        "{}[]",
        "01",
        "truefalse",
        "[",
        "{\"a\":",
        "\"\\uD800\"",
        "\"\\q\"",
    ] {
        assert!(
            build::parse_borrowed(raw.as_bytes(), Limits::strict_default()).is_err(),
            "{raw}"
        );
    }
    let result = build::parse_borrowed(br#"{"a":0,"\u0061":1}"#, Limits::strict_default());
    assert!(matches!(result, Err(error) if error.code == ErrorCode::DuplicateObjectKey));
}

#[test]
fn duplicate_owner_scope_limits_and_permissive_order() -> Result<()> {
    let doc = build::parse_borrowed(br#"{"a":{"a":1},"b":{"a":2}}"#, Limits::strict_default())?;
    assert_eq!(doc.root_node().object()?.len(), 2);
    let mut limits = Limits::strict_default();
    limits.reject_duplicate_keys = false;
    let duplicate = build::parse_borrowed(br#"{"a":1,"\u0061":2}"#, limits)?;
    let values: Vec<_> = duplicate
        .root_node()
        .object()?
        .iter()
        .map(|member| member.value.u64())
        .collect::<Result<_>>()?;
    assert_eq!(values, [1, 2]);
    limits.max_depth = 0;
    assert!(build::parse_borrowed(b"[]", limits).is_err());
    assert!(build::parse_borrowed(b"0", limits).is_ok());
    limits = Limits::strict_default();
    limits.max_array_items = 0;
    assert!(build::parse_borrowed(b"[]", limits).is_ok());
    assert!(build::parse_borrowed(b"[0]", limits).is_err());
    limits.max_fields_per_object = 0;
    assert!(build::parse_borrowed(b"{}", limits).is_ok());
    assert!(build::parse_borrowed(br#"{"a":0}"#, limits).is_err());
    Ok(())
}

#[test]
fn duplicate_depth_state_survives_children_and_recovers_after_errors() -> Result<()> {
    let mut scratch = DomScratch::new();
    for raw in [
        r#"{"a":{"a":1},"\u0061":2}"#,
        r#"{"\u0061":[{"a":1},{"a":2}],"a":3}"#,
        r#"[{"a":1},{"\u0061":2,"a":3}]"#,
        r#"{"é":{},"\u00e9":0}"#,
    ] {
        let result = build::parse_borrowed_with_scratch(
            raw.as_bytes(),
            Limits::strict_default(),
            &mut scratch,
        );
        assert!(matches!(result, Err(error) if error.code == ErrorCode::DuplicateObjectKey));
        // Reuse different owners/decoded offsets after every failed parse.
        let doc = build::parse_borrowed_with_scratch(
            br#"[{"a":1},{"\u0061":2},{"other":3}]"#,
            Limits::strict_default(),
            &mut scratch,
        )?;
        assert_eq!(doc.root_node().array()?.len(), 3);
        doc.recycle(&mut scratch);
    }
    Ok(())
}

#[test]
fn unescaped_token_views_borrow_raw_quotes_for_compact_and_extended_records() -> Result<()> {
    let input = r#"{"ascii":"plain","é":"🦀","\u0061":"\n"}"#;
    let doc = build::parse_borrowed(input.as_bytes(), Limits::strict_default())?;
    assert!(doc.root_node().unescaped_string_token().is_none());
    assert!(doc.root_node().unescaped_key_token().is_none());
    let members: Vec<_> = doc.root_node().object()?.iter().collect();
    for member in &members[..2] {
        let node = member.value;
        let key_span = required(node.key_span())?;
        let key_token = required(node.unescaped_key_token())?;
        let string_token = required(node.unescaped_string_token())?;
        assert_eq!(
            key_token.as_ptr(),
            input.as_bytes()[key_span.offset as usize..].as_ptr()
        );
        assert_eq!(
            string_token.as_ptr(),
            input.as_bytes()[node.raw_span().offset as usize..].as_ptr()
        );
        assert_eq!(
            &string_token[1..string_token.len() - 1],
            node.string()?.as_bytes()
        );
    }
    assert!(members[2].value.unescaped_key_token().is_none());
    assert!(members[2].value.unescaped_string_token().is_none());
    Ok(())
}

#[test]
fn scratch_owned_and_borrowed_reuse_after_success_and_failure() -> Result<()> {
    let raw = br#"{"a":[1,2,3],"b":"x\ny"}"#;
    let payload = Payload::from_slice(raw);
    let mut scratch = DomScratch::new();
    let doc = build::parse_into(payload.clone(), Limits::strict_default(), &mut scratch)?;
    let tape_ptr = doc.tape().as_ptr();
    let decoded_ptr = doc.decoded().as_ptr();
    assert!(doc.payload().shares_backing_with(&payload));
    let doc = build::parse_into(payload, Limits::strict_default(), &mut scratch)?;
    assert_eq!(doc.tape().as_ptr(), tape_ptr);
    assert_eq!(doc.decoded().as_ptr(), decoded_ptr);
    assert!(
        build::parse_into(
            Payload::from_slice(b"[\"\\q\"]"),
            Limits::strict_default(),
            &mut scratch
        )
        .is_err()
    );
    assert!(scratch.document().is_none());
    let doc = build::parse_borrowed_with_scratch(raw, Limits::strict_default(), &mut scratch)?;
    assert_eq!(doc.tape().as_ptr(), tape_ptr);
    assert_eq!(doc.decoded().as_ptr(), decoded_ptr);
    let borrowed_ptr = doc.raw().as_ptr();
    doc.recycle(&mut scratch);
    let doc = build::parse_borrowed_with_scratch(raw, Limits::strict_default(), &mut scratch)?;
    assert_eq!(doc.tape().as_ptr(), tape_ptr);
    assert_eq!(doc.raw().as_ptr(), borrowed_ptr);
    assert!(doc.capability().matches_slice(
        raw,
        doc.decoded().len(),
        0,
        Limits::strict_default().policy_fingerprint()
    ));
    Ok(())
}

#[test]
fn explicit_legacy_conversion_reindexes_nodes_and_preserves_spelling() -> Result<()> {
    let payload = Payload::from_slice(br#" {"a":[1,{"\u0062":"\n"}],"c":-0.0e+1} "#);
    let native = build::parse(payload.clone(), Limits::strict_default())?;
    let legacy = native.to_legacy()?;
    legacy.validate_layout()?;
    assert!(legacy.payload.shares_backing_with(&payload));
    assert_eq!(legacy.parts.nodes.len(), native.node_count());
    assert_eq!(legacy.parts.containers.len(), native.container_count());
    let roundtrip = build::from_validated(legacy, Limits::strict_default())?;
    assert_eq!(roundtrip.tape(), native.tape());
    assert_eq!(roundtrip.decoded(), native.decoded());
    let decoded_ptr = native.decoded().as_ptr();
    let moved = native.into_legacy()?;
    assert_eq!(moved.parts.decoded.as_ptr(), decoded_ptr);
    moved.validate_layout()?;
    let mut bad = moved.clone();
    bad.parts.spans.clear();
    assert!(build::from_validated(bad, Limits::strict_default()).is_err());
    Ok(())
}

#[derive(Default)]
struct Sink(Vec<u8>);
impl ByteSink for Sink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        self.0.try_reserve(additional).map_err(|_| {
            Error::new(
                ErrorCode::BudgetExhausted,
                ErrorSource::Encode,
                0,
                "test sink allocation failed",
            )
        })
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}
#[test]
fn rebuild_uses_tape_ids_and_retains_existing_key_spelling() -> Result<()> {
    let doc = build::parse(
        Payload::from_slice(br#" {"\u0061":[1, 2],"z":3} "#),
        Limits::strict_default(),
    )?;
    let a = required(doc.root_node().object()?.get("a"))?;
    let first = required(a.array()?.get(0))?;
    let changed = rebuild::apply(
        &doc,
        &[Edit {
            target: first.id(),
            action: EditAction::Replace(Payload::from_slice(b"true")),
        }],
        Limits::strict_default(),
    )?;
    assert_eq!(changed.raw(), br#"{"\u0061":[true,2],"z":3}"#);
    let mut sink = Sink::default();
    rebuild::write_document(&changed, &mut sink)?;
    assert_eq!(sink.0, changed.raw());
    let compact = rebuild::compact(&doc, Limits::strict_default())?;
    assert_eq!(compact.raw(), br#"{"\u0061":[1,2],"z":3}"#);
    assert_eq!(compact.root_node().kind(), NodeKind::Object);
    Ok(())
}

#[test]
fn arbitrary_strrefs_remain_checked_and_scalar_counts_are_not_recomputed() -> Result<()> {
    let doc: BorrowedDocument<'_> =
        build::parse_borrowed("\"é\"".as_bytes(), Limits::strict_default())?;
    let invalid = StrRef {
        span: Span32::new(2, 1),
        source: SpanSource::RawPayload,
        scalar_len: 1,
    };
    assert!(doc.resolve_trusted_str(invalid).is_err());
    let valid = required(doc.root_node().string_ref())?;
    assert_eq!(doc.resolve_trusted_str(valid)?, "é");
    Ok(())
}

#[test]
fn numeric_views_preserve_spelling_and_apply_explicit_policy() -> Result<()> {
    let doc = build::parse_borrowed(
        b"[-0,1.25,18446744073709551615,9223372036854775808]",
        Limits::strict_default(),
    )?;
    let array = doc.root_node().array()?;
    let zero = required(array.get(0))?;
    assert_eq!(zero.i64()?, 0);
    assert_eq!(zero.raw()?, b"-0");
    assert!(zero.u64().is_err());
    let fractional = required(array.get(1))?;
    assert_eq!(fractional.f64()?, 1.25);
    assert!(matches!(fractional.i64(), Err(error) if error.code == ErrorCode::InvalidNumber));
    assert_eq!(required(array.get(2))?.u64()?, u64::MAX);
    assert!(
        matches!(required(array.get(3))?.i64(), Err(error) if error.code == ErrorCode::NumberOverflow)
    );
    let numeric = jmax_abi::number::NumericPolicy::strict_default();
    assert!(build::parse_borrowed_with_numeric(b"-0", Limits::strict_default(), numeric).is_err());
    assert!(
        build::parse_borrowed_with_numeric(b"1e309", Limits::strict_default(), numeric).is_err()
    );
    Ok(())
}

#[test]
fn owned_subview_and_clone_capability_keep_the_exact_owner() -> Result<()> {
    let backing = Payload::from_slice(b"prefix [1,2] suffix");
    let input = backing.slice(7..12)?;
    let doc = build::parse(input.clone(), Limits::strict_default())?;
    assert_eq!(doc.raw().as_ptr(), input.as_slice().as_ptr());
    assert!(doc.payload().shares_backing_with(&backing));
    let clone = doc.clone();
    for value in [&doc, &clone] {
        assert!(value.capability().matches_payload(
            value.payload(),
            value.decoded().len(),
            0,
            Limits::strict_default().policy_fingerprint()
        ));
        assert!(!value.capability().matches_payload(
            &backing,
            value.decoded().len(),
            0,
            Limits::strict_default().policy_fingerprint()
        ));
    }
    let borrowed = build::parse_borrowed(b"[3,4]", Limits::strict_default())?;
    let mut sink = Sink::default();
    rebuild::write_document(&borrowed, &mut sink)?;
    assert_eq!(sink.0, b"[3,4]");
    Ok(())
}

#[test]
fn key_compact_boundary_and_unicode_key_reference() -> Result<()> {
    let mut raw = b"{\"".to_vec();
    raw.resize(TAPE_AUX_LEN_MASK as usize + 2, b'x');
    raw.extend_from_slice(b"\":null}");
    let compact = build::parse_borrowed(&raw, Limits::permissive_default())?;
    let member = required(compact.root_node().object()?.iter().next())?;
    assert_eq!(compact.tape()[3].record_words(), 1);
    assert_eq!(member.value.id(), NodeId(4));
    assert_eq!(member.key.len(), TAPE_AUX_LEN_MASK as usize);
    raw.insert(2, b'x');
    let extended = build::parse_borrowed(&raw, Limits::permissive_default())?;
    let member = required(extended.root_node().object()?.iter().next())?;
    assert_eq!(extended.tape()[3].record_words(), 3);
    assert_eq!(member.value.id(), NodeId(6));
    assert!(member.value.header().has_flag(TAPE_FLAG_KEY_EXTENDED));
    assert_eq!(member.key.len(), TAPE_AUX_LEN_MASK as usize + 1);
    let unicode = build::parse_borrowed(
        r#"{"é":"\uD834\uDD1E"}"#.as_bytes(),
        Limits::strict_default(),
    )?;
    let member = required(unicode.root_node().object()?.iter().next())?;
    assert_eq!(member.key, "é");
    assert_eq!(
        required(member.value.key_ref())?.source,
        SpanSource::RawPayload
    );
    assert_eq!(
        required(member.value.string_ref())?.source,
        SpanSource::DecodedArena
    );
    assert_eq!(member.value.string()?, "𝄞");
    assert_eq!(required(member.value.string_ref())?.scalar_len, 1);
    Ok(())
}

#[test]
fn duplicate_table_resize_and_nested_scope() -> Result<()> {
    let mut raw = String::from("{");
    for index in 0..256 {
        if index != 0 {
            raw.push(',');
        }
        raw.push_str(&format!("\"key{index}\":{{\"same\":{index}}}"));
    }
    raw.push('}');
    let doc = build::parse_borrowed(raw.as_bytes(), Limits::strict_default())?;
    assert_eq!(doc.root_node().object()?.len(), 256);
    let key = required(doc.root_node().object()?.get("key255"))?;
    assert_eq!(required(key.object()?.get("same"))?.u64()?, 255);
    raw.pop();
    raw.push_str(",\"\\u006bey0\":0}");
    assert!(
        matches!(build::parse_borrowed(raw.as_bytes(), Limits::strict_default()),
        Err(error) if error.code == ErrorCode::DuplicateObjectKey)
    );
    Ok(())
}
