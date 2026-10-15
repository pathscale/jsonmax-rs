//! Strict, iterative construction of the DOM's preorder node tables.

use super::*;
use jmax_cursor::{Cursor, Event, ScalarKind, ScanStep, SkipMode};

/// Parses one complete JSON value, including validation of trailing input.
///
/// Numbers retain their exact spelling. Object keys and string values retain
/// the cursor's owner-relative references without decoding or rebasing them.
pub fn parse(payload: Payload, limits: Limits) -> Result<Document> {
    from_cursor(jmax_cursor::walk::from_payload(payload, limits, SkipMode::Strict)?)
}

/// Parses with the facade's numeric policy applied during cursor scanning.
/// Number spellings remain raw spans and need no post-construction node pass.
pub fn parse_with_numeric(
    payload: Payload,
    limits: Limits,
    numeric: jmax_abi::number::NumericPolicy,
) -> Result<Document> {
    let cursor = jmax_cursor::walk::from_payload_with_numeric(
        payload, limits, SkipMode::Strict, numeric,
    )?;
    from_cursor(cursor)
}

fn from_cursor(mut cursor: Cursor) -> Result<Document> {
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
    builder.parts.node_metadata = builder.metadata;
    let payload_len = u32::try_from(output.payload.len()).map_err(|_| dom_error(
        ErrorCode::PayloadTooLarge, offset, "DOM payload length exceeds u32"))?;
    let decoded_len = u32::try_from(builder.parts.decoded.len()).map_err(|_| dom_error(
        ErrorCode::DecodedArenaOverflow, offset, "DOM decoded arena length exceeds u32"))?;
    // SAFETY: The strict cursor consumed one complete JSON value through EOF.
    // Builder::complete checked the parallel tables, every stored reference was
    // resolved and its scalar count checked while processing the cursor events,
    // and the exact payload and decoded owners are transferred below. Zero is
    // the schema-free syntax fingerprint; no schema validation is claimed.
    let proof = unsafe { ValidationProof::issue_unchecked(0, payload_len, decoded_len) };
    Ok(Document {
        payload: output.payload,
        parts: builder.parts,
        proof,
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
        let depth = u16::try_from(self.stack.len()).map_err(|_| dom_error(
            ErrorCode::DepthLimitExceeded, after, "DOM container depth exceeds u16"))?;
        self.parts.max_depth = self.parts.max_depth.max(depth);
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

/// Reusable DOM construction storage. Results borrow this scratch, so the next
/// parse cannot overwrite a live document. Growth can allocate; warmed capacities
/// are retained across successful calls and after syntax errors.
pub struct ParseScratch {
    cursor: Cursor,
    builder: Builder,
    document: Option<Document>,
}
impl Default for ParseScratch {
    fn default() -> Self { Self {
        cursor: Cursor::new(Limits::strict_default(), SkipMode::Strict),
        builder: Builder::new(), document: None,
    } }
}
impl ParseScratch {
    pub fn document(&self) -> Option<&Document> { self.document.as_ref() }
}

pub fn parse_into(payload: Payload, limits: Limits, scratch: &mut ParseScratch)
    -> Result<&Document> {
    parse_with_options_into(payload, limits, None, scratch)
}

pub fn parse_with_numeric_into(payload: Payload, limits: Limits,
    numeric: jmax_abi::number::NumericPolicy, scratch: &mut ParseScratch) -> Result<&Document> {
    parse_with_options_into(payload, limits, Some(numeric), scratch)
}
fn parse_with_options_into(payload: Payload, limits: Limits,
    numeric: Option<jmax_abi::number::NumericPolicy>, scratch: &mut ParseScratch) -> Result<&Document> {
    if let Some(document) = scratch.document.take() {
        scratch.builder.parts = document.parts;
        scratch.builder.metadata = core::mem::take(&mut scratch.builder.parts.node_metadata);
        scratch.cursor.swap_decoded(&mut scratch.builder.parts.decoded);
    }
    let builder = &mut scratch.builder;
    builder.parts.clear_keep_capacity();
    builder.metadata.clear();
    builder.stack.clear();
    let cursor = &mut scratch.cursor;
    cursor.reset_with_payload(payload.clone(), limits, SkipMode::Strict, numeric)?;
    loop {
        let before = cursor.state().byte_offset;
        match jmax_cursor::walk::next(cursor)? {
            ScanStep::Emit(event) => builder.event(event, cursor, before, cursor.state().byte_offset)?,
            ScanStep::Finished => break,
            ScanStep::NeedMore => return Err(dom_error(ErrorCode::InternalError,
                cursor.state().byte_offset, "complete DOM input requested more bytes")),
        }
    }
    builder.complete(cursor.state().byte_offset)?;
    cursor.swap_decoded(&mut builder.parts.decoded);
    builder.parts.node_metadata = core::mem::take(&mut builder.metadata);
    let payload_len = u32::try_from(payload.len()).map_err(|_| dom_error(
        ErrorCode::PayloadTooLarge, 0, "DOM payload length exceeds u32"))?;
    let decoded_len = u32::try_from(builder.parts.decoded.len()).map_err(|_| dom_error(
        ErrorCode::DecodedArenaOverflow, 0, "DOM decoded length exceeds u32"))?;
    // SAFETY: The strict cursor reached EOF, Builder checked all links and
    // references, and the exact payload and decoded arena move into the document.
    let proof = unsafe { ValidationProof::issue_unchecked(0, payload_len, decoded_len) };
    scratch.document = Some(Document { payload, parts: core::mem::take(&mut builder.parts), proof });
    scratch.document.as_ref().ok_or_else(|| dom_error(ErrorCode::InternalError, 0,
        "completed DOM document is missing"))
}
