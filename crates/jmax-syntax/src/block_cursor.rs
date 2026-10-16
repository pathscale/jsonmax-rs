//! Complete-slice lexical cursor. Full blocks are borrowed directly from input;
//! only escaped output and bounded grammar/duplicate state need storage.

use super::*;
use jmax_abi::number::NumericPolicy;
use jmax_abi::scan::{BlockMasks, ScanCarry};
use jmax_abi::syntax::SkipMode;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CursorCache {
    masks: BlockMasks,
    block_start: u32,
    block_end: u32,
    carry: ScanCarry,
    offset: u32,
    dispatch: jmax_dispatch::Dispatch,
    escape_masker: jmax_string::EscapeMasker,
    discarded_decoded: u32,
}

/// Reusable strict-discard frames and per-depth duplicate-key tables.
/// Storage grows for new shapes and remains available across skipped subtrees
/// and documents. No input or decoded string bytes are retained here.
#[derive(Debug, Default)]
pub struct SkipScratch {
    frames: Vec<Frame>,
    tables: Vec<frames::KeyTable>,
}
pub(crate) type DiscardScratch = SkipScratch;

/// Persistent parser over an exact borrowed, immutable complete input slice.
/// Lexical methods consume one token. `next_event` additionally owns grammar
/// state; callers doing their own grammar must use the lexical methods instead.
#[derive(Debug)]
pub struct BlockCursor<'a> {
    input: &'a [u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    dup_owner: DuplicatePolicyOwner,
    cache: CursorCache,
    frames: Vec<Frame>,
    key_tables: Vec<frames::KeyTable>,
    root_phase: FramePhase,
    finished: bool,
    failure: Option<Error>,
    query_skipped: bool,
    pub(crate) discard: DiscardScratch,
    scratch: Option<&'a mut SkipScratch>,
}

impl Drop for BlockCursor<'_> {
    fn drop(&mut self) {
        if let Some(scratch) = self.scratch.take() {
            self.discard.frames.clear();
            *scratch = core::mem::take(&mut self.discard);
        }
    }
}

pub(crate) fn check_duplicate_owner(limits: Limits, owner: DuplicatePolicyOwner) -> Result<()> {
    if limits.reject_duplicate_keys && owner == DuplicatePolicyOwner::Disabled {
        return Err(syntax_error(
            ErrorCode::ConstraintViolation,
            0,
            "duplicate rejection requires a syntax or schema VM owner",
        ));
    }
    Ok(())
}

impl<'a> BlockCursor<'a> {
    pub fn new(
        input: &'a [u8],
        limits: Limits,
        numeric: Option<NumericPolicy>,
        dup_owner: DuplicatePolicyOwner,
    ) -> Result<Self> {
        frames::check_payload_growth(0, input.len(), limits)?;
        check_duplicate_owner(limits, dup_owner)?;
        if numeric.is_some_and(|policy| policy.max_exponent_abs < 0) {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                0,
                "numeric exponent limit cannot be negative",
            ));
        }
        let dispatch = jmax_dispatch::Dispatch::new(input.len(), Default::default())?;
        Ok(Self::from_cache(
            input,
            limits,
            numeric,
            dup_owner,
            CursorCache {
                masks: BlockMasks::default(),
                block_start: 0,
                block_end: 0,
                carry: ScanCarry::default(),
                offset: 0,
                dispatch,
                escape_masker: jmax_string::EscapeMasker::new(),
                discarded_decoded: 0,
            },
        ))
    }

    /// Uses reusable strict-skip storage, returning it automatically on drop,
    /// including after lexical, policy, resource-limit, or allocation errors.
    pub fn with_scratch(
        input: &'a [u8],
        limits: Limits,
        numeric: Option<NumericPolicy>,
        dup_owner: DuplicatePolicyOwner,
        scratch: &'a mut SkipScratch,
    ) -> Result<Self> {
        let mut cursor = Self::new(input, limits, numeric, dup_owner)?;
        cursor.discard = core::mem::take(scratch);
        cursor.scratch = Some(scratch);
        Ok(cursor)
    }

    pub(crate) fn from_cache(
        input: &'a [u8],
        limits: Limits,
        numeric: Option<NumericPolicy>,
        dup_owner: DuplicatePolicyOwner,
        cache: CursorCache,
    ) -> Self {
        Self {
            input,
            limits,
            numeric,
            dup_owner,
            cache,
            frames: Vec::new(),
            key_tables: Vec::new(),
            root_phase: FramePhase::RootStart,
            finished: false,
            failure: None,
            query_skipped: false,
            discard: DiscardScratch::default(),
            scratch: None,
        }
    }
    pub(crate) const fn cache(&self) -> CursorCache {
        self.cache
    }
    pub const fn input(&self) -> &'a [u8] {
        self.input
    }
    #[inline(always)]
    pub const fn offset(&self) -> u32 {
        self.cache.offset
    }
    pub const fn limits(&self) -> Limits {
        self.limits
    }
    pub const fn numeric_policy(&self) -> Option<NumericPolicy> {
        self.numeric
    }
    pub const fn duplicate_owner(&self) -> DuplicatePolicyOwner {
        self.dup_owner
    }
    pub const fn is_finished(&self) -> bool {
        self.finished
    }
    /// False after any query-only structural skip, even if EOF is reached.
    pub const fn fully_validated(&self) -> bool {
        self.finished
            && !self.query_skipped
            && !matches!(self.dup_owner, DuplicatePolicyOwner::SchemaVm)
    }
    pub fn failure(&self) -> Option<&Error> {
        self.failure.as_ref()
    }
    pub fn state(&self) -> CursorState {
        CursorState {
            byte_offset: self.offset(),
            depth: self.frames.len() as u16,
            phase: Some(self.phase()),
            finished: self.finished,
        }
    }
    #[inline]
    fn phase(&self) -> FramePhase {
        self.frames
            .last()
            .map_or(self.root_phase, |frame| frame.phase)
    }
    #[inline]
    fn check_failure(&self) -> Result<()> {
        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
    #[inline]
    fn retain<T>(&mut self, result: Result<T>) -> Result<T> {
        result.map_err(|error| self.failure.get_or_insert(error).clone())
    }
    #[inline(always)]
    pub fn peek_byte(&self) -> Option<u8> {
        self.input.get(self.offset() as usize).copied()
    }
    /// Advances within the slice. Over-advancing clamps to EOF; lexical methods
    /// and `expect_byte` provide checked token consumption.
    #[inline(always)]
    pub fn advance(&mut self, n: u32) {
        if self.failure.is_none() {
            self.cache.offset = self.offset().saturating_add(n).min(self.input.len() as u32);
        }
    }
    #[inline]
    pub fn expect_byte(&mut self, expected: u8, detail: &'static str) -> Result<u32> {
        self.check_failure()?;
        let byte = self.peek_byte();
        let result = if byte == Some(expected) {
            let offset = self.offset();
            self.advance(1);
            Ok(offset)
        } else {
            Err(syntax_error(
                if byte.is_none() {
                    ErrorCode::UnexpectedEof
                } else {
                    ErrorCode::InvalidSyntax
                },
                self.offset(),
                detail,
            ))
        };
        self.retain(result)
    }

    /// Ensures the active block covers offset, classifying intervening blocks
    /// when a lexical consumer advanced across them. Carry always flows forward.
    #[inline]
    fn refill(&mut self) -> Result<()> {
        let position = self.offset();
        if position >= self.cache.block_start && position < self.cache.block_end {
            return Ok(());
        }
        if position < self.cache.block_end || position as usize >= self.input.len() {
            return Err(syntax_error(
                ErrorCode::InternalError,
                position,
                "invalid block cursor refill",
            ));
        }
        while self.cache.block_end <= position {
            let start = self.cache.block_end as usize;
            let end = (start + 64).min(self.input.len());
            let bytes = &self.input[start..end];
            let masks = if bytes.len() == 64 {
                let block: &[u8; 64] = bytes.try_into().map_err(|_| {
                    syntax_error(
                        ErrorCode::InternalError,
                        start as u32,
                        "invalid full scanner block",
                    )
                })?;
                self.cache
                    .dispatch
                    .classify_block(block, &mut self.cache.carry)?
            } else {
                self.cache.dispatch.classify(bytes, &mut self.cache.carry)?
            };
            self.cache.masks = masks;
            self.cache.block_start = start as u32;
            self.cache.block_end = end as u32;
        }
        Ok(())
    }
    #[inline]
    pub fn skip_whitespace(&mut self) -> Result<Option<u8>> {
        self.check_failure()?;
        let result = (|| {
            while self.offset() < self.input.len() as u32 {
                self.refill()?;
                let relative = self.offset() - self.cache.block_start;
                let remaining =
                    (!self.cache.masks.whitespace_mask & self.cache.masks.valid_mask) >> relative;
                if remaining != 0 {
                    self.cache.offset += remaining.trailing_zeros();
                    return Ok(self.peek_byte());
                }
                self.cache.offset = self.cache.block_end;
            }
            Ok(None)
        })();
        self.retain(result)
    }

    #[inline]
    pub fn scan_string_meta(&mut self, decoded: &mut Vec<u8>) -> Result<StringMeta> {
        self.check_failure()?;
        let checkpoint = decoded.len();
        let result = self.scan_string_inner(decoded, true);
        if result.is_err() {
            decoded.truncate(checkpoint);
        }
        self.retain(result)
    }
    #[inline]
    fn scan_string_inner(&mut self, decoded: &mut Vec<u8>, capture: bool) -> Result<StringMeta> {
        let start = self.expect_byte(b'"', "expected string opening quote")?;
        let content_start = self.offset();
        let mut backslash = 0u64;
        let mut non_ascii = 0u64;
        let mut first_control = None;
        let mut closed = false;
        while self.offset() < self.input.len() as u32 {
            self.refill()?;
            let masks = self.cache.masks;
            let remaining =
                masks.valid_mask & (u64::MAX << (self.offset() - self.cache.block_start));
            let quotes = masks.delimiter_quotes() & remaining;
            let end = if quotes == 0 {
                self.cache.block_end
            } else {
                self.cache.block_start + quotes.trailing_zeros()
            };
            let content = remaining & low_bits(end - self.cache.block_start);
            backslash |= masks.backslash_mask & content;
            non_ascii |= masks.non_ascii_mask & content;
            let controls = masks.control_mask & content;
            if first_control.is_none() && controls != 0 {
                first_control = Some(self.cache.block_start + controls.trailing_zeros());
            }
            self.cache.offset = end;
            if quotes != 0 {
                closed = true;
                break;
            }
        }
        let content_end = self.offset();
        let raw_content = Span32::new(content_start, content_end - content_start);
        // The classifier already proved the common ASCII case. Avoid creating
        // another checked slice or entering UTF-8/error arbitration for it.
        if backslash == 0
            && non_ascii == 0
            && first_control.is_none()
            && raw_content.len <= self.limits.max_string_bytes
            && closed
        {
            self.cache.offset += 1;
            return Ok(StringMeta {
                raw_content,
                raw_token: Span32::new(start, self.offset() - start),
                cooked: StrRef {
                    span: raw_content,
                    source: SpanSource::RawPayload,
                    scalar_len: raw_content.len,
                },
                is_unescaped: true,
                is_ascii: true,
            });
        }
        let (cooked, is_ascii) = if backslash == 0 {
            // ASCII: no byte loop and no output write. Unicode: a single bulk
            // validation/scalar-count pass over the original borrowed content.
            let validated = if non_ascii == 0 {
                Ok(raw_content.len)
            } else {
                jmax_utf8::validate_and_count_scalars(
                    &self.input[content_start as usize..content_end as usize],
                    content_start,
                )
            };
            let mut error = validated.as_ref().err().cloned();
            if let Some(offset) = first_control {
                earlier(
                    &mut error,
                    lexical(
                        ErrorCode::ControlCharInString,
                        offset,
                        "unescaped control byte in string",
                    ),
                );
            }
            if raw_content.len > self.limits.max_string_bytes {
                earlier(
                    &mut error,
                    syntax_error(
                        ErrorCode::ConstraintViolation,
                        content_start + self.limits.max_string_bytes,
                        "raw string byte limit exceeded",
                    ),
                );
            }
            if let Some(error) = error {
                return Err(error);
            }
            (
                StrRef {
                    span: raw_content,
                    source: SpanSource::RawPayload,
                    scalar_len: validated?,
                },
                non_ascii == 0,
            )
        } else {
            let mut limits = self.limits;
            limits.max_decoded_bytes = limits
                .max_decoded_bytes
                .saturating_sub(self.cache.discarded_decoded);
            let (cooked, ascii) = decode_content(
                self.input,
                start,
                content_end,
                closed,
                limits,
                decoded,
                capture,
                self.cache.escape_masker,
            )?;
            if !capture {
                self.cache.discarded_decoded += cooked.span.len;
            }
            (cooked, ascii)
        };
        if !closed {
            return Err(lexical(
                ErrorCode::UnclosedString,
                start,
                "string has no closing quote",
            ));
        }
        self.advance(1);
        Ok(StringMeta {
            raw_content,
            raw_token: Span32::new(start, self.offset() - start),
            cooked,
            is_unescaped: backslash == 0,
            is_ascii,
        })
    }
    #[inline]
    pub fn scan_key_meta(&mut self, decoded: &mut Vec<u8>, compute_hash: bool) -> Result<KeyMeta> {
        let string = self.scan_string_meta(decoded)?;
        let bytes = match string.cooked.source {
            SpanSource::RawPayload => string.cooked.span.slice(self.input),
            SpanSource::DecodedArena => string.cooked.span.slice(decoded),
        }
        .ok_or_else(|| {
            syntax_error(
                ErrorCode::InternalError,
                string.raw_token.offset,
                "cooked key reference outside arena",
            )
        })?;
        Ok(KeyMeta {
            string,
            swar_head: KeyMeta::compute_swar_head(bytes),
            cooked_hash: if compute_hash {
                KeyMeta::compute_fnv1a64(bytes)
            } else {
                0
            },
        })
    }

    /// Discovers an atom and counts digit bits during the same mask traversal.
    #[inline]
    fn atom_span(&mut self) -> Result<(Span32, Option<u32>, u32)> {
        let start = self.offset();
        let mut digits = 0u32;
        let mut excess = None;
        while self.offset() < self.input.len() as u32 {
            self.refill()?;
            let masks = self.cache.masks;
            let remaining =
                masks.valid_mask & (u64::MAX << (self.offset() - self.cache.block_start));
            let boundaries =
                (masks.structural_mask | masks.raw_punctuation_mask() | masks.whitespace_mask)
                    & remaining;
            let end = if boundaries == 0 {
                self.cache.block_end
            } else {
                self.cache.block_start + boundaries.trailing_zeros()
            };
            let mut digit_bits =
                masks.digit_mask & remaining & low_bits(end - self.cache.block_start);
            let count = digit_bits.count_ones();
            let allowed = u32::from(self.limits.max_number_digits).saturating_sub(digits);
            if excess.is_none() && count > allowed {
                for _ in 0..allowed {
                    digit_bits &= digit_bits - 1;
                }
                excess = Some(self.cache.block_start + digit_bits.trailing_zeros());
            }
            digits += count;
            self.cache.offset = end;
            if boundaries != 0 {
                break;
            }
        }
        Ok((Span32::new(start, self.offset() - start), excess, digits))
    }
    #[inline]
    pub fn scan_number_span(&mut self) -> Result<Span32> {
        self.check_failure()?;
        let result = (|| {
            let (span, excess, digits) = self.atom_span()?;
            // A mask-proven nonzero-leading integer needs no second byte/UTF-8
            // traversal. Floats, leading zeros, malformed boundaries and
            // negative-zero policy errors retain the full numeric validator.
            let bytes = &self.input[span.offset as usize..self.offset() as usize];
            let unsigned = digits == span.len
                && (bytes.first().is_some_and(|&byte| byte != b'0') || span.len == 1);
            let signed = span.len >= 2
                && digits == span.len - 1
                && bytes[0] == b'-'
                && (bytes[1] != b'0'
                    || (span.len == 2
                        && !self
                            .numeric
                            .is_some_and(|policy| policy.reject_negative_zero)));
            if (unsigned || signed)
                && self
                    .peek_byte()
                    .is_none_or(jmax_number::is_number_delimiter)
            {
                if let Some(offset) = excess {
                    return Err(syntax_error(
                        ErrorCode::ConstraintViolation,
                        offset,
                        "number digit limit exceeded",
                    ));
                }
                return Ok(span);
            }
            super::feed::validate_number_slice(self.input, span, self.numeric, excess)?;
            Ok(span)
        })();
        self.retain(result)
    }
    pub fn scan_literal(&mut self) -> Result<(TokenKind, Span32)> {
        self.check_failure()?;
        let result = (|| {
            let start = self.offset();
            let (kind, spelling): (TokenKind, &[u8]) = match self.peek_byte() {
                Some(b't') => (TokenKind::True, b"true"),
                Some(b'f') => (TokenKind::False, b"false"),
                Some(b'n') => (TokenKind::Null, b"null"),
                _ => {
                    return Err(lexical(
                        ErrorCode::InvalidToken,
                        start,
                        "invalid literal start",
                    ));
                }
            };
            for &expected in spelling {
                match self.peek_byte() {
                    Some(byte) if byte == expected => self.advance(1),
                    Some(_) => {
                        return Err(lexical(
                            ErrorCode::InvalidToken,
                            self.offset(),
                            "invalid literal or literal boundary",
                        ));
                    }
                    None => {
                        return Err(lexical(
                            ErrorCode::UnexpectedEof,
                            self.offset(),
                            "incomplete literal",
                        ));
                    }
                }
            }
            if self
                .peek_byte()
                .is_some_and(|byte| !jmax_number::is_number_delimiter(byte))
            {
                return Err(lexical(
                    ErrorCode::InvalidToken,
                    self.offset(),
                    "invalid literal or literal boundary",
                ));
            }
            Ok((kind, Span32::new(start, self.offset() - start)))
        })();
        self.retain(result)
    }

    pub(crate) fn next_token(
        &mut self,
        decoded: &mut Vec<u8>,
        key: bool,
        discard: bool,
    ) -> Result<(Token, Option<u64>)> {
        let start = self.offset();
        let kind = match self.peek_byte() {
            Some(b'{') => TokenKind::LBrace,
            Some(b'}') => TokenKind::RBrace,
            Some(b'[') => TokenKind::LBracket,
            Some(b']') => TokenKind::RBracket,
            Some(b':') => TokenKind::Colon,
            Some(b',') => TokenKind::Comma,
            Some(b'"') => {
                let hash = key
                    && self.limits.reject_duplicate_keys
                    && self.dup_owner == DuplicatePolicyOwner::Syntax;
                let (string, cooked_hash) = if key && hash {
                    let meta = self.scan_key_meta(decoded, true)?;
                    (meta.string, Some(meta.cooked_hash))
                } else {
                    let meta = if discard {
                        self.scan_string_inner(decoded, false)?
                    } else {
                        self.scan_string_meta(decoded)?
                    };
                    (meta, None)
                };
                return Ok((
                    Token {
                        kind: TokenKind::String,
                        span: string.raw_token,
                        decoded: Some(string.cooked),
                    },
                    cooked_hash,
                ));
            }
            Some(b'-' | b'0'..=b'9') => {
                return Ok((
                    Token {
                        kind: TokenKind::Number,
                        span: self.scan_number_span()?,
                        decoded: None,
                    },
                    None,
                ));
            }
            Some(b't' | b'f' | b'n') => {
                let (kind, span) = self.scan_literal()?;
                return Ok((
                    Token {
                        kind,
                        span,
                        decoded: None,
                    },
                    None,
                ));
            }
            Some(_) => {
                return Err(lexical(
                    ErrorCode::InvalidToken,
                    start,
                    "invalid token start",
                ));
            }
            None => {
                return Err(syntax_error(
                    ErrorCode::UnexpectedEof,
                    start,
                    "expected JSON value",
                ));
            }
        };
        self.advance(1);
        Ok((
            Token {
                kind,
                span: Span32::new(start, 1),
                decoded: None,
            },
            None,
        ))
    }

    /// Fused complete-input event traversal, checking each grammar transition once.
    pub fn next_event(&mut self, decoded: &mut Vec<u8>) -> Result<ScanStep> {
        self.check_failure()?;
        if self.finished {
            return Ok(ScanStep::Finished);
        }
        let checkpoint = decoded.len();
        let result = self.next_event_inner(decoded, false);
        if result.is_err() {
            decoded.truncate(checkpoint);
        }
        self.retain(result)
    }
    fn next_event_inner(&mut self, decoded: &mut Vec<u8>, discard: bool) -> Result<ScanStep> {
        loop {
            if self.skip_whitespace()?.is_none() {
                if self.frames.is_empty() && self.root_phase == FramePhase::RootDone {
                    self.finished = true;
                    return Ok(ScanStep::Finished);
                }
                return Err(syntax_error(
                    ErrorCode::UnexpectedEof,
                    self.offset(),
                    "incomplete JSON document",
                ));
            }
            if self.frames.is_empty() && self.root_phase == FramePhase::RootDone {
                return Err(syntax_error(
                    ErrorCode::TrailingContent,
                    self.offset(),
                    "content follows the root value",
                ));
            }
            let key = matches!(
                self.phase(),
                FramePhase::ObjectStart | FramePhase::ObjectKeyOrEnd
            );
            let (token, hash) = self.next_token(decoded, key, discard)?;
            let transition = machine::transition(self.phase(), token)?;
            let reclaim = if discard && transition.action == FrameAction::Pop {
                self.frames.last().map(|frame| frame.decoded_checkpoint)
            } else {
                None
            };
            frames::apply_parts(
                self.limits,
                self.dup_owner,
                self.input,
                decoded,
                &mut self.frames,
                &mut self.key_tables,
                &mut self.root_phase,
                token,
                transition,
                hash,
            )?;
            if let Some(checkpoint) = reclaim {
                // The closed object's duplicate table is no longer consulted.
                // Preserve cumulative resource accounting while returning the
                // physical arena to its parent's live-key checkpoint.
                self.cache.discarded_decoded = self
                    .cache
                    .discarded_decoded
                    .saturating_add((decoded.len() - checkpoint) as u32);
                decoded.truncate(checkpoint);
            }
            if let Some(event) = transition.event {
                return Ok(ScanStep::Emit(event));
            }
        }
    }

    pub fn finish_eof(&mut self) -> Result<()> {
        self.check_failure()?;
        let result = match self.skip_whitespace()? {
            None => Ok(()),
            Some(_) => Err(syntax_error(
                ErrorCode::TrailingContent,
                self.offset(),
                "content follows the root value",
            )),
        };
        self.retain(result)
    }

    pub fn skip_value(
        &mut self,
        decoded: &mut Vec<u8>,
        current_depth: u16,
        mode: SkipMode,
    ) -> Result<Span32> {
        self.check_failure()?;
        let checkpoint = decoded.len();
        let result = (|| {
            self.skip_whitespace()?;
            let start = self.offset();
            match mode {
                SkipMode::StrictValidate => self.skip_strict_inner(decoded, current_depth)?,
                SkipMode::QueryOnly => {
                    self.query_skipped = true;
                    self.skip_query_inner(current_depth, false)?;
                }
                SkipMode::ValidatedOwner(capability) => {
                    // A syntax cursor has no schema engine. Only syntax-scope
                    // capabilities (schema 0) authorize this entry point.
                    if self.numeric.is_some()
                        || !capability.matches_slice(
                            self.input,
                            capability.decoded_len() as usize,
                            0,
                            self.limits.policy_fingerprint(),
                        )
                    {
                        return Err(syntax_error(
                            ErrorCode::ConstraintViolation,
                            start,
                            "validated skip capability does not match slice or policy",
                        ));
                    }
                    self.skip_query_inner(current_depth, true)?;
                }
            }
            Ok(Span32::new(start, self.offset() - start))
        })();
        if result.is_err() {
            decoded.truncate(checkpoint);
        }
        self.retain(result)
    }

    /// Skip immediately after `next_event` emitted an opening container.
    /// The parent's grammar phase/count was committed by the opening event.
    pub fn skip_open_container(&mut self, decoded: &mut Vec<u8>, mode: SkipMode) -> Result<Span32> {
        self.check_failure()?;
        let frame = self.frames.last().ok_or_else(|| {
            syntax_error(
                ErrorCode::TypeMismatch,
                self.offset(),
                "skip requires a current container start",
            )
        })?;
        if self.offset() != frame.start_offset + 1 || frame.count != 0 {
            return Err(syntax_error(
                ErrorCode::TypeMismatch,
                self.offset(),
                "container has already been traversed",
            ));
        }
        let start = frame.start_offset;
        let depth = (self.frames.len() - 1) as u16;
        self.rewind_opening(start)?;
        let span = self.skip_value(decoded, depth, mode)?;
        self.frames.pop();
        Ok(span)
    }

    pub(crate) fn rewind_opening(&mut self, start: u32) -> Result<()> {
        if start + 1 != self.offset()
            || start < self.cache.block_start
            || start >= self.cache.block_end
        {
            return Err(syntax_error(
                ErrorCode::InternalError,
                start,
                "opening token is outside active block",
            ));
        }
        self.cache.offset = start;
        Ok(())
    }

    fn skip_strict_inner(&mut self, decoded: &mut Vec<u8>, current_depth: u16) -> Result<()> {
        if current_depth > self.limits.max_depth {
            return Err(syntax_error(
                ErrorCode::DepthLimitExceeded,
                self.offset(),
                "container depth exceeds its limit",
            ));
        }
        // Reusable frames share the scanner/cache and decoded arena. They never
        // build nodes or captures, and retain capacities across skipped values.
        let saved_frames =
            core::mem::replace(&mut self.frames, core::mem::take(&mut self.discard.frames));
        let saved_tables = core::mem::replace(
            &mut self.key_tables,
            core::mem::take(&mut self.discard.tables),
        );
        let saved_phase = self.root_phase;
        let saved_finished = self.finished;
        let saved_limit = self.limits.max_depth;
        let saved_owner = self.dup_owner;
        self.dup_owner = DuplicatePolicyOwner::Syntax;
        self.root_phase = FramePhase::RootStart;
        self.finished = false;
        self.limits.max_depth -= current_depth;
        let checkpoint = decoded.len();
        let result = (|| {
            loop {
                self.next_event_inner(decoded, true)?;
                if self.frames.is_empty() && self.root_phase == FramePhase::RootDone {
                    break;
                }
            }
            Ok(())
        })();
        self.cache.discarded_decoded = self
            .cache
            .discarded_decoded
            .saturating_add((decoded.len() - checkpoint) as u32);
        decoded.truncate(checkpoint);
        self.discard.frames = core::mem::replace(&mut self.frames, saved_frames);
        self.discard.frames.clear();
        self.discard.tables = core::mem::replace(&mut self.key_tables, saved_tables);
        self.root_phase = saved_phase;
        self.finished = saved_finished;
        self.limits.max_depth = saved_limit;
        self.dup_owner = saved_owner;
        result
    }

    fn skip_query_inner(&mut self, current_depth: u16, trusted: bool) -> Result<()> {
        let first = self.peek_byte().ok_or_else(|| {
            syntax_error(
                ErrorCode::UnexpectedEof,
                self.offset(),
                "expected skipped value",
            )
        })?;
        if first != b'{' && first != b'[' {
            if first == b'"' {
                // Boundary-only string skip, with no decoded output.
                self.advance(1);
                while self.offset() < self.input.len() as u32 {
                    self.refill()?;
                    let bits = self.cache.masks.delimiter_quotes()
                        & (u64::MAX << (self.offset() - self.cache.block_start));
                    if bits != 0 {
                        self.cache.offset = self.cache.block_start + bits.trailing_zeros() + 1;
                        return Ok(());
                    }
                    self.cache.offset = self.cache.block_end;
                }
                return Err(lexical(
                    ErrorCode::UnclosedString,
                    self.offset(),
                    "skipped string has no closing quote",
                ));
            }
            if !matches!(first, b'-' | b'0'..=b'9' | b't' | b'f' | b'n') {
                return Err(lexical(
                    ErrorCode::InvalidToken,
                    self.offset(),
                    "invalid skipped value start",
                ));
            }
            self.atom_span()?;
            return Ok(());
        }
        let mut tags: Vec<u8> = Vec::new();
        let mut depth = 0u32;
        while self.offset() < self.input.len() as u32 {
            self.refill()?;
            let remaining = self.cache.masks.valid_mask
                & (u64::MAX << (self.offset() - self.cache.block_start));
            let opens = self.cache.masks.open_container_mask() & remaining;
            let closes = self.cache.masks.close_container_mask() & remaining;
            // If depth > closes.count_ones(), this block cannot end the
            // subtree. We still check ordered tags: net depth alone cannot
            // detect crossed bracket types or enforce the nesting ceiling.
            let cannot_end = depth > closes.count_ones();
            let incoming_depth = depth;
            if trusted && cannot_end {
                // The live capability already checked bracket kinds and peak
                // depth for this exact immutable slice and these limits.
                depth = depth + opens.count_ones() - closes.count_ones();
                self.cache.offset = self.cache.block_end;
                continue;
            }
            let mut bits = opens | closes;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                let offset = self.cache.block_start + bit;
                let byte = self.input[offset as usize];
                if opens & (1u64 << bit) != 0 {
                    if depth + u32::from(current_depth) >= u32::from(self.limits.max_depth) {
                        return Err(syntax_error(
                            ErrorCode::DepthLimitExceeded,
                            offset,
                            "container depth exceeds its limit",
                        ));
                    }
                    if !trusted {
                        tags.try_reserve(1).map_err(|_| {
                            syntax_error(
                                ErrorCode::BudgetExhausted,
                                offset,
                                "skip bracket stack allocation failed",
                            )
                        })?;
                        tags.push(byte);
                    }
                    depth += 1;
                } else {
                    let opening = if trusted { None } else { tags.pop() };
                    if !trusted
                        && !matches!((opening, byte), (Some(b'{'), b'}') | (Some(b'['), b']'))
                    {
                        return Err(syntax_error(
                            ErrorCode::InvalidSyntax,
                            offset,
                            "mismatched skipped container delimiter",
                        ));
                    }
                    depth -= 1;
                    if depth == 0 {
                        self.cache.offset = offset + 1;
                        return Ok(());
                    }
                }
            }
            if cannot_end {
                // Count-based update is independently checked against ordered
                // tag processing; whole byte blocks never undergo tokenization.
                depth = incoming_depth + opens.count_ones() - closes.count_ones();
                debug_assert!(trusted || depth as usize == tags.len());
            }
            self.cache.offset = self.cache.block_end;
        }
        Err(syntax_error(
            ErrorCode::UnexpectedEof,
            self.offset(),
            "input ended in skipped container",
        ))
    }
}

#[inline(always)]
fn low_bits(len: u32) -> u64 {
    if len >= 64 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    }
}
fn lexical(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Lexer, offset, detail)
}
fn earlier(current: &mut Option<Error>, candidate: Error) {
    if current
        .as_ref()
        .is_none_or(|error| candidate.byte_offset < error.byte_offset)
    {
        *current = Some(candidate);
    }
}

/// Single-pass escaped-content traversal with bulk ordinary runs. The legacy
/// escape unit is invoked only at a backslash, retaining its exact diagnostics.
fn decode_content(
    input: &[u8],
    start: u32,
    end: u32,
    closed: bool,
    limits: Limits,
    decoded: &mut Vec<u8>,
    capture: bool,
    masker: jmax_string::EscapeMasker,
) -> Result<(StrRef, bool)> {
    let checkpoint = decoded.len();
    u32::try_from(checkpoint)
        .ok()
        .filter(|&len| len <= limits.max_decoded_bytes)
        .ok_or_else(|| {
            syntax_error(
                ErrorCode::DecodedArenaOverflow,
                start,
                "decoded arena byte limit exceeded",
            )
        })?;
    let source = super::feed::StringInput {
        bytes: input,
        start,
        end,
        closed,
        raw_limit: limits.max_string_bytes,
    };
    let result = (|| {
        let mut position = start + 1;
        let content_start = position;
        let content = &input[content_start as usize..end as usize];
        let mut finder = jmax_string::EscapeMaskCursor::with_masker(content, masker);
        let mut byte_len = 0u32;
        let mut scalar_len = 0u32;
        let mut ascii = true;
        while position < end {
            let unit_start = position;
            if input[position as usize] == b'\\' {
                let scalar = source.unit(&mut position)?;
                ascii &= scalar.is_ascii();
                let mut encoded = [0u8; 4];
                let bytes = scalar.encode_utf8(&mut encoded).as_bytes();
                append_run(
                    decoded,
                    checkpoint,
                    &mut byte_len,
                    bytes,
                    limits,
                    unit_start,
                    capture,
                )?;
                scalar_len += 1;
            } else {
                let tail = &input[position as usize..end as usize];
                let relative = (position - content_start) as usize;
                let run_len = finder.next_special(relative) - relative;
                if run_len == 0 {
                    // Gives controls and raw-limit violations their legacy
                    // ordering without a loop over ordinary ASCII bytes.
                    source.unit(&mut position)?;
                    return Err(syntax_error(
                        ErrorCode::InternalError,
                        unit_start,
                        "invalid ordinary string run",
                    ));
                }
                let run = &tail[..run_len];
                let validation = jmax_utf8::validate_and_count_scalars(run, position);
                let raw_remaining =
                    limits.max_string_bytes.saturating_sub(position - start - 1) as usize;
                let decoded_remaining = limits.max_string_bytes.saturating_sub(byte_len) as usize;
                let arena_remaining = limits
                    .max_decoded_bytes
                    .saturating_sub((checkpoint as u32).saturating_add(byte_len))
                    as usize;
                let first_limit = raw_remaining.min(decoded_remaining).min(arena_remaining);
                if run_len > first_limit {
                    let limit_offset = position + first_limit as u32;
                    if let Err(error) = &validation {
                        if error.byte_offset <= limit_offset {
                            return Err(error.clone());
                        }
                    }
                    return Err(syntax_error(
                        if arena_remaining < raw_remaining.min(decoded_remaining) {
                            ErrorCode::DecodedArenaOverflow
                        } else {
                            ErrorCode::ConstraintViolation
                        },
                        limit_offset,
                        "string byte limit exceeded",
                    ));
                }
                let count = validation?;
                ascii &= count as usize == run.len();
                append_run(
                    decoded,
                    checkpoint,
                    &mut byte_len,
                    run,
                    limits,
                    position,
                    capture,
                )?;
                scalar_len += count;
                position += run_len as u32;
            }
        }
        Ok((
            StrRef {
                span: Span32::new(checkpoint as u32, byte_len),
                source: SpanSource::DecodedArena,
                scalar_len,
            },
            ascii,
        ))
    })();
    if result.is_err() {
        decoded.truncate(checkpoint);
    }
    result
}
fn append_run(
    decoded: &mut Vec<u8>,
    checkpoint: usize,
    byte_len: &mut u32,
    bytes: &[u8],
    limits: Limits,
    offset: u32,
    capture: bool,
) -> Result<()> {
    let next = byte_len
        .checked_add(bytes.len() as u32)
        .filter(|&len| len <= limits.max_string_bytes)
        .ok_or_else(|| {
            syntax_error(
                ErrorCode::ConstraintViolation,
                offset,
                "decoded string byte limit exceeded",
            )
        })?;
    if checkpoint
        .checked_add(next as usize)
        .is_none_or(|len| len > limits.max_decoded_bytes as usize)
    {
        return Err(syntax_error(
            ErrorCode::DecodedArenaOverflow,
            offset,
            "decoded arena byte limit exceeded",
        ));
    }
    if capture {
        decoded.try_reserve(bytes.len()).map_err(|_| {
            syntax_error(
                ErrorCode::BudgetExhausted,
                offset,
                "unable to reserve decoded string storage",
            )
        })?;
        decoded.extend_from_slice(bytes);
    }
    *byte_len = next;
    Ok(())
}
