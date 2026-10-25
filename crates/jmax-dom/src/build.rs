//! Complete-input, single-pass tape construction directly from BlockCursor.
use super::*;
use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::{DuplicatePolicyOwner, StringMeta, TokenKind};
use jmax_syntax::block_cursor::BlockCursor;
mod fast;

/// Direct clean-token path for the one-pass tape builder. The established
/// cursor remains the authority for escapes, UTF-8, rare numbers and errors.
/// Every shortcut proves an accepting token before advancing the cursor.
struct FastDomCursor<'a, const FAST: bool>(BlockCursor<'a>);
impl<'a, const FAST: bool> core::ops::Deref for FastDomCursor<'a, FAST> {
    type Target = BlockCursor<'a>;
    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<const FAST: bool> core::ops::DerefMut for FastDomCursor<'_, FAST> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<const FAST: bool> FastDomCursor<'_, FAST> {
    #[inline(always)]
    fn skip_whitespace(&mut self) -> Result<Option<u8>> {
        if !FAST {
            return self.0.skip_whitespace();
        }
        let start = self.offset() as usize;
        let mut end = start;
        while self
            .input()
            .get(end)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            end += 1;
            if self
                .input()
                .get(end)
                .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            {
                end += jmax_dispatch::short::whitespace_prefix(&self.input()[end..]);
                break;
            }
        }
        self.advance((end - start) as u32);
        Ok(self.peek_byte())
    }

    #[inline(always)]
    fn scan_number_span(&mut self) -> Result<Span32> {
        if !FAST {
            return self.0.scan_number_span();
        }
        if let Some(span) = self.0.try_scan_number_span_fast() {
            return Ok(span);
        }
        self.0.scan_number_span()
    }

    #[inline(always)]
    fn finish_eof(&mut self) -> Result<()> {
        if self.skip_whitespace()?.is_none() {
            Ok(())
        } else {
            self.0.finish_eof()
        }
    }

    #[inline(always)]
    fn scan_literal(&mut self) -> Result<(TokenKind, Span32)> {
        if !FAST {
            return self.0.scan_literal();
        }
        let start = self.offset() as usize;
        let (spelling, kind): (&[u8], _) = match self.peek_byte() {
            Some(b'n') => (b"null", TokenKind::Null),
            Some(b't') => (b"true", TokenKind::True),
            Some(b'f') => (b"false", TokenKind::False),
            _ => return self.0.scan_literal(),
        };
        let end = start + spelling.len();
        if self.input().get(start..end) == Some(spelling)
            && self
                .input()
                .get(end)
                .is_none_or(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'))
        {
            self.advance(spelling.len() as u32);
            return Ok((kind, Span32::new(start as u32, spelling.len() as u32)));
        }
        self.0.scan_literal()
    }
}

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
/// Most JSON documents nest shallowly. Deeper documents retain a spill vector.
#[derive(Debug)]
struct ContainerStack {
    inline: [OpenContainer; 32],
    len: usize,
    spill: Vec<OpenContainer>,
}
impl Default for ContainerStack {
    fn default() -> Self {
        Self {
            inline: [OpenContainer {
                node: 0,
                tag: 0,
                count: 0,
                phase: Phase::First,
            }; 32],
            len: 0,
            spill: Vec::new(),
        }
    }
}
impl ContainerStack {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len + self.spill.len()
    }
    fn capacity(&self) -> usize {
        self.inline.len() + self.spill.capacity()
    }
    fn clear(&mut self) {
        self.len = 0;
        self.spill.clear();
    }
    #[inline(always)]
    fn last(&self) -> Option<&OpenContainer> {
        if self.spill.is_empty() {
            self.inline[..self.len].last()
        } else {
            self.spill.last()
        }
    }
    #[inline(always)]
    fn last_mut(&mut self) -> Option<&mut OpenContainer> {
        if self.spill.is_empty() {
            self.inline[..self.len].last_mut()
        } else {
            self.spill.last_mut()
        }
    }
    #[inline(always)]
    fn push(&mut self, value: OpenContainer) {
        if self.len < self.inline.len() {
            self.inline[self.len] = value;
            self.len += 1;
        } else {
            self.spill.push(value);
        }
    }
    #[inline(always)]
    fn pop(&mut self) -> Option<OpenContainer> {
        if let Some(value) = self.spill.pop() {
            return Some(value);
        }
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.inline[self.len])
    }
    fn reserve(&mut self, additional: usize, offset: u32) -> Result<()> {
        let need = self
            .len()
            .checked_add(additional)
            .ok_or_else(|| dom_error(ErrorCode::BudgetExhausted, offset, "DOM depth overflow"))?;
        if need > self.inline.len() {
            let additional = need - self.inline.len() - self.spill.len();
            reserve(&mut self.spill, additional, offset)?;
        }
        Ok(())
    }
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
    inline: [SeenKey; 128],
    slots: Vec<SeenKey>,
    generation: u32,
    len: usize,
}
impl Default for DepthKeys {
    fn default() -> Self {
        Self {
            inline: [SeenKey::EMPTY; 128],
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
            self.inline.fill(SeenKey::EMPTY);
            self.generation = 1;
        }
    }
}

/// Eight shallow duplicate levels stay inside the builder. Each level uses
/// generation tags; deeper documents retain a fallible spill vector. This
/// consumes about 32 KiB of fixed builder storage instead of allocating it.
#[derive(Debug)]
struct DepthTables {
    inline: [DepthKeys; 8],
    spill: Vec<DepthKeys>,
}
impl Default for DepthTables {
    fn default() -> Self {
        Self {
            inline: core::array::from_fn(|_| DepthKeys::default()),
            spill: Vec::new(),
        }
    }
}
impl DepthTables {
    fn len(&self) -> usize {
        self.inline.len() + self.spill.len()
    }
    fn get_mut(&mut self, index: usize) -> Option<&mut DepthKeys> {
        if index < self.inline.len() {
            self.inline.get_mut(index)
        } else {
            self.spill.get_mut(index - self.inline.len())
        }
    }
    fn iter(&self) -> impl Iterator<Item = &DepthKeys> {
        self.inline.iter().chain(self.spill.iter())
    }
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut DepthKeys> {
        self.inline.iter_mut().chain(self.spill.iter_mut())
    }
    fn reserve(&mut self, additional: usize, offset: u32) -> Result<()> {
        reserve(&mut self.spill, additional, offset)
    }
    fn resize_with(&mut self, len: usize, make: impl FnMut() -> DepthKeys) {
        if len > self.inline.len() {
            self.spill.resize_with(len - self.inline.len(), make);
        }
    }
}
impl core::ops::Index<usize> for DepthTables {
    type Output = DepthKeys;
    fn index(&self, index: usize) -> &DepthKeys {
        if index < self.inline.len() {
            &self.inline[index]
        } else {
            &self.spill[index - self.inline.len()]
        }
    }
}
impl core::ops::IndexMut<usize> for DepthTables {
    fn index_mut(&mut self, index: usize) -> &mut DepthKeys {
        self.get_mut(index).expect("duplicate depth was reserved")
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
    stack: ContainerStack,
    seen: DepthTables,
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
        for depth in self.seen.iter_mut() {
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
    prepare_fresh_buffers(raw, limits, scratch)?;
    // The specialized grammar handles general JSON too. BlockCursor remains
    // its authority for escaped strings, uncertain numbers, and diagnostics.
    let result = fast::build_inner(raw, limits, numeric, scratch).or_else(|_| {
        // Clean inputs never replay. On a rejected candidate, restore the
        // established builder's exact diagnostic and error precedence.
        scratch.clear_keep_capacity();
        build_inner(raw, limits, numeric, scratch)
    });
    if result.is_err() {
        scratch.clear_keep_capacity();
    }
    result
}

/// Reserve the tape from source density without keeping a half-input sized
/// tape for ordinary JSON. Count only the source spans that must be decoded;
/// their raw lengths are a safe upper bound for the cooked arena.
fn prepare_fresh_buffers(raw: &[u8], limits: Limits, scratch: &mut DomScratch) -> Result<()> {
    if scratch.tape.capacity() == 0 {
        let words = initial_tape_words(raw)
            .clamp(16, 1_048_576)
            .min(limits.max_payload_bytes as usize);
        reserve(&mut scratch.tape, words, 0)?;
    }
    if scratch.decoded.capacity() == 0 && raw.contains(&b'\\') {
        let decoded = decoded_reservation(raw).min(limits.max_decoded_bytes as usize);
        if decoded != 0 {
            reserve(&mut scratch.decoded, decoded, 0)?;
        }
    }
    Ok(())
}

/// A bounded token-density sample sizes storage only. It neither accepts JSON
/// nor replaces any parser checks; unusual later shapes grow normally.
fn initial_tape_words(raw: &[u8]) -> usize {
    let sample = &raw[..raw.len().min(4096)];
    if sample.is_empty() {
        return 16;
    }
    let mut words = 0usize;
    let mut at = 0;
    while at < sample.len() {
        match sample[at] {
            b'"' => {
                words += 2;
                at += 1;
                let mut escaped = false;
                while at < sample.len() {
                    match sample[at] {
                        b'\\' => {
                            escaped = true;
                            at += 2;
                        }
                        b'"' => break,
                        _ => at += jmax_dispatch::short::string_special(&sample[at..]).max(1),
                    }
                }
                let mut next = at.saturating_add(1);
                while sample
                    .get(next)
                    .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                {
                    next += 1;
                }
                if sample.get(next) == Some(&b':') {
                    words -= 1;
                }
                if escaped {
                    words += 2;
                }
            }
            b'{' | b'[' => words += 3,
            b'-' | b'0'..=b'9' => {
                words += 2;
                while sample.get(at + 1).is_some_and(|b| {
                    b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')
                }) {
                    at += 1;
                }
            }
            b'n' | b't' | b'f' => {
                words += 2;
                while sample.get(at + 1).is_some_and(u8::is_ascii_lowercase) {
                    at += 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    // Fifteen percent slack covers sample variance and extended records.
    ((words as u64 * raw.len() as u64 * 115 / sample.len() as u64).div_ceil(100))
        .min(usize::MAX as u64) as usize
}

/// Sum raw interiors of escaped strings. This is conservative for UTF-8 and
/// Unicode escapes, and avoids reserving the whole input for a lone escape.
fn decoded_reservation(raw: &[u8]) -> usize {
    let mut total = 0usize;
    let mut at = 0usize;
    while at < raw.len() {
        if raw[at] != b'"' {
            at += next_quote(&raw[at..]);
            continue;
        }
        let start = at + 1;
        at = start;
        let mut escaped = false;
        while at < raw.len() {
            match raw[at] {
                b'\\' => {
                    escaped = true;
                    at = at.saturating_add(2).min(raw.len());
                }
                b'"' => {
                    if escaped {
                        total = total.saturating_add(at - start);
                    }
                    at += 1;
                    break;
                }
                _ => at += jmax_dispatch::short::string_special(&raw[at..]).max(1),
            }
        }
    }
    total
}

fn next_quote(raw: &[u8]) -> usize {
    const LOW: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut at = 0;
    while raw.len() - at >= 8 {
        let word = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap_or_default())
            ^ (LOW * u64::from(b'"'));
        let mask = word.wrapping_sub(LOW) & !word & HIGH;
        if mask != 0 {
            return at + (mask.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    at + raw[at..]
        .iter()
        .position(|&b| b == b'"')
        .unwrap_or(raw.len() - at)
}
fn build_inner(
    raw: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<Summary> {
    let mut cursor = BlockCursor::new(raw, limits, numeric, DuplicatePolicyOwner::Syntax)?;
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
            scratch.stack.reserve(1, offset)?;
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
// Both builders share this hot operation. Keep it inline: an out-of-line call
// per key costs more than the common one-probe duplicate-table lookup itself.
#[inline(always)]
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
        scratch.seen.reserve(additional, offset)?;
        scratch.seen.resize_with(depth + 1, DepthKeys::default);
    }
    let table = &mut scratch.seen[depth];
    if table.len < 6 {
        for entry in table.inline.iter().take(table.len).copied() {
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
        }
        table.inline[table.len] = SeenKey {
            generation: table.generation,
            span: reference.span,
            source: reference.source,
            hash,
        };
        table.len += 1;
        return Ok(());
    }
    if table.len == 6 {
        let prefix: [SeenKey; 6] = table.inline[..6].try_into().unwrap_or([SeenKey::EMPTY; 6]);
        table.inline[..6].fill(SeenKey::EMPTY);
        for entry in prefix {
            insert_seen(&mut table.inline, entry);
        }
    }
    if table.len < 64 {
        let mut slot = hash as usize & (table.inline.len() - 1);
        loop {
            let entry = table.inline[slot];
            if entry.generation != table.generation {
                table.inline[slot] = SeenKey {
                    generation: table.generation,
                    span: reference.span,
                    source: reference.source,
                    hash,
                };
                table.len += 1;
                return Ok(());
            }
            if entry.hash == hash
                && entry.span.len == reference.span.len
                && string_bytes(raw, &scratch.decoded, entry.span, entry.source)
                    == string_bytes(raw, &scratch.decoded, reference.span, reference.source)
            {
                return Err(syntax_error(
                    ErrorCode::DuplicateObjectKey,
                    offset,
                    "duplicate decoded JSON object key",
                ));
            }
            slot = (slot + 1) & (table.inline.len() - 1);
        }
    }
    if table.len == 64 {
        let mut slots = if table.slots.len() < 512 {
            let mut slots = Vec::new();
            reserve(&mut slots, 512, offset)?;
            slots.resize(512, SeenKey::EMPTY);
            slots
        } else {
            let mut slots = core::mem::take(&mut table.slots);
            slots
        };
        for entry in table
            .inline
            .iter()
            .copied()
            .filter(|entry| entry.generation == table.generation)
        {
            insert_seen(&mut slots, entry);
        }
        table.slots = slots;
    }
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
/// Constant work per key (length, head word, tail word) instead of FNV-1a's
/// dependent multiply per byte or a pass over every word.
#[inline(always)]
fn key_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let len = bytes.len();
    // Constant work per key: length plus a head word and a tail word, which
    // overlap (and so cover every byte) up to 16 bytes. Bytes beyond that in
    // the middle only matter to the byte comparison that confirms a match.
    let (head, tail) = if len >= 8 {
        (load_u64(bytes, 0), load_u64(bytes, len - 8))
    } else if len >= 4 {
        let word = u64::from(load_u32(bytes, 0)) | u64::from(load_u32(bytes, len - 4)) << 32;
        (word, word)
    } else if len > 0 {
        let word =
            u64::from(bytes[0]) | u64::from(bytes[len >> 1]) << 8 | u64::from(bytes[len - 1]) << 16;
        (word, word)
    } else {
        (0, 0)
    };
    let mut hash = ((len as u64).wrapping_mul(K) ^ head).wrapping_mul(K);
    hash ^= hash >> 32;
    hash = (hash ^ tail).wrapping_mul(K);
    hash ^ (hash >> 32)
}

#[inline(always)]
fn load_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[inline(always)]
fn load_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn direct_general_corpora_match_every_reference_word_and_decoded_byte() -> Result<()> {
        for name in [
            "twitter",
            "citm_catalog",
            "canada",
            "github_events",
            "apache_builds",
            "endpoint_batch",
        ] {
            let path = alloc::format!(
                "{}/../../bench/data/{name}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let Ok(raw) = std::fs::read(path) else {
                continue;
            };
            for reject_duplicate_keys in [false, true] {
                let mut limits = Limits::strict_default();
                limits.reject_duplicate_keys = reject_duplicate_keys;
                let mut direct = DomScratch::default();
                let mut reference = DomScratch::default();
                let a = fast::build_inner(&raw, limits, None, &mut direct)?;
                let b = build_inner(&raw, limits, None, &mut reference)?;
                assert_eq!(direct.tape, reference.tape, "{name}");
                assert_eq!(direct.decoded, reference.decoded, "{name}");
                assert_eq!(
                    (a.nodes, a.containers, a.max_depth),
                    (b.nodes, b.containers, b.max_depth)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn swar_candidate_without_boundary_keeps_non_ascii_state() {
        for at in 0..8 {
            let mut raw = *b"\"aaaaaaaaaaaaa\"";
            raw[at + 1] = 0xff;
            let mut a = DomScratch::default();
            let mut b = DomScratch::default();
            let actual = fast::build_inner(&raw, Limits::strict_default(), None, &mut a).err();
            let expected = build_inner(&raw, Limits::strict_default(), None, &mut b).err();
            assert_eq!(actual, expected);
        }
    }

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
        assert_eq!(scratch.duplicate_capacity(), 0);
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
        assert_eq!(scratch.duplicate_capacity(), 0);
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
    #[test]
    fn owned_and_borrowed_builders_keep_identical_tape_records() -> Result<()> {
        let cases: &[&[u8]] = &[
            br#"{"name":"Ada","active":true,"items":[1,-2,null]}"#,
            br#"{"escaped":"line\nfeed","unicode":"\u03bb","empty":{}}"#,
            r#"["é",{"k":0.25e2},false]"#.as_bytes(),
        ];
        for raw in cases {
            let mut direct = DomScratch::default();
            let mut reference = DomScratch::default();
            let a = fast::build_inner(raw, Limits::strict_default(), None, &mut direct)?;
            let b = build_inner(raw, Limits::strict_default(), None, &mut reference)?;
            assert_eq!(direct.tape, reference.tape);
            assert_eq!(direct.decoded, reference.decoded);
            assert_eq!(
                (a.nodes, a.containers, a.max_depth),
                (b.nodes, b.containers, b.max_depth)
            );
            let owned = parse(Payload::from_slice(raw), Limits::strict_default())?;
            let borrowed = parse_borrowed(raw, Limits::strict_default())?;
            assert_eq!(owned.tape(), borrowed.tape());
            assert_eq!(owned.decoded(), borrowed.decoded());
            assert_eq!(owned.metadata(), borrowed.metadata());
        }
        Ok(())
    }

    #[test]
    fn direct_tokens_preserve_reference_errors_and_numeric_policy() {
        let cases: &[&[u8]] = &[
            br#"{"k":1,"k":2}"#,
            br#"[1,]"#,
            br#"[01]"#,
            b"[1e309]",
            b"[-0]",
            b"[true:false]",
            b"[nullx]",
            b"[\"\xff\"]",
            br#"["\ud800"]"#,
            br#"["unterminated]"#,
            b"{\"x\":1} false",
        ];
        for raw in cases {
            for depth in [0, 1, 128] {
                let mut limits = Limits::strict_default();
                limits.max_depth = depth;
                let numeric = Some(NumericPolicy::strict_default());
                let a = fast::build_inner(raw, limits, numeric, &mut DomScratch::default());
                let b = build_inner(raw, limits, numeric, &mut DomScratch::default());
                match (a, b) {
                    (Err(a), Err(b)) => assert_eq!(a, b, "{raw:?}"),
                    (Ok(a), Ok(b)) => assert_eq!((a.nodes, a.containers), (b.nodes, b.containers)),
                    _ => panic!("direct/reference acceptance differs for {raw:?}"),
                }
            }
        }
    }

    #[test]
    fn duplicate_and_limit_failures_match_across_payload_owners() {
        let duplicate = br#"{"key":1,"key":2}"#;
        let owned = parse(Payload::from_slice(duplicate), Limits::strict_default()).unwrap_err();
        let borrowed = parse_borrowed(duplicate, Limits::strict_default()).unwrap_err();
        assert_eq!(
            (owned.code, owned.byte_offset),
            (borrowed.code, borrowed.byte_offset)
        );

        let mut limits = Limits::strict_default();
        limits.max_array_items = 1;
        let raw = b"[0,1]";
        let owned = parse(Payload::from_slice(raw), limits).unwrap_err();
        let borrowed = parse_borrowed(raw, limits).unwrap_err();
        assert_eq!(
            (owned.code, owned.byte_offset),
            (borrowed.code, borrowed.byte_offset)
        );
    }
    #[test]
    fn coordinate_rows_match_independent_builder_with_policy_and_limits() -> Result<()> {
        let valid = br#"[[-123.25,0.5],[3,4.567],[0,2.0]]"#;
        let invalid = br#"[[-123.25,0.5],x3,4]]"#;
        for raw in [valid.as_slice(), invalid.as_slice()] {
            for numeric in [None, Some(NumericPolicy::strict_default())] {
                for max_depth in [1, 2, 3, 8] {
                    for max_array_items in [1, 2, 10] {
                        let mut limits = Limits::strict_default();
                        limits.max_depth = max_depth;
                        limits.max_array_items = max_array_items;
                        let mut direct = DomScratch::default();
                        let mut reference = DomScratch::default();
                        let actual = fast::build_inner(raw, limits, numeric, &mut direct);
                        let expected = build_inner(raw, limits, numeric, &mut reference);
                        if raw == valid.as_slice() && max_depth >= 2 && max_array_items >= 3 {
                            assert!(actual.is_ok());
                        }
                        match (actual, expected) {
                            (Ok(a), Ok(b)) => {
                                assert_eq!(direct.tape, reference.tape);
                                assert_eq!(direct.decoded, reference.decoded);
                                assert_eq!(
                                    (a.nodes, a.containers, a.max_depth),
                                    (b.nodes, b.containers, b.max_depth)
                                );
                            }
                            (Err(a), Err(b)) => assert_eq!(a, b),
                            _ => panic!("coordinate/reference acceptance differs"),
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
