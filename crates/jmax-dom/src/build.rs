//! Strict, iterative construction of the DOM's preorder node tables.

use super::*;
use jmax_cursor::{Cursor, Event, ScalarKind, ScanStep, SkipMode};

/// Parses one complete JSON value, including validation of trailing input.
///
/// Numbers retain their exact spelling. Object keys and string values retain
/// the cursor's owner-relative references without decoding or rebasing them.
pub fn parse(payload: Payload, limits: Limits) -> Result<Document> {
    let mut cursor = jmax_cursor::walk::from_payload(payload, limits, SkipMode::Strict)?;
    let mut builder = Builder::new();
    loop {
        let before = cursor.state().byte_offset;
        match jmax_cursor::walk::next(&mut cursor)? {
            ScanStep::Emit(event) => {
                let after = cursor.state().byte_offset;
                builder.event(event, &cursor, before, after)?;
            }
            ScanStep::Finished => break,
            ScanStep::NeedMore => return Err(dom_error(
                ErrorCode::InternalError, cursor.state().byte_offset,
                "final DOM input unexpectedly requested more bytes",
            )),
        }
    }
    let offset = cursor.state().byte_offset;
    builder.complete(offset)?;
    let output = jmax_cursor::resume::finish(cursor)?;
    if !output.fully_validated {
        return Err(dom_error(ErrorCode::InternalError, offset,
            "DOM cursor finalization did not validate the complete input"));
    }
    builder.parts.decoded = output.decoded;
    Ok(Document {
        payload: output.payload,
        parts: builder.parts,
        metadata: builder.metadata,
    })
}

/// Reparses the payload under the supplied limits, discarding foreign tables
/// and schema provenance rather than assuming they describe this DOM layout.
pub fn from_validated(document: ValidatedDocument, limits: Limits) -> Result<Document> {
    let ValidatedDocument { payload, parts, proof: _ } = document;
    drop(parts);
    parse(payload, limits)
}

#[derive(Clone, Copy, Debug)]
struct OpenContainer {
    node: NodeId,
    record: u32,
    tag: u8,
    last_child: Option<NodeId>,
    key: Option<(StrRef, Span32)>,
}

struct Builder {
    parts: DocumentParts,
    metadata: Vec<NodeMeta>,
    stack: Vec<OpenContainer>,
}

impl Builder {
    fn new() -> Self {
        Self { parts: DocumentParts::default(), metadata: Vec::new(), stack: Vec::new() }
    }

    fn event(&mut self, event: Event, cursor: &Cursor, before: u32, after: u32) -> Result<()> {
        if after < before || after as usize > cursor.raw_bytes().len() {
            return Err(dom_error(ErrorCode::InternalError, after,
                "DOM cursor committed an invalid input range"));
        }
        match event {
            Event::Key { str_ref } => {
                validate_text(cursor, str_ref, after)?;
                let span = key_span(cursor.raw_bytes(), before, after)?;
                let frame = self.stack.last_mut().ok_or_else(|| dom_error(
                    ErrorCode::InternalError, after, "DOM key has no parent object"))?;
                if frame.tag != TAG_OBJECT || frame.key.is_some() {
                    return Err(dom_error(ErrorCode::InternalError, after,
                        "DOM key is outside an available object key position"));
                }
                frame.key = Some((str_ref, span));
                Ok(())
            }
            Event::ObjectStart { offset } => {
                self.start(TAG_OBJECT, offset, cursor.raw_bytes(), before, after)
            }
            Event::ArrayStart { offset } => {
                self.start(TAG_ARRAY, offset, cursor.raw_bytes(), before, after)
            }
            Event::ObjectEnd { offset } => {
                self.end(TAG_OBJECT, offset, cursor.raw_bytes(), before, after)
            }
            Event::ArrayEnd { offset } => {
                self.end(TAG_ARRAY, offset, cursor.raw_bytes(), before, after)
            }
            Event::Scalar { kind, span, decoded } => {
                if span.is_empty() || span.offset < before || span.end() != Some(after)
                    || span.slice(cursor.raw_bytes()).is_none() {
                    return Err(dom_error(ErrorCode::InternalError, after,
                        "DOM scalar has an invalid raw span"));
                }
                let tag = match kind {
                    ScalarKind::Null => TAG_NULL,
                    ScalarKind::False => TAG_FALSE,
                    ScalarKind::True => TAG_TRUE,
                    ScalarKind::Number => TAG_NUMBER,
                    ScalarKind::String => TAG_STRING,
                };
                match (kind, decoded) {
                    (ScalarKind::String, Some(reference)) => validate_text(cursor, reference, after)?,
                    (ScalarKind::String, None) => return Err(dom_error(
                        ErrorCode::InternalError, after, "DOM string has no cooked reference")),
                    (_, Some(_)) => return Err(dom_error(
                        ErrorCode::InternalError, after, "DOM non-string has a cooked reference")),
                    (_, None) => {}
                }
                self.append(tag, span, decoded, None, after)?;
                Ok(())
            }
        }
    }

    fn start(&mut self, tag: u8, offset: u32, raw: &[u8], before: u32, after: u32) -> Result<()> {
        delimiter(raw, offset, before, after, if tag == TAG_OBJECT { b'{' } else { b'[' })?;
        let record = available_index(self.parts.containers.len(), after)?;
        reserve(&mut self.parts.containers, 1, after)?;
        reserve(&mut self.stack, 1, after)?;
        let node = self.append(tag, Span32::new(offset, 1), None, Some(record), after)?;
        self.parts.containers.push(Container32 {
            first_child: NO_NODE, child_count: 0, end_node: 0, reserved: 0,
        });
        self.stack.push(OpenContainer { node, record, tag, last_child: None, key: None });
        Ok(())
    }

    fn append(
        &mut self,
        tag: u8,
        span: Span32,
        string: Option<StrRef>,
        record: Option<u32>,
        offset: u32,
    ) -> Result<NodeId> {
        let id = NodeId(available_index(self.parts.nodes.len(), offset)?);
        if self.parts.spans.len() != self.parts.nodes.len()
            || self.metadata.len() != self.parts.nodes.len() {
            return Err(dom_error(ErrorCode::InternalError, offset,
                "DOM parallel construction tables have different lengths"));
        }
        let (parent, key, key_span) = match self.stack.last() {
            Some(frame) => {
                let key = match (frame.tag, frame.key) {
                    (TAG_OBJECT, Some(key)) => Some(key),
                    (TAG_ARRAY, None) => None,
                    _ => return Err(dom_error(ErrorCode::InternalError, offset,
                        "DOM parent has no available value position")),
                };
                (Some(frame.node), key.map(|entry| entry.0), key.map(|entry| entry.1))
            }
            None if id.0 == 0 => (None, None, None),
            None => return Err(dom_error(ErrorCode::InternalError, offset,
                "DOM received more than one root value")),
        };
        reserve(&mut self.parts.nodes, 1, offset)?;
        reserve(&mut self.parts.spans, 1, offset)?;
        reserve(&mut self.metadata, 1, offset)?;
        if let Some(frame) = self.stack.last_mut() {
            let container = self.parts.containers.get_mut(frame.record as usize)
                .ok_or_else(|| dom_error(ErrorCode::InternalError, offset,
                    "DOM parent container record is missing"))?;
            let count = container.child_count.checked_add(1).ok_or_else(|| dom_error(
                ErrorCode::PayloadTooLarge, offset, "DOM child count exceeds u32"))?;
            if let Some(previous) = frame.last_child {
                let metadata = self.metadata.get_mut(previous.0 as usize).ok_or_else(|| dom_error(
                    ErrorCode::InternalError, offset, "DOM preceding sibling is missing"))?;
                if metadata.parent != parent || metadata.next_sibling.is_some() {
                    return Err(dom_error(ErrorCode::InternalError, offset,
                        "DOM preceding sibling has inconsistent links"));
                }
                metadata.next_sibling = Some(id);
            } else {
                if container.child_count != 0 || container.first_child != NO_NODE {
                    return Err(dom_error(ErrorCode::InternalError, offset,
                        "DOM empty parent has inconsistent child links"));
                }
                container.first_child = id.0;
            }
            container.child_count = count;
            frame.last_child = Some(id);
            frame.key = None;
        }
        self.parts.nodes.push(NodeWord::pack(tag, 0, record.unwrap_or(id.0)));
        self.parts.spans.push(span);
        self.metadata.push(NodeMeta {
            parent, next_sibling: None, key, key_span, string, number_index: None,
        });
        Ok(id)
    }

    fn end(&mut self, tag: u8, offset: u32, raw: &[u8], before: u32, after: u32) -> Result<()> {
        delimiter(raw, offset, before, after, if tag == TAG_OBJECT { b'}' } else { b']' })?;
        let frame = self.stack.last().copied().ok_or_else(|| dom_error(
            ErrorCode::InternalError, after, "DOM closing delimiter has no open container"))?;
        if frame.tag != tag || frame.key.is_some() {
            return Err(dom_error(ErrorCode::InternalError, after,
                "DOM closing delimiter does not match a complete container"));
        }
        let end_node = u32::try_from(self.parts.nodes.len()).map_err(|_| dom_error(
            ErrorCode::PayloadTooLarge, after, "DOM exclusive subtree end exceeds u32"))?;
        let span = self.parts.spans.get_mut(frame.node.0 as usize).ok_or_else(|| dom_error(
            ErrorCode::InternalError, after, "DOM open container span is missing"))?;
        if offset <= span.offset {
            return Err(dom_error(ErrorCode::InternalError, after,
                "DOM closing delimiter precedes its opening delimiter"));
        }
        let len = after.checked_sub(span.offset).ok_or_else(|| dom_error(
            ErrorCode::InternalError, after, "DOM container span is reversed"))?;
        let container = self.parts.containers.get_mut(frame.record as usize).ok_or_else(|| dom_error(
            ErrorCode::InternalError, after, "DOM closing container record is missing"))?;
        span.len = len;
        container.end_node = end_node;
        self.stack.pop();
        Ok(())
    }

    fn complete(&self, offset: u32) -> Result<()> {
        if self.parts.nodes.is_empty() || !self.stack.is_empty()
            || self.parts.nodes.len() != self.parts.spans.len()
            || self.parts.nodes.len() != self.metadata.len() {
            return Err(dom_error(ErrorCode::InternalError, offset,
                "DOM traversal finished with an incomplete construction"));
        }
        Ok(())
    }
}

/// The maximum u32 value is an exclusive end or sentinel, never a node ID.
fn available_index(len: usize, offset: u32) -> Result<u32> {
    match u32::try_from(len) {
        Ok(index) if index != NO_NODE => Ok(index),
        _ => Err(dom_error(ErrorCode::PayloadTooLarge, offset,
            "DOM table index exceeds the representable range")),
    }
}

fn reserve<T>(values: &mut Vec<T>, additional: usize, offset: u32) -> Result<()> {
    values.try_reserve(additional).map_err(|_| dom_error(ErrorCode::BudgetExhausted, offset,
        "DOM construction allocation failed"))
}

fn validate_text(cursor: &Cursor, reference: StrRef, offset: u32) -> Result<()> {
    let text = cursor.resolve_str(reference).ok_or_else(|| dom_error(
        ErrorCode::InternalError, offset, "DOM cooked string reference is invalid"))?;
    if text.chars().count() != reference.scalar_len as usize {
        return Err(dom_error(ErrorCode::InternalError, offset,
            "DOM cooked string scalar count is inconsistent"));
    }
    Ok(())
}

fn delimiter(raw: &[u8], offset: u32, before: u32, after: u32, expected: u8) -> Result<()> {
    if offset < before || offset.checked_add(1) != Some(after)
        || raw.get(offset as usize) != Some(&expected) {
        return Err(dom_error(ErrorCode::InternalError, after,
            "DOM container event has an invalid delimiter offset"));
    }
    Ok(())
}

/// Key events omit raw spans. Only scan the retained, committed event range,
/// skipping the whitespace and comma that can precede an object key.
fn key_span(raw: &[u8], before: u32, after: u32) -> Result<Span32> {
    let range = after.checked_sub(before).and_then(|len| Span32::new(before, len).slice(raw))
        .ok_or_else(|| dom_error(
        ErrorCode::InternalError, after, "DOM key committed range is invalid"))?;
    let skipped = range.iter().take_while(|byte| matches!(**byte,
        b' ' | b'\t' | b'\r' | b'\n' | b',')).count();
    let token = range.get(skipped..).ok_or_else(|| dom_error(
        ErrorCode::InternalError, after, "DOM key prefix is invalid"))?;
    if token.len() < 2 || token.first() != Some(&b'"') || token.last() != Some(&b'"') {
        return Err(dom_error(ErrorCode::InternalError, after,
            "DOM key event does not cover a complete quoted token"));
    }
    let skipped = u32::try_from(skipped).map_err(|_| dom_error(
        ErrorCode::PayloadTooLarge, after, "DOM key prefix exceeds u32"))?;
    let start = before.checked_add(skipped).ok_or_else(|| dom_error(
        ErrorCode::PayloadTooLarge, after, "DOM key start exceeds u32"))?;
    Ok(Span32::new(start, after - start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(raw: &[u8]) -> Document {
        parse(Payload::from_slice(raw), Limits::strict_default()).unwrap()
    }

    fn raw(document: &Document, node: usize) -> &[u8] {
        document.parts.spans[node].slice(document.payload.as_slice()).unwrap()
    }

    fn assert_layout(document: &Document) {
        let count = document.node_count();
        assert!(count > 0);
        assert_eq!(document.parts.spans.len(), count);
        assert_eq!(document.metadata.len(), count);
        assert!(document.parts.numbers.is_empty());
        for (index, word) in document.parts.nodes.iter().copied().enumerate() {
            assert_eq!(word.aux24(), 0);
            let meta = document.metadata[index];
            assert!(meta.number_index.is_none());
            assert_eq!(meta.string.is_some(), word.kind() == TAG_STRING);
            assert!(!raw(document, index).is_empty());
            if let Some(reference) = meta.string {
                let text = document.resolve_str(reference).unwrap();
                assert_eq!(text.chars().count(), reference.scalar_len as usize);
            }
            if index == 0 {
                assert_eq!((meta.parent, meta.next_sibling, meta.key, meta.key_span),
                    (None, None, None, None));
            } else {
                let parent = meta.parent.unwrap().0 as usize;
                assert!(parent < index);
                let parent_tag = document.parts.nodes[parent].kind();
                assert!(matches!(parent_tag, TAG_ARRAY | TAG_OBJECT));
                assert_eq!(meta.key.is_some(), parent_tag == TAG_OBJECT);
                assert_eq!(meta.key_span.is_some(), parent_tag == TAG_OBJECT);
                if let Some(reference) = meta.key {
                    let text = document.resolve_str(reference).unwrap();
                    assert_eq!(text.chars().count(), reference.scalar_len as usize);
                    let key_raw = meta.key_span.unwrap().slice(document.payload.as_slice()).unwrap();
                    assert_eq!(key_raw.first(), Some(&b'"'));
                    assert_eq!(key_raw.last(), Some(&b'"'));
                }
            }
            if matches!(word.kind(), TAG_ARRAY | TAG_OBJECT) {
                let record = document.parts.containers[word.payload_index() as usize];
                assert_eq!(record.reserved, 0);
                assert!(record.end_node as usize > index);
                assert!(record.end_node as usize <= count);
                if record.child_count == 0 {
                    assert_eq!(record.first_child, NO_NODE);
                    assert_eq!(record.end_node as usize, index + 1);
                } else {
                    assert_eq!(record.first_child as usize, index + 1);
                }
                let mut next = if record.child_count == 0 { None } else { Some(NodeId(record.first_child)) };
                let mut subtree_end = index + 1;
                for _ in 0..record.child_count {
                    let child = next.unwrap().0 as usize;
                    assert_eq!(child, subtree_end);
                    assert_eq!(document.metadata[child].parent, Some(NodeId(index as u32)));
                    let child_word = document.parts.nodes[child];
                    subtree_end = if matches!(child_word.kind(), TAG_ARRAY | TAG_OBJECT) {
                        document.parts.containers[child_word.payload_index() as usize].end_node as usize
                    } else { child + 1 };
                    next = document.metadata[child].next_sibling;
                }
                assert_eq!(next, None);
                assert_eq!(subtree_end, record.end_node as usize);
            } else {
                assert!(NodeKind::from_tag(word.kind()).is_some());
                assert_eq!(word.payload_index() as usize, index);
            }
        }
    }

    #[test]
    fn scalar_roots_retain_exact_spans_and_tags() {
        for (input, expected, tag) in [
            (&b" \tnull\r\n"[..], &b"null"[..], TAG_NULL),
            (&b"false"[..], &b"false"[..], TAG_FALSE),
            (&b"true "[..], &b"true"[..], TAG_TRUE),
            (&b" -0 "[..], &b"-0"[..], TAG_NUMBER),
            (&b"1.2300e+0042"[..], &b"1.2300e+0042"[..], TAG_NUMBER),
            (&b"\"\""[..], &b"\"\""[..], TAG_STRING),
        ] {
            let doc = document(input);
            assert_layout(&doc);
            assert_eq!(doc.node_count(), 1);
            assert_eq!(doc.parts.nodes[0], NodeWord::pack(tag, 0, 0));
            assert_eq!(raw(&doc, 0), expected);
            assert_eq!(doc.payload.as_slice(), input);
            assert!(doc.parts.containers.is_empty());
        }
    }

    #[test]
    fn nested_preorder_links_skip_complete_subtrees() {
        let doc = document(br#" { "a": [true, {"b": null}, []], "c": {}, "d": false } "#);
        assert_layout(&doc);
        assert_eq!(doc.parts.nodes.iter().map(|word| word.kind()).collect::<Vec<_>>(),
            [TAG_OBJECT, TAG_ARRAY, TAG_TRUE, TAG_OBJECT, TAG_NULL, TAG_ARRAY, TAG_OBJECT, TAG_FALSE]);
        assert_eq!(doc.parts.containers, [
            Container32 { first_child: 1, child_count: 3, end_node: 8, reserved: 0 },
            Container32 { first_child: 2, child_count: 3, end_node: 6, reserved: 0 },
            Container32 { first_child: 4, child_count: 1, end_node: 5, reserved: 0 },
            Container32 { first_child: NO_NODE, child_count: 0, end_node: 6, reserved: 0 },
            Container32 { first_child: NO_NODE, child_count: 0, end_node: 7, reserved: 0 },
        ]);
        assert_eq!(doc.metadata[1].next_sibling, Some(NodeId(6)));
        assert_eq!(doc.metadata[3].next_sibling, Some(NodeId(5)));
        assert_eq!(doc.metadata[4].next_sibling, None);
        assert_eq!(raw(&doc, 1), br#"[true, {"b": null}, []]"#);
        assert_eq!(raw(&doc, 0), br#"{ "a": [true, {"b": null}, []], "c": {}, "d": false }"#);
    }

    #[test]
    fn empty_containers_and_empty_keys_have_distinct_valid_spans() {
        for input in [&b"[]"[..], &b"{}"[..], &b"[ {}, [ ], {} ]"[..], &br#"{"": ""}"#[..]] {
            assert_layout(&document(input));
        }
        let doc = document(br#"{"": ""}"#);
        assert_eq!(doc.resolve_str(doc.metadata[1].key.unwrap()), Some(""));
        assert_eq!(doc.metadata[1].key_span, Some(Span32::new(1, 2)));
        assert_eq!(doc.resolve_str(doc.metadata[1].string.unwrap()), Some(""));
        assert_eq!(raw(&doc, 1), b"\"\"");
    }

    #[test]
    fn key_spans_follow_committed_raw_bytes_and_arena_offsets_do_not_rebase() {
        let input = "{ \"\\u0061\" : \"\\n\", \n\t\"é\" : \"𝄞\", \"\\uD834\\uDD1E\":\"x\\t\\u0062\" }";
        let doc = document(input.as_bytes());
        assert_layout(&doc);
        let keys = ["a", "é", "𝄞"];
        let values = ["\n", "𝄞", "x\tb"];
        let raw_keys: [&[u8]; 3] = [br#""\u0061""#, "\"é\"".as_bytes(), br#""\uD834\uDD1E""#];
        for index in 1..4 {
            let meta = doc.metadata[index];
            assert_eq!(doc.resolve_str(meta.key.unwrap()), Some(keys[index - 1]));
            assert_eq!(doc.resolve_str(meta.string.unwrap()), Some(values[index - 1]));
            assert_eq!(meta.key_span.unwrap().slice(doc.payload.as_slice()).unwrap(), raw_keys[index - 1]);
        }
        assert_eq!(doc.metadata[1].key.unwrap().source, SpanSource::DecodedArena);
        assert_eq!(doc.metadata[2].key.unwrap().source, SpanSource::RawPayload);
        assert_eq!(doc.metadata[2].string.unwrap().source, SpanSource::RawPayload);
        assert_eq!(doc.metadata[3].key.unwrap().scalar_len, 1);
        let mut cursor = jmax_cursor::walk::from_payload(
            Payload::from_slice(input.as_bytes()), Limits::strict_default(), SkipMode::Strict).unwrap();
        let mut index = 1;
        loop {
            match jmax_cursor::walk::next(&mut cursor).unwrap() {
                ScanStep::Emit(Event::Key { str_ref }) => assert_eq!(doc.metadata[index].key, Some(str_ref)),
                ScanStep::Emit(Event::Scalar { decoded, .. }) => {
                    assert_eq!(doc.metadata[index].string, decoded);
                    index += 1;
                }
                ScanStep::Emit(_) => {}
                ScanStep::Finished => break,
                ScanStep::NeedMore => panic!("complete input requested more bytes"),
            }
        }
        assert_eq!(doc.parts.decoded, cursor.decoded_bytes());
        let clone = doc.clone();
        drop(doc);
        assert_eq!(clone.resolve_str(clone.metadata[3].string.unwrap()), Some("x\tb"));
    }

    #[test]
    fn huge_numbers_remain_lexical_without_conversion_or_cache() {
        let input = b"[18446744073709551616,-9223372036854775809,1e9999,1e-9999,-0.000000]";
        let doc = document(input);
        assert_layout(&doc);
        for (index, spelling) in [
            &b"18446744073709551616"[..], &b"-9223372036854775809"[..],
            &b"1e9999"[..], &b"1e-9999"[..], &b"-0.000000"[..],
        ].iter().enumerate() {
            assert_eq!(raw(&doc, index + 1), *spelling);
            assert_eq!(doc.parts.nodes[index + 1].kind(), TAG_NUMBER);
        }
    }

    #[test]
    fn escaped_controls_quotes_and_backslashes_preserve_raw_and_cooked_text() {
        let input = br#"{"a\"b\\c":"\"\\\/\b\f\n\r\t\u0000\u001f"}"#;
        let doc = document(input);
        assert_layout(&doc);
        let meta = doc.metadata[1];
        assert_eq!(doc.resolve_str(meta.key.unwrap()), Some("a\"b\\c"));
        assert_eq!(doc.resolve_str(meta.string.unwrap()), Some("\"\\/\u{8}\u{c}\n\r\t\0\u{1f}"));
        assert_eq!(meta.key_span.unwrap().slice(doc.payload.as_slice()), Some(&br#""a\"b\\c""#[..]));
        assert_eq!(raw(&doc, 1), br#""\"\\\/\b\f\n\r\t\u0000\u001f""#);
        assert_eq!(meta.key.unwrap().scalar_len, 5);
        assert_eq!(meta.string.unwrap().scalar_len, 10);
    }

    #[test]
    fn duplicate_policy_compares_cooked_keys_and_preserves_allowed_members() {
        let input = br#"{"a":0,"\u0061":1,"a":2}"#;
        let error = parse(Payload::from_slice(input), Limits::strict_default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
        assert_eq!(error.byte_offset, 7);
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        let doc = parse(Payload::from_slice(input), limits).unwrap();
        assert_layout(&doc);
        assert_eq!(doc.parts.containers[0].child_count, 3);
        for index in 1..4 {
            assert_eq!(doc.resolve_str(doc.metadata[index].key.unwrap()), Some("a"));
        }
        assert_eq!(doc.metadata[2].key_span.unwrap().slice(doc.payload.as_slice()), Some(&br#""\u0061""#[..]));
    }

    #[test]
    fn syntax_failures_are_propagated_with_their_original_diagnostics() {
        let inputs: &[&[u8]] = &[
            b"", b" \t", b"[", b"{\"x\":}", b"[1,]", b"{\"x\":1,}",
            b"true false", b"[]x", b"null\x00", b"01", b"+1", b"1.", b"1e+",
            b"\"unterminated", br#""\q""#, br#""\uD800""#, b"\"\xff\"", b"\"\x01\"",
        ];
        for &input in inputs {
            let limits = Limits::strict_default();
            let mut cursor = jmax_cursor::walk::from_payload(Payload::from_slice(input), limits, SkipMode::Strict).unwrap();
            let expected = loop {
                match jmax_cursor::walk::next(&mut cursor) {
                    Err(error) => break error,
                    Ok(ScanStep::Emit(_)) => {}
                    other => panic!("invalid vector unexpectedly completed: {other:?}"),
                }
            };
            assert_eq!(parse(Payload::from_slice(input), limits).unwrap_err(), expected);
        }
        let trailing = parse(Payload::from_slice(b"[0] \n true"), Limits::strict_default()).unwrap_err();
        assert_eq!(trailing.code, ErrorCode::TrailingContent);
        assert_eq!(trailing.byte_offset, 6);
    }

    #[test]
    fn inclusive_limits_and_zero_resource_boundaries_are_enforced() {
        let defaults = Limits::strict_default();
        let cases: &[(&[u8], &[u8], Limits, ErrorCode)] = &[
            (b"null", b"null ", Limits { max_payload_bytes: 4, ..defaults }, ErrorCode::PayloadTooLarge),
            (b"[]", b"[[]]", Limits { max_depth: 1, ..defaults }, ErrorCode::DepthLimitExceeded),
            (b"null", b"[]", Limits { max_depth: 0, ..defaults }, ErrorCode::DepthLimitExceeded),
            (b"[0]", b"[0,1]", Limits { max_array_items: 1, ..defaults }, ErrorCode::ConstraintViolation),
            (b"[]", b"[0]", Limits { max_array_items: 0, ..defaults }, ErrorCode::ConstraintViolation),
            (br#"{"a":0}"#, br#"{"a":0,"b":1}"#, Limits { max_fields_per_object: 1, ..defaults }, ErrorCode::ConstraintViolation),
            (b"{}", br#"{"a":0}"#, Limits { max_fields_per_object: 0, ..defaults }, ErrorCode::ConstraintViolation),
            (b"12", b"123", Limits { max_number_digits: 2, ..defaults }, ErrorCode::ConstraintViolation),
            (b"null", b"0", Limits { max_number_digits: 0, ..defaults }, ErrorCode::ConstraintViolation),
            (br#""ab""#, br#""abc""#, Limits { max_string_bytes: 2, ..defaults }, ErrorCode::ConstraintViolation),
            (br#""""#, br#""a""#, Limits { max_string_bytes: 0, ..defaults }, ErrorCode::ConstraintViolation),
            (br#""\n""#, br#"["\n","\t"]"#, Limits { max_decoded_bytes: 1, ..defaults }, ErrorCode::DecodedArenaOverflow),
            (br#""raw""#, br#""\n""#, Limits { max_decoded_bytes: 0, ..defaults }, ErrorCode::DecodedArenaOverflow),
        ];
        for &(valid, invalid, limits, code) in cases {
            assert_layout(&parse(Payload::from_slice(valid), limits).unwrap());
            assert_eq!(parse(Payload::from_slice(invalid), limits).unwrap_err().code, code);
        }
        assert_eq!(parse(Payload::from_slice(b"0"), Limits { max_payload_bytes: 0, ..defaults }).unwrap_err().code,
            ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn iterative_construction_handles_deep_and_wide_inputs() {
        let depth = 512;
        let mut input = Vec::new();
        input.resize(depth, b'[');
        input.push(b'0');
        input.resize(depth * 2 + 1, b']');
        let doc = parse(Payload::from_vec(input), Limits { max_depth: depth as u16, ..Limits::strict_default() }).unwrap();
        assert_layout(&doc);
        assert_eq!(doc.node_count(), depth + 1);
        for record in &doc.parts.containers {
            assert_eq!(record.end_node as usize, depth + 1);
            assert_eq!(record.child_count, 1);
        }
        let input = alloc::format!("[{}]", core::iter::repeat_n("null", 257).collect::<Vec<_>>().join(","));
        let doc = document(input.as_bytes());
        assert_layout(&doc);
        assert_eq!(doc.node_count(), 258);
        assert_eq!(doc.parts.containers[0].child_count, 257);
        assert_eq!(doc.metadata[257].next_sibling, None);
    }

    #[test]
    fn table_indices_reserve_the_sentinel_and_allocation_failure_is_fallible() {
        assert_eq!(available_index(0, 17).unwrap(), 0);
        assert_eq!(available_index((u32::MAX - 1) as usize, 17).unwrap(), u32::MAX - 1);
        let error = available_index(u32::MAX as usize, 17).unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::PayloadTooLarge, ErrorSource::Dom, 17));
        if usize::BITS > 32 {
            assert_eq!(available_index(usize::MAX, 18).unwrap_err().code, ErrorCode::PayloadTooLarge);
        }
        // This request exceeds Vec's addressable capacity before an allocator
        // is called, so the error path is deterministic on every target.
        let mut values = Vec::<u8>::new();
        let error = reserve(&mut values, usize::MAX, 19).unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset),
            (ErrorCode::BudgetExhausted, ErrorSource::Dom, 19));
    }

    #[test]
    fn raw_key_recovery_rejects_invalid_and_overflowing_ranges() {
        let raw = b" , \t\"a\\\"b\"";
        assert_eq!(key_span(raw, 0, raw.len() as u32).unwrap(), Span32::new(4, 6));
        assert_eq!(key_span(b"\"\"", 0, 2).unwrap(), Span32::new(0, 2));
        for (raw, before, after) in [
            (&b"\"x\""[..], 3, 2), (&b"\"x\""[..], 0, 4),
            (&b"\"x\""[..], u32::MAX, u32::MAX), (&b" "[..], 0, 1),
            (&b"x"[..], 0, 1), (&b"\"x"[..], 0, 2), (&b""[..], 0, 0),
        ] {
            let error = key_span(raw, before, after).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset),
                (ErrorCode::InternalError, ErrorSource::Dom, after));
        }
    }

    #[test]
    fn impossible_event_orders_and_invalid_references_return_dom_errors() {
        let cursor = jmax_cursor::walk::from_payload(Payload::from_slice(b"{}[]null"),
            Limits::strict_default(), SkipMode::Strict).unwrap();
        let mut builder = Builder::new();
        assert_eq!(builder.complete(0).unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(builder.event(Event::ObjectEnd { offset: 1 }, &cursor, 1, 2).unwrap_err().code,
            ErrorCode::InternalError);
        builder.event(Event::ObjectStart { offset: 0 }, &cursor, 0, 1).unwrap();
        assert_eq!(builder.event(Event::ArrayEnd { offset: 3 }, &cursor, 3, 4).unwrap_err().code,
            ErrorCode::InternalError);
        assert_eq!(builder.event(Event::Scalar { kind: ScalarKind::Null,
            span: Span32::new(4, 4), decoded: None }, &cursor, 4, 8).unwrap_err().code,
            ErrorCode::InternalError);
        builder.event(Event::ObjectEnd { offset: 1 }, &cursor, 1, 2).unwrap();
        assert_eq!(builder.event(Event::ArrayStart { offset: 2 }, &cursor, 2, 3).unwrap_err().code,
            ErrorCode::InternalError);
        for reference in [
            StrRef { span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 1 },
            StrRef { span: Span32::new(4, 4), source: SpanSource::RawPayload, scalar_len: 3 },
            StrRef { span: Span32::new(u32::MAX, 1), source: SpanSource::RawPayload, scalar_len: 0 },
        ] {
            assert_eq!(validate_text(&cursor, reference, 8).unwrap_err().code, ErrorCode::InternalError);
        }
        let mut builder = Builder::new();
        assert_eq!(builder.event(Event::Scalar { kind: ScalarKind::String,
            span: Span32::new(4, 4), decoded: None }, &cursor, 4, 8).unwrap_err().code,
            ErrorCode::InternalError);
        assert_eq!(builder.event(Event::Scalar { kind: ScalarKind::Null,
            span: Span32::new(u32::MAX, 1), decoded: None }, &cursor, 4, 8).unwrap_err().code,
            ErrorCode::InternalError);
        assert_eq!(delimiter(b"[", u32::MAX, 0, 0, b'[').unwrap_err().code, ErrorCode::InternalError);
    }
}
