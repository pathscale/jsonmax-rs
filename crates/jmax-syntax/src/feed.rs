//! Retained-input incremental JSON event feed.
//!
//! Boundary discovery walks cached 64-byte classifier masks. Complete strings are
//! validated and measured before any arena bytes are appended. All offsets in
//! the carry and emitted events are absolute offsets into their owning buffer.

use super::*;

/// Copies a chunk without parsing it. The first failure is permanent.
pub fn push(state: &mut SyntaxState, bytes: &[u8]) -> Result<()> {
    if let Some(error) = state.failure.as_ref() {
        return Err(error.clone());
    }
    let result = (|| {
        let end = offset(state.input.len())?;
        if state.final_input || state.finished {
            return Err(syntax_error(
                ErrorCode::InvalidSyntax,
                end,
                "input was already ended",
            ));
        }
        frames::check_payload_growth(state.input.len(), bytes.len(), state.limits)?;
        let input = match &mut state.input {
            InputStorage::Incremental(input) => input,
            InputStorage::Complete(_) => return Err(internal(end)),
        };
        reserve(input, bytes.len(), end)?;
        input.extend_from_slice(bytes);
        if !bytes.is_empty() {
            // A full cached block remains valid. Reclassify an extended tail
            // from its incoming carry, never from the old tail's outgoing one.
            if let Some(block) = state.block {
                if block.end - block.start < 64 {
                    state.scan_offset = block.start;
                    state.scan_carry = block.before;
                    state.block = None;
                }
            }
            state.dispatch = None;
        }
        Ok(())
    })();
    result.map_err(|error| state.fail(error))
}

/// Advances through silent punctuation and whitespace to one event or a pause.
///
/// Chunk exhaustion is always `NeedMore` until EOF is explicitly marked by the
/// finish module. An emitted event leaves the cursor immediately after its token.
pub fn next(state: &mut SyntaxState) -> Result<ScanStep> {
    if let Some(error) = state.failure.as_ref() {
        return Err(error.clone());
    }
    if state.finished {
        return Ok(ScanStep::Finished);
    }
    let result = if matches!(state.input, InputStorage::Complete(_)) {
        next_complete(state)
    } else {
        next_inner(state)
    };
    result.map_err(|error| state.fail(error))
}

fn next_inner(state: &mut SyntaxState) -> Result<ScanStep> {
    loop {
        let end = offset(state.input.len())?;
        if state.byte_offset > end {
            return Err(internal(state.byte_offset));
        }
        if state.pending.is_none() {
            while state.byte_offset < end {
                let position = state.byte_offset;
                let block = block_at(state, position)?;
                let shift = state.byte_offset - block.start;
                let valid_bits = block.masks.valid_mask >> shift;
                let masks = block.masks;
                let whitespace_mask = masks.whitespace_mask >> shift;
                let non_whitespace = !whitespace_mask & valid_bits;
                if non_whitespace != 0 {
                    state.byte_offset += non_whitespace.trailing_zeros();
                    break;
                }
                state.byte_offset = block.end;
            }
            if state.byte_offset == end {
                if !state.final_input {
                    return Ok(ScanStep::NeedMore);
                }
                if state.frames.is_empty() && state.root_phase == FramePhase::RootDone {
                    state.finished = true;
                    return Ok(ScanStep::Finished);
                }
                return Err(syntax_error(
                    ErrorCode::UnexpectedEof,
                    end,
                    "incomplete JSON document",
                ));
            }
            // Trailing content is diagnosed before inspecting its lexical form.
            if state.frames.is_empty() && state.root_phase == FramePhase::RootDone {
                return Err(syntax_error(
                    ErrorCode::TrailingContent,
                    state.byte_offset,
                    "content follows the root value",
                ));
            }
        }

        let arena_start = state.decoded.len();
        let token = match token(state)? {
            Some(token) => token,
            None => return Ok(ScanStep::NeedMore),
        };
        let result = frames::apply_transition(state, token, None);
        let transition = match result {
            Ok(transition) => transition,
            Err(error) => {
                // A rejected key/value must not leave unpublished arena bytes.
                state.decoded.truncate(arena_start);
                return Err(error);
            }
        };
        state.byte_offset = token
            .span
            .end()
            .ok_or_else(|| internal(token.span.offset))?;
        state.pending = None;
        if let Some(event) = transition.event {
            return Ok(ScanStep::Emit(event));
        }
    }
}

fn token(state: &mut SyntaxState) -> Result<Option<Token>> {
    let mut carry = if let Some(carry) = state.pending {
        carry
    } else {
        let start = state.byte_offset;
        let byte = *state
            .input
            .get(start as usize)
            .ok_or_else(|| internal(start))?;
        let punctuation = match byte {
            b'{' => Some(TokenKind::LBrace),
            b'}' => Some(TokenKind::RBrace),
            b'[' => Some(TokenKind::LBracket),
            b']' => Some(TokenKind::RBracket),
            b':' => Some(TokenKind::Colon),
            b',' => Some(TokenKind::Comma),
            _ => None,
        };
        if let Some(kind) = punctuation {
            return Ok(Some(Token {
                kind,
                span: Span32::new(start, 1),
                decoded: None,
            }));
        }
        let kind = match byte {
            b'"' => PendingKind::String,
            b'-' | b'0'..=b'9' => PendingKind::Number,
            b't' => PendingKind::Literal(TokenKind::True),
            b'f' => PendingKind::Literal(TokenKind::False),
            b'n' => PendingKind::Literal(TokenKind::Null),
            _ => {
                return Err(lexical(
                    ErrorCode::InvalidToken,
                    start,
                    "invalid token start",
                ))
            }
        };
        PendingToken {
            kind,
            start,
            scan_offset: start + u32::from(kind == PendingKind::String),
            first_escape: None,
            non_ascii: false,
            control: false,
            digits: 0,
            excess_digit: None,
        }
    };
    let end = offset(state.input.len())?;
    if carry.start != state.byte_offset
        || carry.scan_offset < carry.start
        || carry.scan_offset > end
        || (carry.kind == PendingKind::String && carry.scan_offset == carry.start)
    {
        return Err(internal(state.byte_offset));
    }
    while carry.scan_offset < end {
        let block = block_at(state, carry.scan_offset)?;
        let masks = block.masks;
        let remaining = masks.valid_mask & (u64::MAX << (carry.scan_offset - block.start));
        if carry.kind == PendingKind::String {
            // Delimiter quotes are outside the content mask. Escape parity,
            // including a backslash at lane 63, is carried by the classifier.
            let quotes = masks.delimiter_quotes() & remaining;
            let content_end = if quotes == 0 {
                block.end
            } else {
                block.start + quotes.trailing_zeros()
            };
            let content_bits = remaining & low_bits(content_end - block.start);
            let escapes = masks.backslash_mask & content_bits;
            if carry.first_escape.is_none() && escapes != 0 {
                carry.first_escape = Some(block.start + escapes.trailing_zeros());
            }
            carry.non_ascii |= masks.non_ascii_mask & content_bits != 0;
            carry.control |= masks.control_mask & content_bits != 0;
            if content_end - carry.start - 1 > state.limits.max_string_bytes {
                let excess = carry.start + 1 + state.limits.max_string_bytes;
                // Preserve lexical failures before the first excess raw byte.
                measure_string(
                    state,
                    carry.start,
                    excess + 1,
                    false,
                    carry
                        .first_escape
                        .is_some_and(|position| position <= excess),
                )?;
                return Err(internal(excess));
            }
            carry.scan_offset = content_end;
            if quotes != 0 {
                let reference =
                    if carry.first_escape.is_none() && !carry.non_ascii && !carry.control {
                        let len = content_end - carry.start - 1;
                        StrRef {
                            span: Span32::new(carry.start + 1, len),
                            source: SpanSource::RawPayload,
                            scalar_len: len,
                        }
                    } else {
                        decode_string(
                            state,
                            carry.start,
                            content_end,
                            true,
                            carry.first_escape.is_some(),
                        )?
                    };
                return Ok(Some(Token {
                    kind: TokenKind::String,
                    span: Span32::new(carry.start, content_end + 1 - carry.start),
                    decoded: Some(reference),
                }));
            }
        } else {
            // Include raw punctuation too: a malformed atom can contain a
            // quote, which must not hide its later delimiter from the lexer.
            let delimiters = (masks.whitespace_mask
                | masks.structural_mask
                | masks.colon_mask
                | masks.comma_mask
                | masks.lbrace_mask
                | masks.rbrace_mask
                | masks.lbracket_mask
                | masks.rbracket_mask)
                & remaining;
            let atom_end = if delimiters == 0 {
                block.end
            } else {
                block.start + delimiters.trailing_zeros()
            };
            if carry.kind == PendingKind::Number && carry.excess_digit.is_none() {
                let mut digits = masks.digit_mask & remaining & low_bits(atom_end - block.start);
                let count = digits.count_ones();
                let allowed =
                    u32::from(state.limits.max_number_digits).saturating_sub(carry.digits);
                if count > allowed {
                    // Only locate a particular digit when reporting a limit.
                    for _ in 0..allowed {
                        digits &= digits - 1;
                    }
                    carry.excess_digit = Some(block.start + digits.trailing_zeros());
                }
                carry.digits += count;
            }
            carry.scan_offset = atom_end;
            if delimiters != 0 {
                return complete_atom(state, carry, atom_end).map(Some);
            }
        }
    }
    state.pending = Some(carry);
    if !state.final_input {
        return Ok(None);
    }
    if carry.kind == PendingKind::String {
        // Validate even an unclosed string so a known bad escape, control byte,
        // surrogate, or UTF-8 sequence takes precedence over missing quotes.
        measure_string(state, carry.start, end, false, carry.first_escape.is_some())?;
        return Err(lexical(
            ErrorCode::UnclosedString,
            carry.start,
            "string has no closing quote",
        ));
    }
    complete_atom(state, carry, end).map(Some)
}

/// Low occupied lanes, avoiding a shift by the mask width.
fn low_bits(len: u32) -> u64 {
    if len >= 64 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    }
}

/// Classifies consecutive input slices directly and caches the current block
/// across events. Native selection uses the whole retained input length.
fn block_at(state: &mut SyntaxState, position: u32) -> Result<ClassifiedBlock> {
    if let Some(block) = state.block {
        if position >= block.start && position < block.end {
            return Ok(block);
        }
    }
    if position < state.scan_offset || position as usize >= state.input.len() {
        return Err(internal(position));
    }
    let dispatch = match state.dispatch {
        Some(dispatch) => dispatch,
        None => {
            let dispatch = jmax_dispatch::dispatch(
                state.input.len(),
                jmax_dispatch::DispatchPolicy::default(),
            )?;
            state.dispatch = Some(dispatch);
            dispatch
        }
    };
    loop {
        let block_start = state.scan_offset as usize;
        let block_end = block_start.saturating_add(64).min(state.input.len());
        if block_start >= block_end {
            return Err(internal(position));
        }
        let before = state.scan_carry;
        let masks =
            dispatch.classify(&state.input[block_start..block_end], &mut state.scan_carry)?;
        let block = ClassifiedBlock {
            start: block_start as u32,
            end: block_end as u32,
            before,
            masks,
        };
        state.scan_offset = block.end;
        state.block = Some(block);
        if position < block.end {
            return Ok(block);
        }
    }
}

fn complete_atom(state: &SyntaxState, carry: PendingToken, end: u32) -> Result<Token> {
    let bytes = state
        .input
        .get(carry.start as usize..end as usize)
        .ok_or_else(|| internal(carry.start))?;
    let at_eof = state.final_input && end as usize == state.input.len();
    let kind = match carry.kind {
        PendingKind::Number => {
            validate_number(state, carry, end, at_eof)?;
            TokenKind::Number
        }
        PendingKind::Literal(kind) => {
            let expected: &[u8] = match kind {
                TokenKind::True => b"true",
                TokenKind::False => b"false",
                TokenKind::Null => b"null",
                _ => return Err(internal(carry.start)),
            };
            for (index, &byte) in bytes.iter().enumerate() {
                if expected.get(index) != Some(&byte) {
                    return Err(lexical(
                        ErrorCode::InvalidToken,
                        carry.start + index as u32,
                        "invalid literal or literal boundary",
                    ));
                }
            }
            if bytes.len() != expected.len() {
                return Err(lexical(
                    if at_eof {
                        ErrorCode::UnexpectedEof
                    } else {
                        ErrorCode::InvalidToken
                    },
                    end,
                    "incomplete literal",
                ));
            }
            kind
        }
        PendingKind::String => return Err(internal(carry.start)),
    };
    Ok(Token {
        kind,
        span: Span32::new(carry.start, end - carry.start),
        decoded: None,
    })
}

fn validate_number(state: &SyntaxState, carry: PendingToken, end: u32, at_eof: bool) -> Result<()> {
    // Keep incremental token publication and error timing unchanged: the old
    // chunked lexer is bounded by its discovered atom, even at punctuation.
    validate_number_inner(
        &state.input[..end as usize],
        Span32::new(carry.start, end - carry.start),
        state.numeric,
        carry.excess_digit,
        at_eof,
    )
}

pub(crate) fn validate_number_slice(
    input: &[u8],
    span: Span32,
    numeric: Option<jmax_abi::number::NumericPolicy>,
    excess_digit: Option<u32>,
) -> Result<()> {
    let end = span.end().ok_or_else(|| internal(span.offset))?;
    validate_number_inner(
        input,
        span,
        numeric,
        excess_digit,
        end as usize == input.len(),
    )
}

fn validate_number_inner(
    input: &[u8],
    span: Span32,
    numeric: Option<jmax_abi::number::NumericPolicy>,
    excess_digit: Option<u32>,
    at_eof: bool,
) -> Result<()> {
    let end = span.end().ok_or_else(|| internal(span.offset))?;
    // Ordinary syntax accepts all grammatical numbers. Facade parsing supplies
    // a policy and applies it here, before publishing the numeric event.
    let policy = numeric.unwrap_or(jmax_number::NumericPolicy {
        allow_Quoted_int: false,
        reject_non_finite_float: false,
        reject_negative_zero: false,
        max_exponent_abs: i16::MAX,
    });
    // The original suffix lets the lexer reject illegal number boundaries.
    // A successful lexed span must equal the bitmask-discovered atom span.
    let result = jmax_number::lex::lex_number(input, span.offset, &policy);
    let error = match result {
        Ok(number) if number.span() == span => {
            if numeric.is_some() && number.kind() == jmax_abi::span::NumberLexKind::FloatDecimal {
                jmax_number::float::parse_f64(&number, &policy).err()
            } else {
                None
            }
        }
        Ok(_) => Some(internal(span.offset)),
        Err(error)
            if numeric.is_none()
                && error.code == ErrorCode::ConstraintViolation
                && error.source == ErrorSource::Lexer
                && error.detail == "explicit exponent exceeds the policy limit" =>
        {
            None
        }
        Err(mut error) => {
            if at_eof && error.code == ErrorCode::InvalidNumber && error.byte_offset == end {
                error.code = ErrorCode::UnexpectedEof;
            }
            Some(error)
        }
    };
    if let Some(position) = excess_digit {
        if error
            .as_ref()
            .is_none_or(|error| error.byte_offset > position)
        {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                position,
                "number digit limit exceeded",
            ));
        }
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// A bounded view excluding both quotes. Missing bytes distinguish truncated
/// escapes at EOF from malformed escapes stopped by an actual closing quote.
pub(crate) struct StringInput<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) closed: bool,
    pub(crate) raw_limit: u32,
}

impl StringInput<'_> {
    fn byte(&self, position: u32) -> Result<u8> {
        if position >= self.end {
            return Err(lexical(
                if self.closed {
                    ErrorCode::InvalidEscape
                } else {
                    ErrorCode::UnexpectedEof
                },
                self.end,
                "truncated string escape",
            ));
        }
        if position <= self.start {
            return Err(internal(position));
        }
        if position - self.start - 1 >= self.raw_limit {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                position,
                "raw string byte limit exceeded",
            ));
        }
        self.bytes
            .get(position as usize)
            .copied()
            .ok_or_else(|| internal(position))
    }

    fn hex_quad(&self, position: &mut u32) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.byte(*position)? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => {
                    return Err(lexical(
                        ErrorCode::InvalidEscape,
                        *position,
                        "invalid Unicode escape digit",
                    ))
                }
            };
            value = (value << 4) | u16::from(digit);
            *position += 1;
        }
        Ok(value)
    }

    pub(crate) fn unit(&self, position: &mut u32) -> Result<char> {
        let start = *position;
        let byte = self.byte(start)?;
        *position += 1;
        let value = match byte {
            0..=0x1f => {
                return Err(lexical(
                    ErrorCode::ControlCharInString,
                    start,
                    "unescaped control byte in string",
                ))
            }
            b'\\' => {
                let escape = self.byte(*position)?;
                *position += 1;
                match escape {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{0008}',
                    b'f' => '\u{000c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let high = self.hex_quad(position)?;
                        let scalar = if (0xd800..=0xdbff).contains(&high) {
                            if *position == self.end && self.closed {
                                return Err(lone_surrogate(start));
                            }
                            if self.byte(*position)? != b'\\' {
                                return Err(lone_surrogate(start));
                            }
                            *position += 1;
                            if self.byte(*position)? != b'u' {
                                return Err(lone_surrogate(start));
                            }
                            *position += 1;
                            let low = self.hex_quad(position)?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(lone_surrogate(start));
                            }
                            0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
                        } else if (0xdc00..=0xdfff).contains(&high) {
                            return Err(lone_surrogate(start));
                        } else {
                            u32::from(high)
                        };
                        core::char::from_u32(scalar).ok_or_else(|| lone_surrogate(start))?
                    }
                    _ => {
                        return Err(lexical(
                            ErrorCode::InvalidEscape,
                            *position - 1,
                            "invalid string escape",
                        ))
                    }
                }
            }
            0x20..=0x7f => char::from(byte),
            _ => {
                let (width, mut scalar, second_min, second_max) = match byte {
                    0xc2..=0xdf => (2, u32::from(byte & 0x1f), 0x80, 0xbf),
                    0xe0 => (3, u32::from(byte & 0x0f), 0xa0, 0xbf),
                    0xed => (3, u32::from(byte & 0x0f), 0x80, 0x9f),
                    0xe1..=0xec | 0xee..=0xef => (3, u32::from(byte & 0x0f), 0x80, 0xbf),
                    0xf0 => (4, u32::from(byte & 0x07), 0x90, 0xbf),
                    0xf4 => (4, u32::from(byte & 0x07), 0x80, 0x8f),
                    0xf1..=0xf3 => (4, u32::from(byte & 0x07), 0x80, 0xbf),
                    _ => return Err(invalid_utf8(start)),
                };
                for index in 1..width {
                    if *position == self.end {
                        return Err(invalid_utf8(start));
                    }
                    let continuation = self.byte(*position)?;
                    let (min, max) = if index == 1 {
                        (second_min, second_max)
                    } else {
                        (0x80, 0xbf)
                    };
                    if !(min..=max).contains(&continuation) {
                        return Err(invalid_utf8(*position));
                    }
                    scalar = (scalar << 6) | u32::from(continuation & 0x3f);
                    *position += 1;
                }
                core::char::from_u32(scalar).ok_or_else(|| invalid_utf8(start))?
            }
        };
        Ok(value)
    }
}

struct StringMeasure {
    bytes: u32,
    scalars: u32,
    escaped: bool,
}

fn measure_string(
    state: &SyntaxState,
    start: u32,
    end: u32,
    closed: bool,
    escaped: bool,
) -> Result<StringMeasure> {
    let content_start = start.checked_add(1).ok_or_else(|| internal(start))?;
    state
        .input
        .get(content_start as usize..end as usize)
        .ok_or_else(|| internal(start))?;
    let input = StringInput {
        bytes: &state.input,
        start,
        end,
        closed,
        raw_limit: state.limits.max_string_bytes,
    };
    let mut position = content_start;
    let mut bytes = 0u32;
    let mut scalars = 0u32;
    let arena_start = offset(state.decoded.len())?;
    while position < end {
        let unit_start = position;
        let scalar = input.unit(&mut position)?;
        let width = scalar.len_utf8() as u32;
        bytes = bytes.checked_add(width).ok_or_else(|| {
            syntax_error(
                ErrorCode::ConstraintViolation,
                unit_start,
                "decoded string byte limit exceeded",
            )
        })?;
        if bytes > state.limits.max_string_bytes {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                unit_start,
                "decoded string byte limit exceeded",
            ));
        }
        if escaped {
            // For an ordinary UTF-8 scalar, identify the exact raw byte which
            // would exceed the arena. Escapes map their output to the opening
            // backslash because their decoded bytes have no individual raw
            // byte positions.
            let remaining = state.limits.max_decoded_bytes.saturating_sub(arena_start);
            let before = bytes - width;
            let excess_position = if state.input.get(unit_start as usize) == Some(&b'\\') {
                unit_start
            } else {
                unit_start + remaining.saturating_sub(before).min(width - 1)
            };
            arena_growth(
                arena_start,
                bytes,
                state.limits.max_decoded_bytes,
                excess_position,
            )?;
        }
        // Each scalar consumes at least one raw byte in the bounded input.
        scalars += 1;
    }
    Ok(StringMeasure {
        bytes,
        scalars,
        escaped,
    })
}

fn decode_string(
    state: &mut SyntaxState,
    start: u32,
    end: u32,
    closed: bool,
    escaped: bool,
) -> Result<StrRef> {
    let measured = measure_string(state, start, end, closed, escaped)?;
    if !measured.escaped {
        return Ok(StrRef {
            span: Span32::new(start + 1, end - start - 1),
            source: SpanSource::RawPayload,
            scalar_len: measured.scalars,
        });
    }
    let arena_start = offset(state.decoded.len())?;
    arena_growth(
        arena_start,
        measured.bytes,
        state.limits.max_decoded_bytes,
        start,
    )?;
    reserve(&mut state.decoded, measured.bytes as usize, start)?;
    let input = StringInput {
        bytes: &state.input,
        start,
        end,
        closed,
        raw_limit: state.limits.max_string_bytes,
    };
    let mut position = start + 1;
    while position < end {
        let scalar = match input.unit(&mut position) {
            Ok(scalar) => scalar,
            Err(error) => {
                state.decoded.truncate(arena_start as usize);
                return Err(error);
            }
        };
        let mut encoded = [0u8; 4];
        state
            .decoded
            .extend_from_slice(scalar.encode_utf8(&mut encoded).as_bytes());
    }
    Ok(StrRef {
        span: Span32::new(arena_start, measured.bytes),
        source: SpanSource::DecodedArena,
        scalar_len: measured.scalars,
    })
}

fn arena_growth(current: u32, additional: u32, limit: u32, position: u32) -> Result<u32> {
    current
        .checked_add(additional)
        .filter(|&total| total <= limit)
        .ok_or_else(|| {
            syntax_error(
                ErrorCode::DecodedArenaOverflow,
                position,
                "decoded arena byte limit exceeded",
            )
        })
}

fn reserve(storage: &mut Vec<u8>, additional: usize, position: u32) -> Result<()> {
    storage.try_reserve(additional).map_err(|_| {
        syntax_error(
            ErrorCode::BudgetExhausted,
            position,
            "unable to reserve feed storage",
        )
    })
}

fn offset(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| internal(u32::MAX))
}

fn internal(position: u32) -> Error {
    syntax_error(
        ErrorCode::InternalError,
        position,
        "inconsistent feed bounds or carry",
    )
}

fn lexical(code: ErrorCode, position: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Lexer, position, detail)
}

fn lone_surrogate(position: u32) -> Error {
    lexical(
        ErrorCode::LoneSurrogate,
        position,
        "unpaired UTF-16 surrogate",
    )
}

fn invalid_utf8(position: u32) -> Error {
    Error::new(
        ErrorCode::InvalidUtf8,
        ErrorSource::Utf8,
        position,
        "invalid UTF-8 in string",
    )
}

/// Complete owners use the same fused lexical cursor as borrowed consumers.
/// Persist its classifier/carry cache across events without a self-reference.
fn next_complete(state: &mut SyntaxState) -> Result<ScanStep> {
    loop {
        let phase = frames::current_phase(state);
        let key = matches!(phase, FramePhase::ObjectStart | FramePhase::ObjectKeyOrEnd);
        let mut cursor = match state.complete_cache {
            Some(cache) => BlockCursor::from_cache(
                &state.input,
                state.limits,
                state.numeric,
                state.duplicate_owner,
                cache,
            ),
            None => BlockCursor::new(
                &state.input,
                state.limits,
                state.numeric,
                state.duplicate_owner,
            )?,
        };
        if cursor.skip_whitespace()?.is_none() {
            state.byte_offset = cursor.offset();
            state.complete_cache = Some(cursor.cache());
            if state.frames.is_empty() && state.root_phase == FramePhase::RootDone {
                state.finished = true;
                return Ok(ScanStep::Finished);
            }
            return Err(syntax_error(
                ErrorCode::UnexpectedEof,
                state.byte_offset,
                "incomplete JSON document",
            ));
        }
        if state.frames.is_empty() && state.root_phase == FramePhase::RootDone {
            return Err(syntax_error(
                ErrorCode::TrailingContent,
                cursor.offset(),
                "content follows the root value",
            ));
        }
        let checkpoint = state.decoded.len();
        let scanned = cursor.next_token(&mut state.decoded, key, false);
        let cache = cursor.cache();
        let position = cursor.offset();
        // End the immutable input borrow before mutating the grammar state.
        drop(cursor);
        let (token, hash) = match scanned {
            Ok(token) => token,
            Err(error) => {
                state.decoded.truncate(checkpoint);
                return Err(error);
            }
        };
        let transition = match frames::apply_transition(state, token, hash) {
            Ok(transition) => transition,
            Err(error) => {
                state.decoded.truncate(checkpoint);
                return Err(error);
            }
        };
        state.complete_cache = Some(cache);
        state.byte_offset = position;
        if let Some(event) = transition.event {
            return Ok(ScanStep::Emit(event));
        }
    }
}
