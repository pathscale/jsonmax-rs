//! Complete-input, single-pass tape construction directly from BlockCursor.
use super::*;
use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::{DuplicatePolicyOwner, StringMeta, TokenKind};
use jmax_syntax::block_cursor::BlockCursor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    First,
    Next,
    AfterValue,
}
#[derive(Clone, Copy, Debug)]
struct OpenContainer {
    node: u32,
    tag: u8,
    count: u32,
    phase: Phase,
}
#[derive(Clone, Copy, Debug)]
struct SeenKey {
    generation: u32,
    span: Span32,
    source: SpanSource,
    hash: u64,
}
impl SeenKey {
    const EMPTY: Self = Self {
        generation: 0,
        span: Span32::new(0, 0),
        source: SpanSource::RawPayload,
        hash: 0,
    };
}
#[derive(Debug)]
struct DepthKeys {
    slots: Vec<SeenKey>,
    generation: u32,
    len: usize,
}
impl Default for DepthKeys {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            generation: 1,
            len: 0,
        }
    }
}
impl DepthKeys {
    /// Closing an object invalidates its slots without visiting its keys.
    fn reset(&mut self) {
        self.len = 0;
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // Only generation wrap needs a physical clear (once per 2^32 uses).
            self.slots.fill(SeenKey::EMPTY);
            self.generation = 1;
        }
    }
}

/// Reusable final tape, escaped output, nesting stack, and duplicate-key table.
/// `parse_into` lends its owned result until the next mutable scratch use.
/// Borrowed results take the final buffers; return them with `recycle` before
/// the next parse to reuse their capacity. Failed parses retain all capacity.
#[derive(Debug, Default)]
pub struct DomScratch {
    tape: Vec<TapeWord>,
    decoded: Vec<u8>,
    stack: Vec<OpenContainer>,
    seen: Vec<DepthKeys>,
    document: Option<Document>,
}
pub type ParseScratch = DomScratch;
impl DomScratch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn document(&self) -> Option<&Document> {
        self.document.as_ref()
    }
    pub fn take_document(&mut self) -> Option<Document> {
        self.document.take()
    }
    pub fn tape_capacity(&self) -> usize {
        self.document
            .as_ref()
            .map_or(self.tape.capacity(), |doc| doc.tape.capacity())
    }
    pub fn decoded_capacity(&self) -> usize {
        self.document
            .as_ref()
            .map_or(self.decoded.capacity(), |doc| doc.decoded.capacity())
    }
    pub fn stack_capacity(&self) -> usize {
        self.stack.capacity()
    }
    pub fn duplicate_capacity(&self) -> usize {
        self.seen.iter().map(|depth| depth.slots.len()).sum()
    }
    pub fn clear_keep_capacity(&mut self) {
        if let Some(document) = self.document.take() {
            self.reclaim_owned(document);
        }
        self.tape.clear();
        self.decoded.clear();
        self.stack.clear();
        for depth in &mut self.seen {
            if depth.len != 0 {
                depth.reset();
            }
        }
    }
    pub fn clear(&mut self) {
        self.clear_keep_capacity();
    }
    pub(crate) fn reclaim_owned(&mut self, document: Document) {
        self.keep_buffers(document.tape, document.decoded);
    }
    pub(crate) fn reclaim_borrowed(&mut self, document: BorrowedDocument<'_>) {
        self.keep_buffers(document.tape, document.decoded);
    }
    fn keep_buffers(&mut self, mut tape: Vec<TapeWord>, mut decoded: Vec<u8>) {
        tape.clear();
        decoded.clear();
        if tape.capacity() > self.tape.capacity() {
            self.tape = tape;
        }
        if decoded.capacity() > self.decoded.capacity() {
            self.decoded = decoded;
        }
    }
}

pub fn parse(payload: Payload, limits: Limits) -> Result<Document> {
    parse_owned(payload, limits, None, &mut DomScratch::new())
}
pub fn parse_with_numeric(
    payload: Payload,
    limits: Limits,
    numeric: NumericPolicy,
) -> Result<Document> {
    parse_owned(payload, limits, Some(numeric), &mut DomScratch::new())
}
pub fn parse_into(payload: Payload, limits: Limits, scratch: &mut DomScratch) -> Result<&Document> {
    parse_options_into(payload, limits, None, scratch)
}
pub fn parse_with_numeric_into(
    payload: Payload,
    limits: Limits,
    numeric: NumericPolicy,
    scratch: &mut DomScratch,
) -> Result<&Document> {
    parse_options_into(payload, limits, Some(numeric), scratch)
}
fn parse_options_into(
    payload: Payload,
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<&Document> {
    let document = parse_owned(payload, limits, numeric, scratch)?;
    scratch.document = Some(document);
    scratch.document.as_ref().ok_or_else(|| {
        dom_error(
            ErrorCode::InternalError,
            0,
            "DOM completed scratch result is missing",
        )
    })
}
fn parse_owned(
    payload: Payload,
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<Document> {
    let summary = build(payload.as_slice(), limits, numeric, scratch)?;
    let decoded_len = scratch.decoded.len() as u32;
    // SAFETY: build strictly validated the exact immutable payload, all cooked
    // references, grammar, numeric/duplicate policy, limits, and trailing EOF.
    let proof = unsafe { ValidationProof::issue_unchecked(0, payload.len() as u32, decoded_len) };
    // SAFETY: The capability is bound to the exact retained immutable owner.
    let capability = unsafe {
        ValidationCapability::issue_for_payload(
            &payload,
            decoded_len,
            0,
            limits.policy_fingerprint(),
        )
    };
    Ok(TapeDocument {
        payload,
        tape: core::mem::take(&mut scratch.tape),
        decoded: core::mem::take(&mut scratch.decoded),
        node_count: summary.nodes,
        container_count: summary.containers,
        max_depth: summary.max_depth,
        proof,
        capability,
    })
}

pub fn parse_borrowed(raw: &[u8], limits: Limits) -> Result<BorrowedDocument<'_>> {
    parse_borrowed_with_scratch(raw, limits, &mut DomScratch::new())
}
pub fn parse_borrowed_with_numeric(
    raw: &[u8],
    limits: Limits,
    numeric: NumericPolicy,
) -> Result<BorrowedDocument<'_>> {
    parse_borrowed_options_with_scratch(raw, limits, Some(numeric), &mut DomScratch::new())
}
pub fn parse_borrowed_with_scratch<'a>(
    raw: &'a [u8],
    limits: Limits,
    scratch: &mut DomScratch,
) -> Result<BorrowedDocument<'a>> {
    parse_borrowed_options_with_scratch(raw, limits, None, scratch)
}
pub fn parse_borrowed_with_numeric_and_scratch<'a>(
    raw: &'a [u8],
    limits: Limits,
    numeric: NumericPolicy,
    scratch: &mut DomScratch,
) -> Result<BorrowedDocument<'a>> {
    parse_borrowed_options_with_scratch(raw, limits, Some(numeric), scratch)
}
pub fn parse_borrowed_options_with_scratch<'a>(
    raw: &'a [u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<BorrowedDocument<'a>> {
    let summary = build(raw, limits, numeric, scratch)?;
    let raw_len = raw.len() as u32;
    let decoded_len = scratch.decoded.len() as u32;
    // SAFETY: build strictly validated all input and output under these limits;
    // the original borrow prevents mutation for the returned document lifetime.
    let proof = unsafe { ValidationProof::issue_unchecked(0, raw_len, decoded_len) };
    // SAFETY: Both base and visible pointer identify this exact borrowed slice.
    let capability = unsafe {
        ValidationCapability::issue_unchecked(
            raw.as_ptr() as usize,
            raw.as_ptr() as usize,
            raw_len,
            decoded_len,
            0,
            limits.policy_fingerprint(),
        )
    };
    Ok(BorrowedDocument {
        raw,
        tape: core::mem::take(&mut scratch.tape),
        decoded: core::mem::take(&mut scratch.decoded),
        node_count: summary.nodes,
        container_count: summary.containers,
        max_depth: summary.max_depth,
        proof,
        capability,
    })
}

/// Publicly mutable legacy tables are checked, then the retained payload is
/// reparsed. Foreign tables/proofs never authorize native unchecked access.
pub fn from_validated(document: ValidatedDocument, limits: Limits) -> Result<Document> {
    document.validate_layout()?;
    parse(document.payload, limits)
}
pub fn from_legacy(document: ValidatedDocument, limits: Limits) -> Result<Document> {
    from_validated(document, limits)
}

#[derive(Default)]
struct Summary {
    nodes: u32,
    containers: u32,
    max_depth: u16,
}
fn build(
    raw: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<Summary> {
    scratch.clear_keep_capacity();
    let result = build_inner(raw, limits, numeric, scratch);
    if result.is_err() {
        scratch.clear_keep_capacity();
    }
    result
}
fn build_inner(
    raw: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<Summary> {
    let mut cursor = BlockCursor::new(raw, limits, numeric, DuplicatePolicyOwner::Syntax)?;
    if scratch.tape.capacity() == 0 {
        // A single bounded cold reservation avoids incremental tape growth.
        // Warm scratch already knows the required tape size, even for inputs
        // whose bytes are dominated by one long string.
        let initial = (raw.len() / 8)
            .clamp(16, 262_144)
            .min(limits.max_payload_bytes as usize);
        reserve(&mut scratch.tape, initial, 0)?;
    }
    let mut summary = Summary::default();
    append_value(&mut cursor, scratch, &mut summary, 0, None)?;
    while let Some(frame) = scratch.stack.last().copied() {
        let byte = cursor.skip_whitespace()?.ok_or_else(|| {
            syntax_error(
                ErrorCode::UnexpectedEof,
                cursor.offset(),
                "unterminated JSON container",
            )
        })?;
        let closing = if frame.tag == TAG_OBJECT { b'}' } else { b']' };
        if frame.phase == Phase::AfterValue {
            if byte == closing {
                close(&mut cursor, scratch)?;
                continue;
            }
            if byte == b',' {
                cursor.advance(1);
            } else {
                cursor.expect_byte(b',', "expected comma or closing container delimiter")?;
            }
            if let Some(top) = scratch.stack.last_mut() {
                top.phase = Phase::Next;
            }
            continue;
        }
        if byte == closing && frame.phase == Phase::First {
            close(&mut cursor, scratch)?;
            continue;
        }
        let mut key_flags = 0;
        if frame.tag == TAG_OBJECT {
            check_count(frame, limits, cursor.offset())?;
            if byte != b'"' {
                return Err(syntax_error(
                    ErrorCode::InvalidSyntax,
                    cursor.offset(),
                    "expected an object member key",
                ));
            }
            let string = cursor.scan_string_meta(&mut scratch.decoded)?;
            let key_index = tape_index(scratch.tape.len(), string.raw_token.offset)?;
            append_string(&mut scratch.tape, TAPE_TAG_KEY, string, 0, NO_NODE)?;
            if limits.reject_duplicate_keys {
                let depth = scratch.stack.len() - 1;
                // The hash only places keys in this builder's private table;
                // matches are always confirmed on the cooked bytes.
                let hash = key_hash(string_bytes(
                    raw,
                    &scratch.decoded,
                    string.cooked.span,
                    string.cooked.source,
                ));
                check_duplicate(
                    scratch,
                    raw,
                    depth,
                    hash,
                    string.cooked,
                    string.raw_token.offset,
                )?;
            }
            key_flags = TAPE_FLAG_HAS_KEY;
            if scratch.tape[key_index as usize].has_flag(TAPE_FLAG_EXTENDED) {
                key_flags |= TAPE_FLAG_KEY_EXTENDED;
            }
            if cursor.skip_whitespace()? == Some(b':') {
                cursor.advance(1);
            } else {
                cursor.expect_byte(b':', "expected colon after object member key")?;
            }
        } else {
            check_count(frame, limits, cursor.offset())?;
        }
        let value_byte = (frame.tag == TAG_ARRAY).then_some(byte);
        append_value(&mut cursor, scratch, &mut summary, key_flags, value_byte)?;
    }
    cursor.finish_eof()?;
    Ok(summary)
}
#[inline(always)]
fn check_count(frame: OpenContainer, limits: Limits, offset: u32) -> Result<()> {
    let limit = if frame.tag == TAG_OBJECT {
        limits.max_fields_per_object
    } else {
        limits.max_array_items
    };
    if frame.count >= limit {
        Err(syntax_error(
            ErrorCode::ConstraintViolation,
            offset,
            "JSON container exceeds its immediate child limit",
        ))
    } else {
        Ok(())
    }
}
#[inline(always)]
fn append_value(
    cursor: &mut BlockCursor<'_>,
    scratch: &mut DomScratch,
    summary: &mut Summary,
    key_flags: u32,
    value_byte: Option<u8>,
) -> Result<()> {
    let byte = match value_byte {
        Some(byte) => Some(byte),
        None => cursor.skip_whitespace()?,
    }
    .ok_or_else(|| {
        syntax_error(
            ErrorCode::UnexpectedEof,
            cursor.offset(),
            "expected a JSON value",
        )
    })?;
    let offset = cursor.offset();
    let index = tape_index(scratch.tape.len(), offset)?;
    let parent = scratch.stack.last().map_or(NO_NODE, |frame| frame.node);
    match byte {
        b'{' | b'[' => {
            if scratch.stack.len() >= cursor.limits().max_depth as usize {
                return Err(syntax_error(
                    ErrorCode::DepthLimitExceeded,
                    offset,
                    "JSON container exceeds depth limit",
                ));
            }
            let tag = if byte == b'{' { TAG_OBJECT } else { TAG_ARRAY };
            reserve(&mut scratch.tape, 3, offset)?;
            reserve(&mut scratch.stack, 1, offset)?;
            cursor.advance(1);
            scratch
                .tape
                .push(TapeWord::pack_header(tag, key_flags, offset));
            scratch.tape.push(TapeWord::pack_pair(parent, index + 3));
            scratch.tape.push(TapeWord::pack_pair(0, offset + 1));
            commit_child(scratch);
            scratch.stack.push(OpenContainer {
                node: index,
                tag,
                count: 0,
                phase: Phase::First,
            });
            summary.containers = increment(summary.containers, offset)?;
            summary.max_depth = summary.max_depth.max(scratch.stack.len() as u16);
        }
        b'"' => {
            let meta = cursor.scan_string_meta(&mut scratch.decoded)?;
            append_string(&mut scratch.tape, TAG_STRING, meta, key_flags, parent)?;
            commit_child(scratch);
        }
        b'-' | b'0'..=b'9' => {
            let span = cursor.scan_number_span()?;
            append_number(&mut scratch.tape, span, key_flags, parent)?;
            commit_child(scratch);
        }
        b'n' | b't' | b'f' => {
            let (kind, span) = cursor.scan_literal()?;
            let tag = match kind {
                TokenKind::Null => TAG_NULL,
                TokenKind::False => TAG_FALSE,
                TokenKind::True => TAG_TRUE,
                _ => {
                    return Err(dom_error(
                        ErrorCode::InternalError,
                        offset,
                        "BlockCursor returned a nonliteral kind",
                    ));
                }
            };
            reserve(&mut scratch.tape, 2, offset)?;
            scratch.tape.push(TapeWord::pack_header(
                tag,
                key_flags | span.len,
                span.offset,
            ));
            scratch.tape.push(TapeWord::pack_pair(parent, index + 2));
            commit_child(scratch);
        }
        _ => {
            return Err(syntax_error(
                ErrorCode::InvalidSyntax,
                offset,
                "expected a JSON value",
            ));
        }
    }
    summary.nodes = increment(summary.nodes, offset)?;
    Ok(())
}
#[inline(always)]
fn commit_child(scratch: &mut DomScratch) {
    if let Some(frame) = scratch.stack.last_mut() {
        frame.count += 1;
        frame.phase = Phase::AfterValue;
    }
}
fn close(cursor: &mut BlockCursor<'_>, scratch: &mut DomScratch) -> Result<()> {
    let frame = scratch.stack.pop().ok_or_else(|| {
        dom_error(
            ErrorCode::InternalError,
            cursor.offset(),
            "DOM close has no open container",
        )
    })?;
    let closing = if frame.tag == TAG_OBJECT { b'}' } else { b']' };
    if cursor.peek_byte() == Some(closing) {
        cursor.advance(1);
    } else {
        cursor.expect_byte(closing, "mismatched JSON container delimiter")?;
    }
    let end = tape_index(scratch.tape.len(), cursor.offset())?;
    let index = frame.node as usize;
    let parent = scratch.tape[index + 1].low32();
    scratch.tape[index + 1] = TapeWord::pack_pair(parent, end);
    scratch.tape[index + 2] = TapeWord::pack_pair(frame.count, cursor.offset());
    if frame.tag == TAG_OBJECT {
        if let Some(depth) = scratch.seen.get_mut(scratch.stack.len()) {
            depth.reset();
        }
    }
    Ok(())
}
#[inline(always)]
fn append_number(
    tape: &mut Vec<TapeWord>,
    span: Span32,
    key_flags: u32,
    parent: u32,
) -> Result<()> {
    let index = tape_index(tape.len(), span.offset)?;
    let extended = span.len > TAPE_AUX_LEN_MASK;
    let width = if extended { 3 } else { 2 };
    reserve(tape, width, span.offset)?;
    let aux = key_flags
        | if extended {
            TAPE_FLAG_EXTENDED
        } else {
            span.len
        };
    tape.push(TapeWord::pack_header(TAG_NUMBER, aux, span.offset));
    tape.push(TapeWord::pack_pair(parent, index + width as u32));
    if extended {
        tape.push(TapeWord(span.len as u64));
    }
    Ok(())
}
#[inline(always)]
fn append_string(
    tape: &mut Vec<TapeWord>,
    tag: u8,
    meta: StringMeta,
    key_flags: u32,
    parent: u32,
) -> Result<()> {
    let index = tape_index(tape.len(), meta.raw_token.offset)?;
    let cooked = meta.cooked;
    let ascii = cooked.scalar_len == cooked.span.len;
    let compact = meta.is_unescaped && ascii && cooked.span.len <= TAPE_AUX_LEN_MASK;
    if compact {
        let width = if tag == TAPE_TAG_KEY { 1 } else { 2 };
        reserve(tape, width, meta.raw_token.offset)?;
        tape.push(TapeWord::pack_header(
            tag,
            key_flags | TAPE_FLAG_ASCII | cooked.span.len,
            meta.raw_token.offset,
        ));
        if tag != TAPE_TAG_KEY {
            tape.push(TapeWord::pack_pair(parent, index + width as u32));
        }
        return Ok(());
    }
    let mut aux = key_flags | TAPE_FLAG_EXTENDED;
    if ascii {
        aux |= TAPE_FLAG_ASCII;
    }
    if cooked.source == SpanSource::DecodedArena {
        aux |= TAPE_FLAG_ESCAPED;
    }
    let header = TapeWord::pack_header(tag, aux, meta.raw_token.offset);
    let width = header.record_words();
    reserve(tape, width, meta.raw_token.offset)?;
    tape.push(header);
    if tag != TAPE_TAG_KEY {
        tape.push(TapeWord::pack_pair(parent, index + width as u32));
    }
    tape.push(TapeWord::pack_pair(cooked.span.offset, cooked.span.len));
    tape.push(TapeWord::pack_pair(cooked.scalar_len, meta.raw_token.len));
    Ok(())
}
fn increment(value: u32, offset: u32) -> Result<u32> {
    value.checked_add(1).ok_or_else(|| {
        dom_error(
            ErrorCode::PayloadTooLarge,
            offset,
            "DOM counter exceeds u32",
        )
    })
}
#[inline(always)]
fn tape_index(len: usize, offset: u32) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        dom_error(
            ErrorCode::PayloadTooLarge,
            offset,
            "DOM tape exceeds u32 addressing",
        )
    })
}
#[inline(always)]
fn reserve<T>(values: &mut Vec<T>, additional: usize, offset: u32) -> Result<()> {
    let next = values.len().checked_add(additional).ok_or_else(|| {
        dom_error(
            ErrorCode::PayloadTooLarge,
            offset,
            "DOM storage length overflow",
        )
    })?;
    if next > u32::MAX as usize {
        return Err(dom_error(
            ErrorCode::PayloadTooLarge,
            offset,
            "DOM storage exceeds u32 addressing",
        ));
    }
    if next <= values.capacity() {
        return Ok(());
    }
    values.try_reserve(additional).map_err(|_| {
        dom_error(
            ErrorCode::BudgetExhausted,
            offset,
            "DOM storage allocation failed",
        )
    })
}
fn syntax_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Syntax, offset, detail)
}

// The direct grammar consumer owns duplicate policy at object boundaries.
// Each depth retains only its current object's generation. Entries borrow spans
// from raw input or decoded output; no key bytes or tape IDs are retained.
fn check_duplicate(
    scratch: &mut DomScratch,
    raw: &[u8],
    depth: usize,
    hash: u64,
    reference: StrRef,
    offset: u32,
) -> Result<()> {
    if depth >= scratch.seen.len() {
        let additional = depth + 1 - scratch.seen.len();
        reserve(&mut scratch.seen, additional, offset)?;
        scratch.seen.resize_with(depth + 1, DepthKeys::default);
    }
    let table = &mut scratch.seen[depth];
    if table.len + 1 > table.slots.len() / 2 {
        let next = if table.slots.is_empty() {
            16
        } else {
            table.slots.len().checked_mul(2).ok_or_else(|| {
                dom_error(
                    ErrorCode::BudgetExhausted,
                    offset,
                    "DOM duplicate table capacity overflow",
                )
            })?
        };
        let mut slots = Vec::new();
        reserve(&mut slots, next, offset)?;
        slots.resize(next, SeenKey::EMPTY);
        for entry in table
            .slots
            .iter()
            .copied()
            .filter(|entry| entry.generation == table.generation)
        {
            insert_seen(&mut slots, entry);
        }
        table.slots = slots;
    }
    let mask = table.slots.len() - 1;
    let mut slot = hash as usize & mask;
    loop {
        let entry = table.slots[slot];
        if entry.generation != table.generation {
            table.slots[slot] = SeenKey {
                generation: table.generation,
                span: reference.span,
                source: reference.source,
                hash,
            };
            table.len += 1;
            return Ok(());
        }
        if entry.hash == hash && entry.span.len == reference.span.len {
            let previous_bytes = string_bytes(raw, &scratch.decoded, entry.span, entry.source);
            let current_bytes =
                string_bytes(raw, &scratch.decoded, reference.span, reference.source);
            if previous_bytes == current_bytes {
                return Err(syntax_error(
                    ErrorCode::DuplicateObjectKey,
                    offset,
                    "duplicate decoded JSON object key",
                ));
            }
        }
        slot = (slot + 1) & mask;
    }
}
fn insert_seen(table: &mut [SeenKey], entry: SeenKey) {
    let mask = table.len() - 1;
    let mut slot = entry.hash as usize & mask;
    while table[slot].generation == entry.generation {
        slot = (slot + 1) & mask;
    }
    table[slot] = entry;
}
#[inline(always)]
fn string_bytes<'a>(
    raw: &'a [u8],
    decoded: &'a [u8],
    span: Span32,
    source: SpanSource,
) -> &'a [u8] {
    let bytes = if source == SpanSource::RawPayload {
        raw
    } else {
        decoded
    };
    let start = span.offset as usize;
    &bytes[start..start + span.len as usize]
}

/// Placement hash for the builder's private duplicate-key tables. Matches are
/// always confirmed on the cooked bytes, so this affects probe length only.
/// Word-at-a-time instead of FNV-1a's dependent multiply per byte.
#[inline(always)]
fn key_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut hash = (bytes.len() as u64).wrapping_mul(K);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        hash = (hash ^ u64::from_le_bytes(word)).wrapping_mul(K);
        hash ^= hash >> 32;
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut word = 0u64;
        for (index, &byte) in rest.iter().enumerate() {
            word |= u64::from(byte) << (8 * index);
        }
        hash = (hash ^ word).wrapping_mul(K);
        hash ^= hash >> 32;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_objects_reuse_one_depth_table_without_retaining_keys() -> Result<()> {
        let mut raw = String::from("[");
        for index in 0..256 {
            if index != 0 {
                raw.push(',');
            }
            raw.push_str(&alloc::format!("{{\"key{index}\":{index}}}"));
        }
        raw.push(']');
        let mut scratch = DomScratch::new();
        let doc =
            parse_borrowed_with_scratch(raw.as_bytes(), Limits::strict_default(), &mut scratch)?;
        assert!(doc.decoded().is_empty());
        assert_eq!(doc.root_node().array()?.len(), 256);
        assert_eq!(scratch.duplicate_capacity(), 16);
        assert!(scratch.seen.iter().all(|depth| depth.len == 0));
        let slots = scratch.seen[1].slots.as_ptr();
        let generation = scratch.seen[1].generation;
        let tape = doc.tape().as_ptr();
        doc.recycle(&mut scratch);
        let doc =
            parse_borrowed_with_scratch(raw.as_bytes(), Limits::strict_default(), &mut scratch)?;
        assert_eq!(scratch.seen[1].slots.as_ptr(), slots);
        assert_eq!(scratch.seen[1].generation, generation + 256);
        assert_eq!(doc.tape().as_ptr(), tape);
        assert_eq!(scratch.duplicate_capacity(), 16);
        Ok(())
    }

    #[test]
    fn collisions_survive_growth_and_still_compare_cooked_bytes() -> Result<()> {
        let raw = b"abcdefghijklmnopqrstuvwxyz";
        let mut scratch = DomScratch::new();
        for index in 0..raw.len() {
            let reference = StrRef {
                span: Span32::new(index as u32, 1),
                source: SpanSource::RawPayload,
                scalar_len: 1,
            };
            // Force every distinct key to collide, including during rehash.
            check_duplicate(&mut scratch, raw, 0, 7, reference, index as u32)?;
        }
        assert_eq!(scratch.seen[0].len, raw.len());
        let reference = StrRef {
            span: Span32::new(0, 1),
            source: SpanSource::RawPayload,
            scalar_len: 1,
        };
        assert_eq!(
            check_duplicate(&mut scratch, raw, 0, 7, reference, 99)
                .err()
                .map(|error| error.code),
            Some(ErrorCode::DuplicateObjectKey),
        );
        scratch.seen[0].reset();
        scratch.decoded.push(b'a');
        check_duplicate(&mut scratch, raw, 0, 7, reference, 0)?;
        let decoded = StrRef {
            source: SpanSource::DecodedArena,
            ..reference
        };
        assert_eq!(
            check_duplicate(&mut scratch, raw, 0, 7, decoded, 100)
                .err()
                .map(|error| error.code),
            Some(ErrorCode::DuplicateObjectKey),
        );
        Ok(())
    }

    #[test]
    fn generation_wrap_clears_stale_entries() -> Result<()> {
        let mut scratch = DomScratch::new();
        let reference = StrRef {
            span: Span32::new(0, 1),
            source: SpanSource::RawPayload,
            scalar_len: 1,
        };
        check_duplicate(&mut scratch, b"a", 0, 0, reference, 0)?;
        let table = &mut scratch.seen[0];
        table.generation = u32::MAX;
        table.reset();
        assert_eq!(table.generation, 1);
        assert!(table.slots.iter().all(|entry| entry.generation == 0));
        check_duplicate(&mut scratch, b"a", 0, 0, reference, 0)?;
        Ok(())
    }

    #[test]
    fn full_u32_number_length_and_compact_boundary() -> Result<()> {
        let mut tape = Vec::new();
        append_number(&mut tape, Span32::new(7, TAPE_AUX_LEN_MASK), 0, NO_NODE)?;
        assert_eq!(tape[0].record_words(), 2);
        assert_eq!(tape[0].compact_len(), TAPE_AUX_LEN_MASK);
        assert_eq!(tape[1].high32(), 2);
        tape.clear();
        append_number(&mut tape, Span32::new(0, u32::MAX), 0, NO_NODE)?;
        assert_eq!(tape[0].record_words(), 3);
        assert_eq!(tape[1].low32(), NO_NODE);
        assert_eq!(tape[1].high32(), 3);
        assert_eq!(tape[2].0, u32::MAX as u64);
        Ok(())
    }
}
