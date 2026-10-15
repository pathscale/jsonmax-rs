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
    let bytes = node.document.parts.node_metadata.get(node.id.0 as usize)
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
