//! Immutable edits and iterative, spelling-preserving compact serialization.
//!
//! Plans address only the original preorder table. Inserted values are parsed
//! independently and never become edit targets. The assembled bytes are parsed
//! again before publication, rebuilding every owner-relative reference.

use super::*;

/// Applies disjoint edits to the original document and returns a new owner.
///
/// Duplicate or overlapping targets are rejected before validating edit values
/// or writing output. Input validation errors retain input-relative offsets;
/// final validation errors retain offsets into the newly assembled payload.
pub fn apply(document: &Document, edits: &[Edit], limits: Limits) -> Result<Document> {
    let plan = prepare(document, edits, limits)?;
    let mut output = Output::Owned { bytes: Vec::new(), limit: limits.max_payload_bytes };
    serialize(document, &plan, &mut output)?;
    let bytes = match output {
        Output::Owned { bytes, .. } => bytes,
        Output::Borrowed(_) => return Err(dom_error(ErrorCode::InternalError, 0,
            "DOM rebuild lost its owned output")),
    };
    build::parse(Payload::from_vec(bytes), limits).map_err(|mut error| {
        error.detail = "DOM rebuilt document validation failed";
        error
    })
}

/// Rebuilds the reachable tree without insignificant whitespace or old owners.
pub fn compact(document: &Document, limits: Limits) -> Result<Document> {
    apply(document, &[], limits)
}

/// Writes compact JSON, retaining original scalar and member-key spellings.
///
/// The sink controls output capacity. Its reservation and write errors are
/// propagated unchanged, including after it has accepted an output prefix.
pub fn write_document(document: &Document, sink: &mut dyn ByteSink) -> Result<()> {
    serialize(document, &[], &mut Output::Borrowed(sink))
}

struct PreparedEdit<'a> {
    edit: &'a Edit,
    value: Option<Document>,
    member: Option<NodeId>,
}

fn prepare<'a>(document: &Document, edits: &'a [Edit], limits: Limits)
    -> Result<Vec<PreparedEdit<'a>>>
{
    // Check the complete target set before any output or replacement parsing.
    for edit in edits {
        if navigate::node(document, edit.target).is_none() {
            return Err(dom_error(ErrorCode::ConstraintViolation, 0,
                "DOM edit target is outside the original document"));
        }
    }
    let mut plan = Vec::new();
    plan.try_reserve_exact(edits.len()).map_err(|_| dom_error(
        ErrorCode::BudgetExhausted, 0, "DOM edit plan allocation failed"))?;
    for edit in edits {
        plan.push(PreparedEdit { edit, value: None, member: None });
    }
    // Unstable sorting uses no auxiliary allocation. Targets are unique only
    // after the following interval check, so ties must still be rejected.
    plan.sort_unstable_by_key(|entry| entry.edit.target.0);
    for pair in plan.windows(2) {
        let previous = pair[0].edit.target;
        let current = pair[1].edit.target;
        if current.0 < subtree_end(document, previous)? {
            return Err(dom_error(ErrorCode::ConstraintViolation,
                document.parts.spans[current.0 as usize].offset,
                "DOM edit targets duplicate or overlap an original subtree"));
        }
    }
    // Check operation kinds for the whole plan before parsing supplied values.
    for entry in &mut plan {
        let target = Node { document, id: entry.edit.target };
        let offset = navigate::raw_span(target).offset;
        match &entry.edit.action {
            EditAction::Remove if target.id.0 == 0 => return Err(dom_error(
                ErrorCode::ConstraintViolation, offset, "DOM edit cannot remove the root")),
            EditAction::AppendArray(_) if navigate::kind(target) != NodeKind::Array => {
                return Err(dom_error(ErrorCode::TypeMismatch, offset,
                    "DOM append edit requires an array target"));
            }
            EditAction::SetMember { key, .. } => {
                let object = navigate::object(target)?;
                validate_key(key, limits, offset)?;
                entry.member = object.get(key).map(Node::id);
            }
            _ => {}
        }
    }
    for entry in &mut plan {
        let (payload, detail) = match &entry.edit.action {
            EditAction::Replace(payload) => (payload, "DOM replacement input validation failed"),
            EditAction::AppendArray(payload) => (payload, "DOM appended input validation failed"),
            EditAction::SetMember { value, .. } => (value, "DOM member input validation failed"),
            EditAction::Remove => continue,
        };
        entry.value = Some(build::parse(payload.clone(), limits).map_err(|mut error| {
            error.detail = detail;
            error
        })?);
    }
    Ok(plan)
}

fn subtree_end(document: &Document, id: NodeId) -> Result<u32> {
    let word = document.parts.nodes[id.0 as usize];
    if matches!(word.kind(), TAG_ARRAY | TAG_OBJECT) {
        document.parts.containers.get(word.payload_index() as usize)
            .map(|record| record.end_node).ok_or_else(|| dom_error(
                ErrorCode::InternalError, document.parts.spans[id.0 as usize].offset,
                "DOM edit target has no container record"))
    } else {
        id.0.checked_add(1).ok_or_else(|| dom_error(ErrorCode::InternalError, 0,
            "DOM edit target has no representable subtree end"))
    }
}

fn find_edit(plan: &[PreparedEdit<'_>], id: NodeId) -> Option<usize> {
    plan.binary_search_by_key(&id.0, |entry| entry.edit.target.0).ok()
}

fn supplied_root<'a>(entry: &'a PreparedEdit<'_>, offset: u32) -> Result<Node<'a>> {
    entry.value.as_ref().map(navigate::root).ok_or_else(|| dom_error(
        ErrorCode::InternalError, offset, "DOM edit has no validated supplied value"))
}

/// A frame retains only immediate-child iteration state, never all children.
struct Frame<'a> {
    node: Node<'a>,
    children: Children<'a>,
    original: bool,
    plan_index: Option<usize>,
    emitted: bool,
    tail_emitted: bool,
    // Inserted subtrees inherit the original edit target's diagnostic offset.
    origin: u32,
}

enum Work<'a> {
    Value { node: Node<'a>, original: bool, origin: u32 },
    Container(Frame<'a>),
}

fn push_work<'a>(stack: &mut Vec<Work<'a>>, work: Work<'a>, bound: usize, offset: u32)
    -> Result<()>
{
    if stack.len() >= bound {
        return Err(dom_error(ErrorCode::InternalError, offset,
            "DOM serialization exceeded its input-derived traversal bound"));
    }
    stack.try_reserve(1).map_err(|_| dom_error(ErrorCode::BudgetExhausted, offset,
        "DOM serialization stack allocation failed"))?;
    stack.push(work);
    Ok(())
}

fn serialize<'a>(document: &'a Document, plan: &'a [PreparedEdit<'_>], output: &mut Output<'_>)
    -> Result<()>
{
    // An active path consists of original nodes followed by nodes from at most
    // one inserted value. Edits do not apply inside inserted values. This bounds
    // the work stack even before final depth validation, without recursion or
    // allocation proportional to a wide container's immediate child count.
    let inserted_bound = plan.iter().filter_map(|entry| entry.value.as_ref())
        .map(Document::node_count).max().unwrap_or(0);
    let bound = document.node_count().checked_add(inserted_bound).ok_or_else(|| dom_error(
        ErrorCode::PayloadTooLarge, 0, "DOM serialization traversal bound overflowed"))?;
    let mut stack = Vec::new();
    let root = navigate::root(document);
    push_work(&mut stack, Work::Value {
        node: root, original: true, origin: navigate::raw_span(root).offset,
    }, bound, navigate::raw_span(root).offset)?;

    while let Some(work) = stack.pop() {
        match work {
            Work::Value { mut node, mut original, origin } => {
                let mut plan_index = if original { find_edit(plan, node.id) } else { None };
                if let Some(index) = plan_index {
                    match &plan[index].edit.action {
                        EditAction::Replace(_) => {
                            node = supplied_root(&plan[index], origin)?;
                            original = false;
                            plan_index = None;
                        }
                        EditAction::Remove => return Err(dom_error(ErrorCode::InternalError, origin,
                            "DOM removed value reached serialization")),
                        _ => {}
                    }
                }
                match navigate::kind(node) {
                    NodeKind::Array | NodeKind::Object => {
                        let opening = if navigate::kind(node) == NodeKind::Array { b"[" } else { b"{" };
                        output.write(opening, origin)?;
                        let frame = Frame {
                            node, children: navigate::children(node)?, original, plan_index,
                            emitted: false, tail_emitted: false, origin,
                        };
                        push_work(&mut stack, Work::Container(frame), bound, origin)?;
                    }
                    _ => output.write(scalar::raw(node)?, origin)?,
                }
            }
            Work::Container(mut frame) => {
                let mut child = frame.children.next();
                // Removal happens before emitting a comma or an object key.
                while let Some(value) = child {
                    let removed = frame.original && find_edit(plan, value.id)
                        .is_some_and(|index| matches!(&plan[index].edit.action, EditAction::Remove));
                    if !removed { break; }
                    child = frame.children.next();
                }
                if let Some(value) = child {
                    let offset = if frame.original { navigate::raw_span(value).offset } else { frame.origin };
                    if frame.emitted { output.write(b",", offset)?; }
                    if navigate::kind(frame.node) == NodeKind::Object {
                        write_existing_key(value, output, offset)?;
                        output.write(b":", offset)?;
                    }
                    frame.emitted = true;
                    let replacement = frame.plan_index.and_then(|index| {
                        if plan[index].member == Some(value.id) { Some(index) } else { None }
                    });
                    let (node, original, origin) = if let Some(index) = replacement {
                        (supplied_root(&plan[index], frame.origin)?, false, frame.origin)
                    } else {
                        (value, frame.original, offset)
                    };
                    push_work(&mut stack, Work::Container(frame), bound, origin)?;
                    push_work(&mut stack, Work::Value { node, original, origin }, bound, origin)?;
                    continue;
                }

                // AppendArray and an absent SetMember contribute exactly one
                // trailing value. A matching SetMember was handled in place.
                if !frame.tail_emitted {
                    frame.tail_emitted = true;
                    if let Some(index) = frame.plan_index {
                        let entry = &plan[index];
                        let append = matches!(&entry.edit.action, EditAction::AppendArray(_))
                            || matches!(&entry.edit.action, EditAction::SetMember { .. })
                                && entry.member.is_none();
                        if append {
                            let origin = frame.origin;
                            if frame.emitted { output.write(b",", origin)?; }
                            if let EditAction::SetMember { key, .. } = &entry.edit.action {
                                write_new_key(key, output, origin)?;
                                output.write(b":", origin)?;
                            }
                            let node = supplied_root(entry, origin)?;
                            push_work(&mut stack, Work::Container(frame), bound, origin)?;
                            push_work(&mut stack, Work::Value { node, original: false, origin }, bound, origin)?;
                            continue;
                        }
                    }
                }
                let closing = if navigate::kind(frame.node) == NodeKind::Array { b"]" } else { b"}" };
                output.write(closing, frame.origin)?;
            }
        }
    }
    Ok(())
}

fn write_existing_key(node: Node<'_>, output: &mut Output<'_>, offset: u32) -> Result<()> {
    let bytes = node.document.metadata.get(node.id.0 as usize)
        .and_then(|metadata| metadata.key_span)
        .and_then(|span| span.slice(node.document.payload.as_slice()))
        .ok_or_else(|| dom_error(ErrorCode::InternalError, offset,
            "DOM serialized member has no resolvable original key span"))?;
    output.write(bytes, offset)
}

fn escape(byte: u8) -> Option<&'static [u8]> {
    match byte {
        b'"' => Some(b"\\\""),
        b'\\' => Some(b"\\\\"),
        8 => Some(b"\\b"),
        12 => Some(b"\\f"),
        b'\n' => Some(b"\\n"),
        b'\r' => Some(b"\\r"),
        b'\t' => Some(b"\\t"),
        _ => None,
    }
}

fn validate_key(key: &str, limits: Limits, offset: u32) -> Result<()> {
    let mut raw_len = 0usize;
    for byte in key.bytes() {
        let width = escape(byte).map_or(if byte < 0x20 { 6 } else { 1 }, |escaped| escaped.len());
        raw_len = raw_len.checked_add(width).ok_or_else(|| dom_error(
            ErrorCode::PayloadTooLarge, offset, "DOM new member key length overflowed"))?;
        if raw_len > limits.max_string_bytes as usize {
            return Err(dom_error(ErrorCode::ConstraintViolation, offset,
                "DOM new member key exceeds the string byte limit"));
        }
    }
    Ok(())
}

fn write_new_key(key: &str, output: &mut Output<'_>, offset: u32) -> Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.write(b"\"", offset)?;
    let bytes = key.as_bytes();
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte < 0x20 || matches!(byte, b'"' | b'\\') {
            if start != index { output.write(&bytes[start..index], offset)?; }
            if let Some(short) = escape(byte) {
                output.write(short, offset)?;
            } else {
                let escaped = [b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]];
                output.write(&escaped, offset)?;
            }
            start = index + 1;
        }
    }
    if start != bytes.len() { output.write(&bytes[start..], offset)?; }
    output.write(b"\"", offset)
}

enum Output<'a> {
    Owned { bytes: Vec<u8>, limit: u32 },
    Borrowed(&'a mut dyn ByteSink),
}

impl Output<'_> {
    fn write(&mut self, chunk: &[u8], offset: u32) -> Result<()> {
        match self {
            Self::Owned { bytes, limit } => {
                let next = bytes.len().checked_add(chunk.len()).filter(|&len| len <= *limit as usize)
                    .ok_or_else(|| dom_error(ErrorCode::PayloadTooLarge, offset,
                        "DOM rebuilt payload exceeds the payload byte limit"))?;
                bytes.try_reserve(next - bytes.len()).map_err(|_| dom_error(
                    ErrorCode::BudgetExhausted, offset, "DOM rebuilt payload allocation failed"))?;
                bytes.extend_from_slice(chunk);
                Ok(())
            }
            Self::Borrowed(sink) => {
                sink.len().checked_add(chunk.len()).ok_or_else(|| dom_error(
                    ErrorCode::PayloadTooLarge, offset, "DOM sink output length overflowed"))?;
                sink.reserve(chunk.len())?;
                sink.write_bytes(chunk)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigate::{array, node, object, root};

    fn document(bytes: &[u8]) -> Document {
        build::parse(Payload::from_slice(bytes), Limits::strict_default()).unwrap()
    }

    fn payload(bytes: &[u8]) -> Payload { Payload::from_slice(bytes) }

    fn edit(target: u32, action: EditAction) -> Edit {
        Edit { target: NodeId(target), action }
    }

    fn replace(target: u32, bytes: &[u8]) -> Edit {
        edit(target, EditAction::Replace(payload(bytes)))
    }

    fn append(target: u32, bytes: &[u8]) -> Edit {
        edit(target, EditAction::AppendArray(payload(bytes)))
    }

    fn set(target: u32, key: &str, bytes: &[u8]) -> Edit {
        edit(target, EditAction::SetMember { key: String::from(key), value: payload(bytes) })
    }

    fn assert_local<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode, offset: u32) {
        let error = result.unwrap_err();
        assert_eq!((error.code, error.source, error.byte_offset), (code, ErrorSource::Dom, offset));
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_final(result: Result<Document>, bytes: &[u8], limits: Limits) {
        let mut expected = build::parse(payload(bytes), limits).unwrap_err();
        expected.detail = "DOM rebuilt document validation failed";
        assert_eq!(result.unwrap_err(), expected);
    }

    // Check integration with all prior waves, not only the emitted byte vector.
    fn assert_document(document: &Document, expected: &[u8]) {
        assert_eq!(document.payload.as_slice(), expected);
        assert_eq!(scalar::raw(root(document)).unwrap(), expected);
        assert_eq!(document.parts.spans.len(), document.node_count());
        assert_eq!(document.metadata.len(), document.node_count());
        assert!(document.parts.numbers.is_empty());
        assert_eq!(document.parts.spans[0], Span32::new(0, expected.len() as u32));
        for (index, word) in document.parts.nodes.iter().copied().enumerate() {
            let current = node(document, NodeId(index as u32)).unwrap();
            let metadata = document.metadata[index];
            assert_eq!(word.aux24(), 0);
            assert!(metadata.number_index.is_none());
            assert!(!scalar::raw(current).unwrap().is_empty());
            assert_eq!(metadata.string.is_some(), word.kind() == TAG_STRING);
            for reference in [metadata.key, metadata.string].into_iter().flatten() {
                let text = document.resolve_str(reference).unwrap();
                assert_eq!(text.chars().count(), reference.scalar_len as usize);
            }
            if index == 0 {
                assert_eq!((metadata.parent, metadata.next_sibling, metadata.key, metadata.key_span),
                    (None, None, None, None));
            } else {
                let parent = navigate::parent(current).unwrap();
                assert!(parent.id.0 < current.id.0);
                let in_object = navigate::kind(parent) == NodeKind::Object;
                assert_eq!(metadata.key.is_some(), in_object);
                assert_eq!(metadata.key_span.is_some(), in_object);
                if let Some(span) = metadata.key_span {
                    let key = span.slice(document.payload.as_slice()).unwrap();
                    assert_eq!((key.first(), key.last()), (Some(&b'"'), Some(&b'"')));
                }
            }
            if matches!(word.kind(), TAG_ARRAY | TAG_OBJECT) {
                let record = document.parts.containers[word.payload_index() as usize];
                assert_eq!(record.reserved, 0);
                let children = navigate::children(current).unwrap().collect::<Vec<_>>();
                assert_eq!(children.len(), record.child_count as usize);
                assert_eq!(children.first().map_or(NO_NODE, |child| child.id.0), record.first_child);
                let mut next = current.id.0 + 1;
                for (position, child) in children.iter().copied().enumerate() {
                    assert_eq!(child.id.0, next);
                    assert_eq!(navigate::parent(child).unwrap().id, current.id);
                    assert_eq!(document.metadata[child.id.0 as usize].next_sibling,
                        children.get(position + 1).map(|following| following.id));
                    next = subtree_end(document, child.id).unwrap();
                }
                assert_eq!(next, record.end_node);
            } else {
                assert_eq!(word.payload_index(), current.id.0);
                scalar::value(current).unwrap();
            }
        }
        assert!(node(document, NodeId(document.node_count() as u32)).is_none());
        assert!(node(document, NodeId(NO_NODE)).is_none());
        let mut written = Vec::new();
        write_document(document, &mut written).unwrap();
        assert_eq!(written, expected);
    }

    #[test]
    fn compact_vectors_preserve_all_scalar_spellings() {
        let cases: &[(&[u8], &[u8])] = &[
            (b" \tnull\r\n", b"null"), (b" false ", b"false"), (b" true ", b"true"),
            (b" -0 ", b"-0"), (b" 1.2300e+0042 ", b"1.2300e+0042"),
            (b" 1E-99999 ", b"1E-99999"),
            (br#" "a  b\n\u0062\/" "#, br#""a  b\n\u0062\/""#),
            (b" [ ] ", b"[]"), (b" { } ", b"{}"),
            (br#" { "\u0061" : [ true, { "x" : -0 }, [] ], "z": "\uD83D\uDE00" } "#,
             br#"{"\u0061":[true,{"x":-0},[]],"z":"\uD83D\uDE00"}"#),
        ];
        for &(input, expected) in cases {
            let original = document(input);
            let rebuilt = compact(&original, Limits::strict_default()).unwrap();
            assert_document(&rebuilt, expected);
            assert_eq!(original.payload.as_slice(), input);
            let again = compact(&rebuilt, Limits::strict_default()).unwrap();
            assert_document(&again, expected);
            assert_eq!(rebuilt.parts.nodes, again.parts.nodes);
            assert_eq!(rebuilt.parts.spans, again.parts.spans);
            assert_eq!(rebuilt.metadata, again.metadata);
        }
    }

    #[test]
    fn empty_apply_equals_compact_and_writer_appends_to_sink() {
        let original = document(br#" { "x" : [ 1, 2 ] } "#);
        let expected = br#"{"x":[1,2]}"#;
        assert_document(&apply(&original, &[], Limits::strict_default()).unwrap(), expected);
        let mut bytes = b"prefix:".to_vec();
        write_document(&original, &mut bytes).unwrap();
        assert_eq!(bytes, br#"prefix:{"x":[1,2]}"#);
    }

    #[test]
    fn replacing_root_accepts_each_value_kind_and_compacts_insertions() {
        let original = document(br#"{"old":"\n"}"#);
        let cases: &[(&[u8], &[u8])] = &[
            (b" null ", b"null"), (b" false ", b"false"), (b" true ", b"true"),
            (b" -0 ", b"-0"), (br#" "\u0061" "#, br#""\u0061""#),
            (b" [ 1, { } ] ", b"[1,{}]"), (br#" { "x" : [ ] } "#, br#"{"x":[]}"#),
        ];
        for &(input, expected) in cases {
            let rebuilt = apply(&original, &[replace(0, input)], Limits::strict_default()).unwrap();
            assert_document(&rebuilt, expected);
        }
        assert_eq!(original.payload.as_slice(), br#"{"old":"\n"}"#);
    }

    #[test]
    fn edits_use_original_ids_and_are_independent_of_caller_order() {
        let original = document(br#"[0,[1,2],{"k":3},4]"#);
        // Original IDs: array 0; zero 1; array 2; one 3; two 4;
        // object 5; three 6; four 7. Earlier removal must not rebase 5 or 7.
        let mut edits = alloc::vec![replace(7, b"[ 8, 9 ]"), set(5, "k", b"true"),
            edit(1, EditAction::Remove), append(2, b"{\"z\":0}")];
        let expected = br#"[[1,2,{"z":0}],{"k":true},[8,9]]"#;
        let rebuilt = apply(&original, &edits, Limits::strict_default()).unwrap();
        assert_document(&rebuilt, expected);
        edits.reverse();
        assert_document(&apply(&original, &edits, Limits::strict_default()).unwrap(), expected);
        assert_eq!(original.node_count(), 8);
        assert_eq!(scalar::u64(node(&original, NodeId(7)).unwrap()).unwrap(), 4);
    }

    #[test]
    fn removal_handles_first_middle_last_only_and_nested_members() {
        let cases: &[(&[u8], &[u32], &[u8])] = &[
            (b"[0,1,2]", &[1], b"[1,2]"), (b"[0,1,2]", &[2], b"[0,2]"),
            (b"[0,1,2]", &[3], b"[0,1]"), (b"[0,1,2]", &[1,2,3], b"[]"),
            (b"[[0],1]", &[1], b"[1]"),
            (br#"{"a":0,"b":[1],"c":2}"#, &[1], br#"{"b":[1],"c":2}"#),
            (br#"{"a":0,"b":[1],"c":2}"#, &[2], br#"{"a":0,"c":2}"#),
            (br#"{"a":0,"b":[1],"c":2}"#, &[4], br#"{"a":0,"b":[1]}"#),
            (br#"{"a":{"b":1}}"#, &[1], b"{}"),
            (br#"{"a":0,"b":1}"#, &[1,2], b"{}"),
        ];
        for &(input, targets, expected) in cases {
            let original = document(input);
            let edits = targets.iter().map(|&target| edit(target, EditAction::Remove)).collect::<Vec<_>>();
            assert_document(&apply(&original, &edits, Limits::strict_default()).unwrap(), expected);
        }
    }

    #[test]
    fn append_empty_and_populated_arrays_retains_one_value() {
        for input in [&b"[]"[..], &b"[0]"[..]] {
            let original = document(input);
            let rebuilt = apply(&original, &[append(0, b" [ 1, 2 ] ")], Limits::strict_default()).unwrap();
            let expected: &[u8] = if input == b"[]" { b"[[1,2]]" } else { b"[0,[1,2]]" };
            assert_document(&rebuilt, expected);
            assert_eq!(array(root(&rebuilt)).unwrap().len(), if input == b"[]" { 1 } else { 2 });
        }
    }

    #[test]
    fn set_member_preserves_existing_key_spelling_and_position() {
        let original = document(br#"{"z":0,"\u0061":{"old":true},"a/b":2}"#);
        let rebuilt = apply(&original, &[set(0, "a", b" [ 3, 4 ] ")], Limits::strict_default()).unwrap();
        assert_document(&rebuilt, br#"{"z":0,"\u0061":[3,4],"a/b":2}"#);
        assert_eq!(object(root(&rebuilt)).unwrap().iter().map(|member| member.key).collect::<Vec<_>>(),
            ["z", "a", "a/b"]);
        for (input, expected) in [
            (&b"{}"[..], &br#"{"":null}"#[..]),
            (&br#"{"x":0}"#[..], &br#"{"x":0,"":null}"#[..]),
        ] {
            assert_document(&apply(&document(input), &[set(0, "", b"null")],
                Limits::strict_default()).unwrap(), expected);
        }
    }

    #[test]
    fn duplicate_lookup_replaces_only_the_first_decoded_match() {
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        let original = build::parse(payload(br#"{"\u0061":0,"a":1,"b":2}"#), limits).unwrap();
        let rebuilt = apply(&original, &[set(0, "a", b"true")], limits).unwrap();
        assert_document(&rebuilt, br#"{"\u0061":true,"a":1,"b":2}"#);
        assert_eq!(object(root(&rebuilt)).unwrap().len(), 3);
        assert!(scalar::boolean(object(root(&rebuilt)).unwrap().get("a").unwrap()).unwrap());
        let expected = br#"{"\u0061":true,"a":1,"b":2}"#;
        assert_final(apply(&original, &[set(0, "a", b"true")], Limits::strict_default()),
            expected, Limits::strict_default());
        assert_final(compact(&original, Limits::strict_default()),
            br#"{"\u0061":0,"a":1,"b":2}"#, Limits::strict_default());
    }

    #[test]
    fn inserted_key_escaping_is_exact_and_utf8_is_not_normalized() {
        let key = "\"\\\u{8}\u{c}\n\r\t\0\u{1}\u{1f}/é😀";
        let rebuilt = apply(&document(b"{}"), &[set(0, key, b"0")], Limits::strict_default()).unwrap();
        let expected = "{\"\\\"\\\\\\b\\f\\n\\r\\t\\u0000\\u0001\\u001f/é😀\":0}";
        assert_document(&rebuilt, expected.as_bytes());
        assert_eq!(object(root(&rebuilt)).unwrap().iter().next().unwrap().key, key);
        assert_eq!(scalar::u64(object(root(&rebuilt)).unwrap().get(key).unwrap()).unwrap(), 0);
        let rebuilt = apply(&document("{\"é\":0}".as_bytes()), &[set(0, "e\u{301}", b"1")],
            Limits::strict_default()).unwrap();
        assert_document(&rebuilt, "{\"é\":0,\"e\u{301}\":1}".as_bytes());
    }

    #[test]
    fn every_control_byte_has_a_deterministic_lowercase_json_escape() {
        let expected: &[&str] = &[
            "\\u0000", "\\u0001", "\\u0002", "\\u0003", "\\u0004", "\\u0005", "\\u0006", "\\u0007",
            "\\b", "\\t", "\\n", "\\u000b", "\\f", "\\r", "\\u000e", "\\u000f",
            "\\u0010", "\\u0011", "\\u0012", "\\u0013", "\\u0014", "\\u0015", "\\u0016", "\\u0017",
            "\\u0018", "\\u0019", "\\u001a", "\\u001b", "\\u001c", "\\u001d", "\\u001e", "\\u001f",
        ];
        for (byte, spelling) in expected.iter().enumerate() {
            let mut key = String::new();
            key.push(char::from(byte as u8));
            let rebuilt = apply(&document(b"{}"), &[set(0, &key, b"0")], Limits::strict_default()).unwrap();
            let mut bytes = b"{\"".to_vec();
            bytes.extend_from_slice(spelling.as_bytes());
            bytes.extend_from_slice(b"\":0}");
            assert_document(&rebuilt, &bytes);
            assert!(object(root(&rebuilt)).unwrap().get(&key).is_some());
        }
    }

    #[test]
    fn all_targets_and_conflicts_are_rejected_before_value_parsing() {
        let original = document(b" [ [ 0 ], 1 ] ");
        for bad in [original.node_count() as u32, NO_NODE] {
            assert_local(apply(&original, &[replace(1, b"invalid"), replace(bad, b"null")],
                Limits::strict_default()), ErrorCode::ConstraintViolation, 0);
        }
        for edits in [
            alloc::vec![replace(1, b"invalid"), replace(1, b"null")],
            alloc::vec![append(1, b"invalid"), replace(2, b"null")],
            alloc::vec![replace(2, b"invalid"), edit(1, EditAction::Remove)],
            alloc::vec![set(0, "x", b"invalid"), replace(3, b"null")],
        ] {
            let error = apply(&original, &edits, Limits::strict_default()).unwrap_err();
            assert_eq!((error.code, error.source), (ErrorCode::ConstraintViolation, ErrorSource::Dom));
            assert!(error.detail.contains("overlap"));
        }
    }

    #[test]
    fn root_removal_and_wrong_container_kinds_have_original_offsets() {
        let original = document(b" \n[ null, false, true, 1, \"x\", [], {} ]");
        assert_local(apply(&original, &[edit(0, EditAction::Remove)], Limits::strict_default()),
            ErrorCode::ConstraintViolation, 2);
        for id in 1..original.node_count() as u32 {
            let target = node(&original, NodeId(id)).unwrap();
            let offset = navigate::raw_span(target).offset;
            if navigate::kind(target) != NodeKind::Array {
                assert_local(apply(&original, &[append(id, b"null")], Limits::strict_default()),
                    ErrorCode::TypeMismatch, offset);
            }
            if navigate::kind(target) != NodeKind::Object {
                assert_local(apply(&original, &[set(id, "x", b"null")], Limits::strict_default()),
                    ErrorCode::TypeMismatch, offset);
            }
        }
    }

    #[test]
    fn conflict_offsets_identify_the_original_duplicate_or_descendant() {
        let original = document(b" [ [ 0 ], 1 ]");
        for (edits, offset) in [
            (alloc::vec![replace(1, b"null"), append(1, b"0")], 3),
            (alloc::vec![replace(2, b"null"), append(1, b"0")], 5),
            (alloc::vec![replace(0, b"null"), replace(3, b"0")], 10),
        ] {
            assert_local(apply(&original, &edits, Limits::strict_default()),
                ErrorCode::ConstraintViolation, offset);
        }
        // Adjacent subtree endpoints are disjoint, including an empty subtree.
        assert_document(&apply(&document(b"[[],0]"), &[append(1, b"1"), replace(2, b"2")],
            Limits::strict_default()).unwrap(), b"[[1],2]");
    }

    #[test]
    fn malformed_insertions_keep_input_relative_errors_and_stage_details() {
        let invalid: &[&[u8]] = &[
            b"", b" ", b"null true", b"[0,]", br#"{"a":}"#, br#""\q""#,
            br#""\uD800""#, b"\"\xff\"", b"01", b"[", b"/*x*/null",
        ];
        for &bytes in invalid {
            let expected = build::parse(payload(bytes), Limits::strict_default()).unwrap_err();
            for (original, operation, detail) in [
                (document(b" [ 0 ] "), replace(1, bytes), "DOM replacement input validation failed"),
                (document(b" [ ] "), append(0, bytes), "DOM appended input validation failed"),
                (document(b" { } "), set(0, "x", bytes), "DOM member input validation failed"),
            ] {
                let mut staged = expected.clone();
                staged.detail = detail;
                assert_eq!(apply(&original, &[operation], Limits::strict_default()).unwrap_err(), staged);
            }
        }
    }

    #[test]
    fn each_replacement_input_limit_is_checked_before_compaction() {
        let defaults = Limits::strict_default();
        let cases: &[(Limits, &[u8], ErrorCode)] = &[
            (Limits { max_payload_bytes: 4, ..defaults }, b" null ", ErrorCode::PayloadTooLarge),
            (Limits { max_depth: 1, ..defaults }, b"[[]]", ErrorCode::DepthLimitExceeded),
            (Limits { max_array_items: 1, ..defaults }, b"[0,1]", ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 1, ..defaults }, br#"{"a":0,"b":1}"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 1, ..defaults }, br#""ab""#, ErrorCode::ConstraintViolation),
            (Limits { max_number_digits: 1, ..defaults }, b"12", ErrorCode::ConstraintViolation),
            (Limits { max_decoded_bytes: 0, ..defaults }, br#""\n""#, ErrorCode::DecodedArenaOverflow),
            (defaults, br#"{"a":0,"a":1}"#, ErrorCode::DuplicateObjectKey),
        ];
        let original = document(b"null");
        for &(limits, bytes, code) in cases {
            let mut expected = build::parse(payload(bytes), limits).unwrap_err();
            assert_eq!(expected.code, code);
            expected.detail = "DOM replacement input validation failed";
            assert_eq!(apply(&original, &[replace(0, bytes)], limits).unwrap_err(), expected);
        }
    }

    #[test]
    fn combined_depth_and_zero_depth_are_final_document_limits() {
        let original = document(b"[[0]]");
        let limits = Limits { max_depth: 2, ..Limits::strict_default() };
        assert_final(apply(&original, &[replace(2, b"[]")], limits), b"[[[]]]", limits);
        let exact = Limits { max_depth: 3, ..limits };
        assert_document(&apply(&original, &[replace(2, b"[]")], exact).unwrap(), b"[[[]]]");
        let zero = Limits { max_depth: 0, ..limits };
        assert_document(&compact(&document(b"0"), zero).unwrap(), b"0");
        assert_final(compact(&document(b"[]"), zero), b"[]", zero);
        // Removing deep input can satisfy a smaller final depth limit.
        assert_document(&apply(&original, &[replace(0, b"0")], zero).unwrap(), b"0");
    }

    #[test]
    fn combined_item_and_member_counts_enforce_inclusive_limits() {
        let limits = Limits { max_array_items: 1, max_fields_per_object: 1, ..Limits::strict_default() };
        assert_document(&apply(&document(b"[]"), &[append(0, b"0")], limits).unwrap(), b"[0]");
        assert_final(apply(&document(b"[0]"), &[append(0, b"1")], limits), b"[0,1]", limits);
        assert_document(&apply(&document(b"{}"), &[set(0, "a", b"0")], limits).unwrap(), br#"{"a":0}"#);
        assert_final(apply(&document(br#"{"a":0}"#), &[set(0, "b", b"1")], limits),
            br#"{"a":0,"b":1}"#, limits);
        assert_document(&apply(&document(br#"{"a":0}"#), &[set(0, "a", b"1")], limits).unwrap(), br#"{"a":1}"#);
        let zero = Limits { max_array_items: 0, max_fields_per_object: 0, ..limits };
        assert_document(&compact(&document(b"[]"), zero).unwrap(), b"[]");
        assert_document(&compact(&document(b"{}"), zero).unwrap(), b"{}");
        assert_final(apply(&document(b"[]"), &[append(0, b"0")], zero), b"[0]", zero);
        assert_final(apply(&document(b"{}"), &[set(0, "", b"0")], zero), br#"{"":0}"#, zero);
        assert_document(&apply(&document(b"[0,1]"), &[edit(1, EditAction::Remove)], limits).unwrap(), b"[1]");
    }

    #[test]
    fn combined_decoded_arena_includes_retained_and_inserted_strings_and_keys() {
        let original = document(br#"["\n"]"#);
        let limits = Limits { max_decoded_bytes: 1, ..Limits::strict_default() };
        assert_final(apply(&original, &[append(0, br#""\t""#)], limits), br#"["\n","\t"]"#, limits);
        let exact = Limits { max_decoded_bytes: 2, ..limits };
        let rebuilt = apply(&original, &[append(0, br#""\t""#)], exact).unwrap();
        assert_document(&rebuilt, br#"["\n","\t"]"#);
        assert_eq!(rebuilt.parts.decoded, b"\n\t");
        assert_final(apply(&document(br#"{"x":"\n"}"#), &[set(0, "\t", b"0")], limits),
            br#"{"x":"\n","\t":0}"#, limits);
    }

    #[test]
    fn final_validation_applies_stricter_scalar_limits_to_retained_values() {
        let defaults = Limits::strict_default();
        let string_limits = Limits { max_string_bytes: 1, ..defaults };
        assert_final(compact(&document(br#"["ab"]"#), string_limits), br#"["ab"]"#, string_limits);
        let number_limits = Limits { max_number_digits: 1, ..defaults };
        assert_final(compact(&document(b"[12]"), number_limits), b"[12]", number_limits);
        let arena_limits = Limits { max_decoded_bytes: 0, ..defaults };
        assert_final(compact(&document(br#"["\n"]"#), arena_limits), br#"["\n"]"#, arena_limits);
        // Removed content is not charged to the new document's arena or strings.
        let original = document(br#"["\u0061",""]"#);
        let zero = Limits { max_string_bytes: 0, max_decoded_bytes: 0, ..defaults };
        let rebuilt = apply(&original, &[edit(1, EditAction::Remove)], zero).unwrap();
        assert_document(&rebuilt, br#"[""]"#);
        assert!(rebuilt.parts.decoded.is_empty());
    }

    #[test]
    fn inserted_key_limits_count_escaped_bytes_and_preserve_empty_keys() {
        let original = document(b" {}");
        for (key, exact_len) in [("", 0), ("é", 2), ("\0", 6), ("\n", 2), ("\"", 2), ("\\", 2)] {
            let exact = Limits { max_string_bytes: exact_len, ..Limits::strict_default() };
            let rebuilt = apply(&original, &[set(0, key, b"0")], exact).unwrap();
            assert!(object(root(&rebuilt)).unwrap().get(key).is_some());
            if exact_len != 0 {
                let below = Limits { max_string_bytes: exact_len - 1, ..exact };
                assert_local(apply(&original, &[set(0, key, b"0")], below),
                    ErrorCode::ConstraintViolation, 1);
            }
        }
        let zero_arena = Limits { max_decoded_bytes: 0, ..Limits::strict_default() };
        assert_document(&apply(&original, &[set(0, "é", b"0")], zero_arena).unwrap(), "{\"é\":0}".as_bytes());
        assert_final(apply(&original, &[set(0, "\n", b"0")], zero_arena), br#"{"\n":0}"#, zero_arena);
        let existing = document(br#" {"long":0}"#);
        assert_local(apply(&existing, &[set(0, "long", b"1")],
            Limits { max_string_bytes: 3, ..Limits::strict_default() }), ErrorCode::ConstraintViolation, 1);
    }

    #[test]
    fn payload_limits_are_checked_before_growth_at_exact_boundaries() {
        let original = document(b" \n[ 0 ] \t");
        let exact = Limits { max_payload_bytes: 5, ..Limits::strict_default() };
        assert_document(&apply(&original, &[append(0, b"1")], exact).unwrap(), b"[0,1]");
        assert_local(apply(&original, &[append(0, b"1")], Limits { max_payload_bytes: 4, ..exact }),
            ErrorCode::PayloadTooLarge, 2);
        assert_document(&compact(&original, Limits { max_payload_bytes: 3, ..exact }).unwrap(), b"[0]");
        assert_local(compact(&document(b"null"), Limits { max_payload_bytes: 3, ..exact }),
            ErrorCode::PayloadTooLarge, 0);
        assert_local(compact(&document(b"0"), Limits { max_payload_bytes: 0, ..exact }),
            ErrorCode::PayloadTooLarge, 0);
        let mut output = Output::Owned { bytes: Vec::new(), limit: 2 };
        output.write(b"ab", 7).unwrap();
        assert_local(output.write(b"c", 11), ErrorCode::PayloadTooLarge, 11);
        output.write(b"", 13).unwrap();
        match output {
            Output::Owned { bytes, .. } => assert_eq!(bytes, b"ab"),
            Output::Borrowed(_) => panic!("expected owned output"),
        }
    }

    #[test]
    fn compaction_reclaims_removed_decoded_data_and_rebuilds_owner_offsets() {
        let original = document(br#" {"gone":"\u0061\u0062\u0063","keep":"\n"} "#);
        let old_handle = object(root(&original)).unwrap().get("keep").unwrap();
        let old_span = navigate::raw_span(old_handle);
        let old_meta = original.metadata.clone();
        let old_arena = original.parts.decoded.clone();
        let rebuilt = apply(&original, &[edit(1, EditAction::Remove)], Limits::strict_default()).unwrap();
        assert_document(&rebuilt, br#"{"keep":"\n"}"#);
        assert_eq!(rebuilt.parts.decoded, b"\n");
        assert_eq!(rebuilt.node_count(), 2);
        let new_handle = object(root(&rebuilt)).unwrap().get("keep").unwrap();
        assert_eq!(new_handle.id, NodeId(1));
        assert_eq!(scalar::string(new_handle).unwrap(), "\n");
        assert_ne!(navigate::raw_span(new_handle), old_span);
        drop(rebuilt);
        assert_eq!(scalar::string(old_handle).unwrap(), "\n");
        assert_eq!(navigate::raw_span(old_handle), old_span);
        assert_eq!(original.metadata, old_meta);
        assert_eq!(original.parts.decoded, old_arena);
        let detached = {
            let temporary = document(br#" { "x" : "\u0061" } "#);
            compact(&temporary, Limits::strict_default()).unwrap()
        };
        assert_document(&detached, br#"{"x":"\u0061"}"#);
        assert_eq!(scalar::string(object(root(&detached)).unwrap().get("x").unwrap()).unwrap(), "a");
    }

    #[test]
    fn failures_leave_original_storage_and_existing_handles_unchanged() {
        let original = document(br#" [ { "x" : "\n" }, 3 ] "#);
        let handle = node(&original, NodeId(2)).unwrap();
        let before_nodes = original.parts.nodes.clone();
        let before_spans = original.parts.spans.clone();
        let before_metadata = original.metadata.clone();
        let before_decoded = original.parts.decoded.clone();
        for edits in [alloc::vec![replace(1, b"[}")],
            alloc::vec![replace(1, b"0"), replace(2, b"0")], alloc::vec![append(2, b"0")]] {
            assert!(apply(&original, &edits, Limits::strict_default()).is_err());
        }
        assert_eq!(original.parts.nodes, before_nodes);
        assert_eq!(original.parts.spans, before_spans);
        assert_eq!(original.metadata, before_metadata);
        assert_eq!(original.parts.decoded, before_decoded);
        assert_eq!(scalar::string(handle).unwrap(), "\n");
        assert_eq!(original.payload.as_slice(), br#" [ { "x" : "\n" }, 3 ] "#);
    }

    #[test]
    fn huge_exact_numbers_never_require_numeric_conversion() {
        let bytes = b"[18446744073709551616,-9223372036854775809,1e99999,-0,1.0000]";
        let original = document(bytes);
        let rebuilt = compact(&original, Limits::strict_default()).unwrap();
        assert_document(&rebuilt, bytes);
        assert_eq!(scalar::u64(node(&rebuilt, NodeId(1)).unwrap()).unwrap_err().code, ErrorCode::NumberOverflow);
        assert_eq!(scalar::f64(node(&rebuilt, NodeId(3)).unwrap()).unwrap_err().code, ErrorCode::NumberOverflow);
        assert_document(&apply(&document(b"[]"), &[append(0, &bytes[1..21])],
            Limits::strict_default()).unwrap(), b"[18446744073709551616]");
    }

    #[test]
    fn traversal_handles_deep_paths_and_wide_containers_iteratively() {
        let depth = 512usize;
        let mut bytes = alloc::vec![b'['];
        bytes.resize(depth, b'[');
        bytes.extend_from_slice(b"0");
        bytes.resize(depth * 2 + 1, b']');
        let limits = Limits { max_depth: depth as u16, ..Limits::strict_default() };
        let original = build::parse(payload(&bytes), limits).unwrap();
        let rebuilt = compact(&original, limits).unwrap();
        assert_document(&rebuilt, &bytes);
        assert_eq!(rebuilt.node_count(), depth + 1);
        let rebuilt = apply(&original, &[replace(depth as u32, b"1")], limits).unwrap();
        bytes[depth] = b'1';
        assert_document(&rebuilt, &bytes);
        let mut wide = b"[".to_vec();
        for index in 0..256 {
            if index != 0 { wide.push(b','); }
            wide.extend_from_slice(b"[]");
        }
        wide.push(b']');
        let original = document(&wide);
        assert_document(&compact(&original, Limits::strict_default()).unwrap(), &wide);
        let edits = (1..=256).map(|id| edit(id, EditAction::Remove)).collect::<Vec<_>>();
        assert_document(&apply(&original, &edits, Limits::strict_default()).unwrap(), b"[]");
    }

    struct TestSink {
        bytes: Vec<u8>,
        limit: usize,
        fail_reserve: bool,
        fail_write: bool,
        reservations: usize,
        error: Error,
    }

    impl TestSink {
        fn new(limit: usize) -> Self {
            Self { bytes: Vec::new(), limit, fail_reserve: false, fail_write: false,
                reservations: 0, error: Error::new(ErrorCode::Cancelled, ErrorSource::Transport,
                    73, "deterministic sink failure").with_schema(5, 9) }
        }
    }

    impl ByteSink for TestSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reservations += 1;
            if self.fail_reserve || additional > self.limit.saturating_sub(self.bytes.len()) {
                return Err(self.error.clone());
            }
            ByteSink::reserve(&mut self.bytes, additional)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            if self.fail_write { return Err(self.error.clone()); }
            ByteSink::write_bytes(&mut self.bytes, bytes)
        }

        fn len(&self) -> usize { self.bytes.len() }
    }

    #[test]
    fn sink_reservation_write_and_prefix_errors_propagate_unchanged() {
        let original = document(b" [ 1, 2 ] ");
        for (reserve, write) in [(true, false), (false, true)] {
            let mut sink = TestSink::new(usize::MAX);
            sink.fail_reserve = reserve;
            sink.fail_write = write;
            assert_eq!(write_document(&original, &mut sink).unwrap_err(), sink.error);
            assert_eq!(sink.reservations, 1);
            assert!(sink.bytes.is_empty());
        }
        let mut sink = TestSink::new(2);
        assert_eq!(write_document(&original, &mut sink).unwrap_err(), sink.error);
        assert_eq!(sink.bytes, b"[1");
        let mut sink = TestSink::new(5);
        write_document(&original, &mut sink).unwrap();
        assert_eq!(sink.bytes, b"[1,2]");
        assert!(sink.reservations > 0);
        assert_eq!(original.payload.as_slice(), b" [ 1, 2 ] ");
    }

    struct OverflowSink;

    impl ByteSink for OverflowSink {
        fn reserve(&mut self, _: usize) -> Result<()> { panic!("overflow must precede reservation") }
        fn write_bytes(&mut self, _: &[u8]) -> Result<()> { panic!("overflow must precede writing") }
        fn len(&self) -> usize { usize::MAX }
    }

    #[test]
    fn sink_size_overflow_and_traversal_bound_fail_without_growth() {
        assert_local(write_document(&document(b" 0"), &mut OverflowSink), ErrorCode::PayloadTooLarge, 1);
        let original = document(b"0");
        let mut stack = Vec::new();
        assert_local(push_work(&mut stack, Work::Value { node: root(&original), original: true, origin: 7 },
            0, 7), ErrorCode::InternalError, 7);
        assert!(stack.is_empty());
    }
}
