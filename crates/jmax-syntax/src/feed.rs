//! Retained-input incremental JSON event feed.
//!
//! Boundary discovery visits each incoming byte once. Complete strings are
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
                ErrorCode::InvalidSyntax, end, "input was already ended",
            ));
        }
        frames::check_payload_growth(state.input.len(), bytes.len(), state.limits)?;
        reserve(&mut state.input, bytes.len(), end)?;
        state.input.extend_from_slice(bytes);
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
    next_inner(state).map_err(|error| state.fail(error))
}

fn next_inner(state: &mut SyntaxState) -> Result<ScanStep> {
    loop {
        let end = offset(state.input.len())?;
        if state.byte_offset > end {
            return Err(internal(state.byte_offset));
        }
        if state.pending.is_none() {
            while state.input.get(state.byte_offset as usize)
                .is_some_and(|byte| JSON_WHITESPACE.contains(byte))
            {
                state.byte_offset += 1;
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
                    ErrorCode::UnexpectedEof, end, "incomplete JSON document",
                ));
            }
            // Trailing content is diagnosed before inspecting its lexical form.
            if state.frames.is_empty() && state.root_phase == FramePhase::RootDone {
                return Err(syntax_error(
                    ErrorCode::TrailingContent, state.byte_offset,
                    "content follows the root value",
                ));
            }
        }

        let arena_start = state.decoded.len();
        let token = match token(state)? {
            Some(token) => token,
            None => return Ok(ScanStep::NeedMore),
        };
        let result = machine::transition(frames::current_phase(state), token)
            .and_then(|transition| {
                frames::apply(state, token, transition)?;
                Ok(transition)
            });
        let transition = match result {
            Ok(transition) => transition,
            Err(error) => {
                // A rejected key/value must not leave unpublished arena bytes.
                state.decoded.truncate(arena_start);
                return Err(error);
            }
        };
        state.byte_offset = token.span.end().ok_or_else(|| internal(token.span.offset))?;
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
        let byte = *state.input.get(start as usize).ok_or_else(|| internal(start))?;
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
            return Ok(Some(Token { kind, span: Span32::new(start, 1), decoded: None }));
        }
        let kind = match byte {
            b'"' => PendingKind::String,
            b'-' | b'0'..=b'9' => PendingKind::Number,
            b't' => PendingKind::Literal(TokenKind::True),
            b'f' => PendingKind::Literal(TokenKind::False),
            b'n' => PendingKind::Literal(TokenKind::Null),
            _ => return Err(lexical(ErrorCode::InvalidToken, start, "invalid token start")),
        };
        PendingToken {
            kind, start,
            scan_offset: start + u32::from(kind == PendingKind::String),
            escaped: false,
        }
    };
    let end = offset(state.input.len())?;
    if carry.start != state.byte_offset || carry.scan_offset < carry.start
        || carry.scan_offset > end
        || (carry.kind == PendingKind::String && carry.scan_offset == carry.start)
    {
        return Err(internal(state.byte_offset));
    }
    while let Some(&byte) = state.input.get(carry.scan_offset as usize) {
        if carry.kind == PendingKind::String {
            if byte == b'"' && !carry.escaped {
                let reference = decode_string(state, carry.start, carry.scan_offset, true)?;
                return Ok(Some(Token {
                    kind: TokenKind::String,
                    span: Span32::new(carry.start, carry.scan_offset + 1 - carry.start),
                    decoded: Some(reference),
                }));
            }
            if carry.scan_offset - carry.start - 1 >= state.limits.max_string_bytes {
                // Validate the prefix once, preserving earlier lexical errors.
                // The walker necessarily encounters the first excess raw byte.
                measure_string(state, carry.start, carry.scan_offset + 1, false)?;
                return Err(internal(carry.scan_offset));
            }
            carry.escaped = byte == b'\\' && !carry.escaped;
        } else if boundary(byte) {
            return complete_atom(state, carry, carry.scan_offset).map(Some);
        }
        carry.scan_offset += 1;
    }
    state.pending = Some(carry);
    if !state.final_input {
        return Ok(None);
    }
    if carry.kind == PendingKind::String {
        // Validate even an unclosed string so a known bad escape, control byte,
        // surrogate, or UTF-8 sequence takes precedence over missing quotes.
        measure_string(state, carry.start, end, false)?;
        return Err(lexical(
            ErrorCode::UnclosedString, carry.start, "string has no closing quote",
        ));
    }
    complete_atom(state, carry, end).map(Some)
}

fn boundary(byte: u8) -> bool {
    JSON_WHITESPACE.contains(&byte) || matches!(byte, b'{' | b'}' | b'[' | b']' | b':' | b',')
}

fn complete_atom(state: &SyntaxState, carry: PendingToken, end: u32) -> Result<Token> {
    let bytes = state.input.get(carry.start as usize..end as usize)
        .ok_or_else(|| internal(carry.start))?;
    let at_eof = state.final_input && end as usize == state.input.len();
    let kind = match carry.kind {
        PendingKind::Number => {
            validate_number(bytes, carry.start, at_eof, state.limits.max_number_digits)?;
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
                        ErrorCode::InvalidToken, carry.start + index as u32,
                        "invalid literal or literal boundary",
                    ));
                }
            }
            if bytes.len() != expected.len() {
                return Err(lexical(
                    if at_eof { ErrorCode::UnexpectedEof } else { ErrorCode::InvalidToken },
                    end, "incomplete literal",
                ));
            }
            kind
        }
        PendingKind::String => return Err(internal(carry.start)),
    };
    Ok(Token { kind, span: Span32::new(carry.start, end - carry.start), decoded: None })
}

#[derive(Clone, Copy)]
enum NumberPhase { Start, Sign, Zero, Integer, Point, Fraction, Exponent, ExponentSign, ExponentDigits }

fn validate_number(bytes: &[u8], start: u32, at_eof: bool, digit_limit: u16) -> Result<()> {
    use NumberPhase::*;
    let mut phase = Start;
    let mut digits = 0u32;
    for (index, &byte) in bytes.iter().enumerate() {
        let position = start + index as u32;
        phase = match (phase, byte) {
            (Start, b'-') => Sign,
            (Start | Sign, b'0') => Zero,
            (Start | Sign, b'1'..=b'9') => Integer,
            (Integer, b'0'..=b'9') => Integer,
            (Zero | Integer, b'.') => Point,
            (Point | Fraction, b'0'..=b'9') => Fraction,
            (Zero | Integer | Fraction, b'e' | b'E') => Exponent,
            (Exponent, b'+' | b'-') => ExponentSign,
            (Exponent | ExponentSign | ExponentDigits, b'0'..=b'9') => ExponentDigits,
            _ => return Err(lexical(ErrorCode::InvalidNumber, position, "invalid JSON number")),
        };
        if byte.is_ascii_digit() {
            // The configured limit is u16, so this counter cannot overflow.
            digits += 1;
            if digits > u32::from(digit_limit) {
                return Err(syntax_error(
                    ErrorCode::ConstraintViolation, position, "number digit limit exceeded",
                ));
            }
        }
    }
    if !matches!(phase, Zero | Integer | Fraction | ExponentDigits) {
        return Err(lexical(
            if at_eof { ErrorCode::UnexpectedEof } else { ErrorCode::InvalidNumber },
            start + bytes.len() as u32, "incomplete JSON number",
        ));
    }
    Ok(())
}

/// A bounded view excluding both quotes. Missing bytes distinguish truncated
/// escapes at EOF from malformed escapes stopped by an actual closing quote.
struct StringInput<'a> {
    bytes: &'a [u8],
    start: u32,
    end: u32,
    closed: bool,
    raw_limit: u32,
}

impl StringInput<'_> {
    fn byte(&self, position: u32) -> Result<u8> {
        if position >= self.end {
            return Err(lexical(
                if self.closed { ErrorCode::InvalidEscape } else { ErrorCode::UnexpectedEof },
                self.end, "truncated string escape",
            ));
        }
        if position <= self.start {
            return Err(internal(position));
        }
        if position - self.start - 1 >= self.raw_limit {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation, position, "raw string byte limit exceeded",
            ));
        }
        self.bytes.get(position as usize).copied().ok_or_else(|| internal(position))
    }

    fn hex_quad(&self, position: &mut u32) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.byte(*position)? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(lexical(
                    ErrorCode::InvalidEscape, *position, "invalid Unicode escape digit",
                )),
            };
            value = (value << 4) | u16::from(digit);
            *position += 1;
        }
        Ok(value)
    }

    fn unit(&self, position: &mut u32) -> Result<char> {
        let start = *position;
        let byte = self.byte(start)?;
        *position += 1;
        let value = match byte {
            0..=0x1f => return Err(lexical(
                ErrorCode::ControlCharInString, start, "unescaped control byte in string",
            )),
            b'\\' => {
                let escape = self.byte(*position)?;
                *position += 1;
                match escape {
                    b'"' => '"', b'\\' => '\\', b'/' => '/',
                    b'b' => '\u{0008}', b'f' => '\u{000c}',
                    b'n' => '\n', b'r' => '\r', b't' => '\t',
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
                            0x10000 + ((u32::from(high) - 0xd800) << 10)
                                + (u32::from(low) - 0xdc00)
                        } else if (0xdc00..=0xdfff).contains(&high) {
                            return Err(lone_surrogate(start));
                        } else {
                            u32::from(high)
                        };
                        core::char::from_u32(scalar).ok_or_else(|| lone_surrogate(start))?
                    }
                    _ => return Err(lexical(
                        ErrorCode::InvalidEscape, *position - 1, "invalid string escape",
                    )),
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
                    let (min, max) = if index == 1 { (second_min, second_max) } else { (0x80, 0xbf) };
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

fn measure_string(state: &SyntaxState, start: u32, end: u32, closed: bool) -> Result<StringMeasure> {
    let content_start = start.checked_add(1).ok_or_else(|| internal(start))?;
    let content = state.input.get(content_start as usize..end as usize)
        .ok_or_else(|| internal(start))?;
    let escaped = content.contains(&b'\\');
    let input = StringInput {
        bytes: &state.input, start, end, closed, raw_limit: state.limits.max_string_bytes,
    };
    let mut position = content_start;
    let mut bytes = 0u32;
    let mut scalars = 0u32;
    let arena_start = offset(state.decoded.len())?;
    while position < end {
        let unit_start = position;
        let scalar = input.unit(&mut position)?;
        let width = scalar.len_utf8() as u32;
        bytes = bytes.checked_add(width).ok_or_else(|| syntax_error(
            ErrorCode::ConstraintViolation, unit_start, "decoded string byte limit exceeded",
        ))?;
        if bytes > state.limits.max_string_bytes {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation, unit_start, "decoded string byte limit exceeded",
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
            arena_growth(arena_start, bytes, state.limits.max_decoded_bytes, excess_position)?;
        }
        // Each scalar consumes at least one raw byte in the bounded input.
        scalars += 1;
    }
    Ok(StringMeasure { bytes, scalars, escaped })
}

fn decode_string(state: &mut SyntaxState, start: u32, end: u32, closed: bool) -> Result<StrRef> {
    let measured = measure_string(state, start, end, closed)?;
    if !measured.escaped {
        return Ok(StrRef {
            span: Span32::new(start + 1, end - start - 1),
            source: SpanSource::RawPayload, scalar_len: measured.scalars,
        });
    }
    let arena_start = offset(state.decoded.len())?;
    arena_growth(arena_start, measured.bytes, state.limits.max_decoded_bytes, start)?;
    reserve(&mut state.decoded, measured.bytes as usize, start)?;
    let input = StringInput {
        bytes: &state.input, start, end, closed, raw_limit: state.limits.max_string_bytes,
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
        state.decoded.extend_from_slice(scalar.encode_utf8(&mut encoded).as_bytes());
    }
    Ok(StrRef {
        span: Span32::new(arena_start, measured.bytes),
        source: SpanSource::DecodedArena, scalar_len: measured.scalars,
    })
}

fn arena_growth(current: u32, additional: u32, limit: u32, position: u32) -> Result<u32> {
    current.checked_add(additional).filter(|&total| total <= limit).ok_or_else(|| {
        syntax_error(ErrorCode::DecodedArenaOverflow, position, "decoded arena byte limit exceeded")
    })
}

fn reserve(storage: &mut Vec<u8>, additional: usize, position: u32) -> Result<()> {
    storage.try_reserve(additional).map_err(|_| syntax_error(
        ErrorCode::BudgetExhausted, position, "unable to reserve feed storage",
    ))
}

fn offset(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| internal(u32::MAX))
}

fn internal(position: u32) -> Error {
    syntax_error(ErrorCode::InternalError, position, "inconsistent feed bounds or carry")
}

fn lexical(code: ErrorCode, position: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Lexer, position, detail)
}

fn lone_surrogate(position: u32) -> Error {
    lexical(ErrorCode::LoneSurrogate, position, "unpaired UTF-16 surrogate")
}

fn invalid_utf8(position: u32) -> Error {
    Error::new(ErrorCode::InvalidUtf8, ErrorSource::Utf8, position, "invalid UTF-8 in string")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(state: &mut SyntaxState, events: &mut Vec<Event>) -> Result<ScanStep> {
        loop {
            match next(state)? {
                ScanStep::Emit(event) => events.push(event),
                pause => return Ok(pause),
            }
        }
    }

    fn parse(bytes: &[u8], limits: Limits) -> Result<(SyntaxState, Vec<Event>)> {
        let mut state = SyntaxState::new(limits);
        push(&mut state, bytes)?;
        // EOF ownership belongs to Wave 4. Set the frozen flag directly in
        // these module tests so they do not depend on its implementation.
        state.final_input = true;
        let mut events = Vec::new();
        assert_eq!(drain(&mut state, &mut events)?, ScanStep::Finished);
        Ok((state, events))
    }

    fn assert_error(error: &Error, code: ErrorCode, source: ErrorSource, position: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, position);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn rejected(bytes: &[u8], code: ErrorCode, source: ErrorSource, position: u32) {
        let error = parse(bytes, Limits::strict_default()).unwrap_err();
        assert_error(&error, code, source, position);
    }

    fn string_ref(event: Event) -> StrRef {
        match event {
            Event::Key { str_ref } => str_ref,
            Event::Scalar { kind: ScalarKind::String, decoded: Some(reference), .. } => reference,
            _ => panic!("expected a cooked string reference"),
        }
    }

    #[test]
    fn push_only_retains_bytes_and_empty_chunks_are_harmless() {
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, b"").unwrap();
        push(&mut state, b"not JSON").unwrap();
        push(&mut state, b"").unwrap();
        assert_eq!(state.raw_bytes(), b"not JSON");
        assert_eq!(state.byte_offset, 0);
        assert_eq!(state.root_phase, FramePhase::RootStart);
        assert!(state.frames.is_empty());
        assert!(state.decoded.is_empty());
        assert!(state.pending.is_none());
        assert!(state.failure().is_none());
    }

    #[test]
    fn payload_growth_is_inclusive_atomic_and_sticky() {
        let mut limits = Limits::strict_default();
        limits.max_payload_bytes = 3;
        let mut state = SyntaxState::new(limits);
        push(&mut state, b"[0]").unwrap();
        push(&mut state, b"").unwrap();
        let error = push(&mut state, b" ").unwrap_err();
        assert_error(&error, ErrorCode::PayloadTooLarge, ErrorSource::Syntax, 3);
        assert_eq!(state.input, b"[0]");
        assert_eq!(state.byte_offset, 0);
        assert_eq!(push(&mut state, b""), Err(error.clone()));
        assert_eq!(next(&mut state), Err(error));
        limits.max_payload_bytes = 0;
        let mut empty = SyntaxState::new(limits);
        push(&mut empty, b"").unwrap();
        assert_eq!(push(&mut empty, b"0").unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert!(empty.input.is_empty());
    }

    #[test]
    fn eof_rejects_even_empty_pushes_and_finished_next_is_idempotent() {
        for bytes in [&b""[..], &b"x"[..]] {
            let mut state = SyntaxState::new(Limits::strict_default());
            push(&mut state, b"null").unwrap();
            state.final_input = true;
            let error = push(&mut state, bytes).unwrap_err();
            assert_error(&error, ErrorCode::InvalidSyntax, ErrorSource::Syntax, 4);
            assert_eq!(state.input, b"null");
            assert_eq!(next(&mut state), Err(error));
        }
        let (mut state, _) = parse(b"[] \r\n", Limits::strict_default()).unwrap();
        for _ in 0..3 {
            assert_eq!(next(&mut state), Ok(ScanStep::Finished));
        }
        assert!(state.is_finished());
        assert_eq!(state.state().byte_offset, 5);
        let error = push(&mut state, b"").unwrap_err();
        assert_error(&error, ErrorCode::InvalidSyntax, ErrorSource::Syntax, 5);
        assert_eq!(next(&mut state), Err(error));
    }

    #[test]
    fn exact_events_offsets_and_silent_punctuation() {
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, b" {\"x\":[true,null]} \n").unwrap();
        let key = StrRef {
            span: Span32::new(3, 1), source: SpanSource::RawPayload, scalar_len: 1,
        };
        let expected = [
            (Event::ObjectStart { offset: 1 }, 2, 1),
            (Event::Key { str_ref: key }, 5, 1),
            (Event::ArrayStart { offset: 6 }, 7, 2),
            (Event::Scalar {
                kind: ScalarKind::True, span: Span32::new(7, 4), decoded: None,
            }, 11, 2),
            (Event::Scalar {
                kind: ScalarKind::Null, span: Span32::new(12, 4), decoded: None,
            }, 16, 2),
            (Event::ArrayEnd { offset: 16 }, 17, 1),
            (Event::ObjectEnd { offset: 17 }, 18, 0),
        ];
        for (event, position, depth) in expected {
            assert_eq!(next(&mut state), Ok(ScanStep::Emit(event)));
            assert_eq!(state.state().byte_offset, position);
            assert_eq!(state.state().depth, depth);
            assert!(state.pending.is_none());
        }
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        assert_eq!(state.byte_offset, 20);
        assert!(!state.finished);
        state.final_input = true;
        assert_eq!(next(&mut state), Ok(ScanStep::Finished));
    }

    #[test]
    fn every_two_chunk_split_matches_the_complete_document() {
        let documents: &[&[u8]] = &[
            b"0", b"-0.125E+100", b"true", b"false", b"null", b"\"\"",
            "\"aé中😀\"".as_bytes(),
            br#""\"\\\/\b\f\n\r\t\u0000\uD83d\uDe00""#,
            br#" {"a":[{},[],true,false,null,-12.50e-9,"a\"b"],"\u0062":"c"} "#,
        ];
        for &document in documents {
            let (whole, expected) = parse(document, Limits::strict_default()).unwrap();
            for split in 0..=document.len() {
                let mut state = SyntaxState::new(Limits::strict_default());
                let mut events = Vec::new();
                push(&mut state, &document[..split]).unwrap();
                assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
                let arena = state.decoded.clone();
                let pending = state.pending;
                assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
                assert_eq!(state.decoded, arena);
                assert_eq!(state.pending, pending);
                push(&mut state, &document[split..]).unwrap();
                assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
                state.final_input = true;
                assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::Finished));
                assert_eq!(events, expected, "split {split}");
                assert_eq!(state.raw_bytes(), whole.raw_bytes());
                assert_eq!(state.decoded_bytes(), whole.decoded_bytes());
                assert_eq!(state.byte_offset as usize, document.len());
            }
        }
    }

    #[test]
    fn one_byte_feeds_preserve_carry_parity_and_decode_once() {
        let document = br#""ab\\\"\uD83D\uDE00z""#;
        let (whole, expected) = parse(document, Limits::strict_default()).unwrap();
        let mut state = SyntaxState::new(Limits::strict_default());
        let mut events = Vec::new();
        let mut prior_scan = 0;
        for (index, byte) in document.iter().enumerate() {
            push(&mut state, core::slice::from_ref(byte)).unwrap();
            assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
            if index + 1 < document.len() {
                let carry = state.pending.unwrap();
                assert_eq!(carry.start, 0);
                assert_eq!(carry.scan_offset as usize, index + 1);
                assert!(carry.scan_offset >= prior_scan);
                prior_scan = carry.scan_offset;
                assert_eq!(state.byte_offset, 0);
                assert!(state.decoded.is_empty());
                assert!(events.is_empty());
            }
        }
        assert_eq!(events, expected);
        assert_eq!(state.decoded, whole.decoded);
        state.final_input = true;
        assert_eq!(next(&mut state), Ok(ScanStep::Finished));
    }

    #[test]
    fn growing_plain_string_keeps_monotonic_scan_position() {
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, b"\"").unwrap();
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        for length in 1..=1024 {
            push(&mut state, b"a").unwrap();
            assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
            assert_eq!(state.pending.unwrap().scan_offset, length + 1);
            assert_eq!(state.byte_offset, 0);
            assert!(state.decoded.is_empty());
        }
        push(&mut state, b"\"").unwrap();
        let reference = match next(&mut state).unwrap() {
            ScanStep::Emit(event) => string_ref(event),
            _ => panic!("complete string did not emit"),
        };
        assert_eq!(reference.scalar_len, 1024);
        assert_eq!(reference.source, SpanSource::RawPayload);
        assert_eq!(state.byte_offset, 1026);
        assert!(state.pending.is_none());
    }

    #[test]
    fn atoms_wait_for_boundaries_or_confirmed_eof() {
        for document in [&b"123"[..], &b"true"[..], &b"false"[..], &b"null"[..]] {
            let mut state = SyntaxState::new(Limits::strict_default());
            for byte in document {
                push(&mut state, core::slice::from_ref(byte)).unwrap();
                assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
                assert_eq!(state.byte_offset, 0);
                assert_eq!(state.pending.unwrap().scan_offset as usize, state.input.len());
            }
            state.final_input = true;
            assert!(matches!(next(&mut state), Ok(ScanStep::Emit(Event::Scalar { .. }))));
            assert_eq!(state.byte_offset as usize, document.len());
            assert_eq!(next(&mut state), Ok(ScanStep::Finished));
        }
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, b"true").unwrap();
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        push(&mut state, b"x ").unwrap();
        assert_error(&next(&mut state).unwrap_err(), ErrorCode::InvalidToken, ErrorSource::Lexer, 4);
        assert_eq!(state.root_phase, FramePhase::RootStart);
    }

    #[test]
    fn pending_cursor_stays_after_consumed_colon_and_whitespace() {
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, b"{\"a\": \t12").unwrap();
        assert!(matches!(next(&mut state), Ok(ScanStep::Emit(Event::ObjectStart { .. }))));
        assert!(matches!(next(&mut state), Ok(ScanStep::Emit(Event::Key { .. }))));
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        assert_eq!(state.byte_offset, 7);
        assert_eq!(state.pending.unwrap().scan_offset, 9);
        assert_eq!(state.frames[0].phase(), FramePhase::ObjectValue);
        push(&mut state, b".5e").unwrap();
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        assert_eq!(state.byte_offset, 7);
        assert_eq!(state.pending.unwrap().scan_offset, 12);
        push(&mut state, b"+2}").unwrap();
        assert_eq!(next(&mut state), Ok(ScanStep::Emit(Event::Scalar {
            kind: ScalarKind::Number, span: Span32::new(7, 7), decoded: None,
        })));
        assert_eq!(state.byte_offset, 14);
        assert_eq!(next(&mut state), Ok(ScanStep::Emit(Event::ObjectEnd { offset: 14 })));
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        state.final_input = true;
        assert_eq!(next(&mut state), Ok(ScanStep::Finished));
    }

    #[test]
    fn raw_and_escaped_strings_have_exact_owners_and_scalar_counts() {
        let vectors = [
            ("\"\"", "", SpanSource::RawPayload, 0),
            ("\"aé中😀\"", "aé中😀", SpanSource::RawPayload, 4),
            (r#""\"\\\/\b\f\n\r\t""#, "\"\\/\u{0008}\u{000c}\n\r\t", SpanSource::DecodedArena, 8),
            (r#""\u0000\u007f\u0080\u07ff\u0800\uffff""#, "\0\u{7f}\u{80}\u{7ff}\u{800}\u{ffff}", SpanSource::DecodedArena, 6),
            (r#""\uD800\uDC00\uDBFF\uDFFF""#, "\u{10000}\u{10ffff}", SpanSource::DecodedArena, 2),
            ("\"é\\n😀\"", "é\n😀", SpanSource::DecodedArena, 3),
        ];
        for (document, cooked, source, scalars) in vectors {
            let (state, events) = parse(document.as_bytes(), Limits::strict_default()).unwrap();
            assert_eq!(events.len(), 1);
            let reference = string_ref(events[0]);
            assert_eq!(reference.source, source);
            assert_eq!(reference.scalar_len, scalars);
            assert_eq!(reference.span.len as usize, cooked.len());
            assert_eq!(state.resolve_str(reference), Some(cooked));
            if source == SpanSource::RawPayload {
                assert!(state.decoded.is_empty());
            } else {
                assert_eq!(state.decoded, cooked.as_bytes());
            }
        }
    }

    #[test]
    fn references_survive_input_and_arena_growth() {
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, br#"["raw","\u0061""#).unwrap();
        let mut events = Vec::new();
        assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
        let raw = string_ref(events[1]);
        let cooked = string_ref(events[2]);
        for _ in 0..512 {
            push(&mut state, br#", "\u0062""#).unwrap();
            assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::NeedMore));
        }
        push(&mut state, b"]").unwrap();
        state.final_input = true;
        assert_eq!(drain(&mut state, &mut events), Ok(ScanStep::Finished));
        assert_eq!(state.resolve_str(raw), Some("raw"));
        assert_eq!(state.resolve_str(cooked), Some("a"));
        assert_eq!(state.decoded.len(), 513);
        assert_eq!(string_ref(events[514]).span.offset, 512);
    }

    #[test]
    fn strict_number_vectors_remain_raw_without_numeric_conversion() {
        let vectors: &[&[u8]] = &[
            b"0", b"-0", b"1", b"-123456789012345678901234567890",
            b"0.0", b"-0.0001", b"10e0", b"1E+999999", b"1e-999999", b"123.45E-67",
        ];
        for &document in vectors {
            let (state, events) = parse(document, Limits::strict_default()).unwrap();
            assert_eq!(events, [Event::Scalar {
                kind: ScalarKind::Number,
                span: Span32::new(0, document.len() as u32), decoded: None,
            }]);
            assert!(state.decoded.is_empty());
        }
        for (document, position) in [
            ("01", 1), ("-01", 2), ("--1", 1), ("1..0", 2),
            ("1.e2", 2), ("1e+-2", 3), ("1e2e3", 3), ("1+2", 1),
            ("0x1", 1), ("1true", 1), ("1\"x\"", 1), ("1_0", 1),
        ] {
            rejected(document.as_bytes(), ErrorCode::InvalidNumber, ErrorSource::Lexer, position);
        }
        for document in ["+1", ".1", "NaN", "Infinity"] {
            rejected(document.as_bytes(), ErrorCode::InvalidToken, ErrorSource::Lexer, 0);
        }
        let (_, events) = parse(b"\"123\"", Limits::strict_default()).unwrap();
        assert!(matches!(events[0], Event::Scalar { kind: ScalarKind::String, .. }));
    }

    #[test]
    fn literal_spelling_and_boundaries_are_exact() {
        for (document, position) in [
            ("True", 0), ("False", 0), ("NULL", 0), ("truX", 3),
            ("false0", 5), ("nullx", 4), ("truefalse", 4), ("true\"x\"", 4),
            ("nul ", 3), ("tru,", 3),
        ] {
            rejected(document.as_bytes(), ErrorCode::InvalidToken, ErrorSource::Lexer, position);
        }
        for separator in JSON_WHITESPACE {
            let document = [b't', b'r', b'u', b'e', separator];
            assert!(parse(&document, Limits::strict_default()).is_ok());
        }
        rejected(b"true\x0b", ErrorCode::InvalidToken, ErrorSource::Lexer, 4);
        rejected(b"\x0cnull", ErrorCode::InvalidToken, ErrorSource::Lexer, 0);
    }

    #[test]
    fn malformed_escapes_surrogates_and_controls_have_precise_offsets() {
        for (document, code, position) in [
            (r#""\q""#, ErrorCode::InvalidEscape, 2),
            (r#""\u12x4""#, ErrorCode::InvalidEscape, 5),
            (r#""\u12""#, ErrorCode::InvalidEscape, 5),
            (r#""\uDC00""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800x""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800\n""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800\u0041""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800\uDBFF""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800\uDC0x""#, ErrorCode::InvalidEscape, 12),
        ] {
            rejected(document.as_bytes(), code, ErrorSource::Lexer, position);
        }
        for control in 0u8..=31 {
            rejected(&[b'"', b'a', control, b'"'], ErrorCode::ControlCharInString, ErrorSource::Lexer, 2);
        }
        assert!(parse(b"\"\x7f\"", Limits::strict_default()).is_ok());
    }

    #[test]
    fn all_keys_and_nested_values_are_lexically_validated() {
        rejected(br#"{"\q":0}"#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 3);
        rejected(b"{\"a\n\":0}", ErrorCode::ControlCharInString, ErrorSource::Lexer, 3);
        rejected(b"{\"\xff\":0}", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 2);
        rejected(br#"{"\uDC00":0}"#, ErrorCode::LoneSurrogate, ErrorSource::Lexer, 2);
        rejected(br#"{"a":["\q"]}"#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 8);
    }

    #[test]
    fn utf8_rejects_overlong_surrogate_out_of_range_and_truncated_sequences() {
        let invalid: &[(&[u8], u32)] = &[
            (b"\"\x80\"", 1), (b"\"\xc0\x80\"", 1), (b"\"\xc1\xbf\"", 1),
            (b"\"\xe0\x9f\xbf\"", 2), (b"\"\xed\xa0\x80\"", 2),
            (b"\"\xf0\x8f\xbf\xbf\"", 2), (b"\"\xf4\x90\x80\x80\"", 2),
            (b"\"\xf5\x80\x80\x80\"", 1), (b"\"\xff\"", 1),
            (b"\"\xc2x\"", 2), (b"\"\xe2\x82x\"", 3),
            (b"\"\xf0\x90\x80x\"", 4), (b"\"\xc2\"", 1),
            (b"\"\xe2\x82", 1), (b"\"\xf0\x90\x80", 1),
        ];
        for &(document, position) in invalid {
            rejected(document, ErrorCode::InvalidUtf8, ErrorSource::Utf8, position);
        }
        for scalar in ['\u{80}', '\u{7ff}', '\u{800}', '\u{d7ff}', '\u{e000}', '\u{ffff}', '\u{10000}', '\u{10ffff}'] {
            let mut encoded = [0; 4];
            let mut document = Vec::new();
            document.push(b'"');
            document.extend_from_slice(scalar.encode_utf8(&mut encoded).as_bytes());
            document.push(b'"');
            let (_, events) = parse(&document, Limits::strict_default()).unwrap();
            assert_eq!(string_ref(events[0]).scalar_len, 1);
        }
    }

    #[test]
    fn eof_distinguishes_open_documents_strings_and_truncated_atoms() {
        for document in ["", " \r\n\t", "[", "{", "[0,", "{\"x\"", "{\"x\":", "{\"x\":0"] {
            rejected(document.as_bytes(), ErrorCode::UnexpectedEof, ErrorSource::Syntax, document.len() as u32);
        }
        for document in ["t", "tr", "tru", "fals", "nu", "-", "1.", "1e", "1e+"] {
            rejected(document.as_bytes(), ErrorCode::UnexpectedEof, ErrorSource::Lexer, document.len() as u32);
        }
        for document in ["\"", "\"plain", "\"é", r#""\u0061"#] {
            rejected(document.as_bytes(), ErrorCode::UnclosedString, ErrorSource::Lexer, 0);
        }
        for document in ["\"\\", r#""\u"#, r#""\u123"#, r#""\uD800"#, r#""\uD800\"#, r#""\uD800\uDC"#] {
            rejected(document.as_bytes(), ErrorCode::UnexpectedEof, ErrorSource::Lexer, document.len() as u32);
        }
        for document in ["- ", "1.,", "1e]", "1e+ "] {
            rejected(document.as_bytes(), ErrorCode::InvalidNumber, ErrorSource::Lexer, document.len() as u32 - 1);
        }
    }

    #[test]
    fn known_lexical_errors_win_over_missing_closing_quote_or_eof() {
        rejected(br#""\q"#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 2);
        rejected(br#""\u12x"#, ErrorCode::InvalidEscape, ErrorSource::Lexer, 5);
        rejected(br#""\uDC00"#, ErrorCode::LoneSurrogate, ErrorSource::Lexer, 1);
        rejected(b"\"bad\n", ErrorCode::ControlCharInString, ErrorSource::Lexer, 4);
        rejected(b"\"\xff", ErrorCode::InvalidUtf8, ErrorSource::Utf8, 1);
        rejected(b"01e", ErrorCode::InvalidNumber, ErrorSource::Lexer, 1);
        rejected(b"trux", ErrorCode::InvalidToken, ErrorSource::Lexer, 3);
    }

    #[test]
    fn trailing_content_wins_before_lexing_after_a_completed_root() {
        for document in ["[]x", "[]\"", "[]?", "[]0", "[]{}", "[]//comment"] {
            rejected(document.as_bytes(), ErrorCode::TrailingContent, ErrorSource::Syntax, 2);
        }
        rejected(b"0 \xff", ErrorCode::TrailingContent, ErrorSource::Syntax, 2);
        rejected(b"\"x\"true", ErrorCode::TrailingContent, ErrorSource::Syntax, 3);
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, b"[] \n").unwrap();
        assert_eq!(drain(&mut state, &mut Vec::new()), Ok(ScanStep::NeedMore));
        push(&mut state, b"?").unwrap();
        assert_error(&next(&mut state).unwrap_err(), ErrorCode::TrailingContent, ErrorSource::Syntax, 4);
    }

    #[test]
    fn structural_errors_and_missing_separators_are_not_accepted() {
        for (document, position) in [
            ("[1,]", 3), ("{\"a\":1,}", 7), ("[1 2]", 3),
            ("[true false]", 6), ("[\"a\"\"b\"]", 4), ("{\"a\" 0}", 5),
            ("{a:0}", 1), ("[}", 1), ("{]", 1), ("[,,]", 1),
            ("{\"a\":}", 5),
        ] {
            let code = if document == "{a:0}" { ErrorCode::InvalidToken } else { ErrorCode::InvalidSyntax };
            let source = if code == ErrorCode::InvalidToken { ErrorSource::Lexer } else { ErrorSource::Syntax };
            rejected(document.as_bytes(), code, source, position);
        }
    }

    #[test]
    fn string_limits_count_raw_bytes_including_escape_spelling() {
        let mut limits = Limits::strict_default();
        limits.max_string_bytes = 0;
        assert!(parse(b"\"\"", limits).is_ok());
        assert_error(&parse(b"\"a\"", limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        for (document, exact) in [("\"é\"", 2), (r#""\u0061""#, 6), (r#""\uD83D\uDE00""#, 12)] {
            limits.max_string_bytes = exact;
            assert!(parse(document.as_bytes(), limits).is_ok());
            limits.max_string_bytes = exact - 1;
            assert_error(&parse(document.as_bytes(), limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, exact);
        }
        limits.max_string_bytes = 2;
        let mut state = SyntaxState::new(limits);
        push(&mut state, b"\"ab").unwrap();
        assert_eq!(next(&mut state), Ok(ScanStep::NeedMore));
        push(&mut state, b"c").unwrap();
        assert_error(&next(&mut state).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 3);
        assert!(state.decoded.is_empty());
        limits.max_string_bytes = 1;
        assert_error(&parse(b"{\"ab\":0}", limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 3);
    }

    #[test]
    fn arena_limit_is_cumulative_inclusive_and_does_not_charge_raw_strings() {
        let mut limits = Limits::strict_default();
        limits.max_decoded_bytes = 0;
        assert!(parse("\"é😀\"".as_bytes(), limits).is_ok());
        assert_error(&parse(br#""\n""#, limits).unwrap_err(), ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 1);
        limits.max_decoded_bytes = 2;
        let document = br#"["\n","\t"]"#;
        let (state, _) = parse(document, limits).unwrap();
        assert_eq!(state.decoded, b"\n\t");
        limits.max_decoded_bytes = 1;
        let mut state = SyntaxState::new(limits);
        push(&mut state, document).unwrap();
        let error = drain(&mut state, &mut Vec::new()).unwrap_err();
        assert_error(&error, ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 7);
        assert_eq!(state.decoded, b"\n");
        assert_eq!(state.byte_offset, 6);
        limits.max_decoded_bytes = 4;
        assert!(parse(br#""\uD83D\uDE00""#, limits).is_ok());
        limits.max_decoded_bytes = 3;
        assert_error(&parse(br#""\uD83D\uDE00""#, limits).unwrap_err(), ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 1);
    }

    #[test]
    fn arena_overflow_identifies_the_exact_raw_utf8_byte() {
        let mut limits = Limits::strict_default();
        let document = "\"é\\n\"".as_bytes();
        limits.max_decoded_bytes = 1;
        assert_error(&parse(document, limits).unwrap_err(), ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 2);
        limits.max_decoded_bytes = 2;
        assert_error(&parse(document, limits).unwrap_err(), ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 3);
        limits.max_decoded_bytes = 3;
        assert_eq!(parse(document, limits).unwrap().0.decoded, "é\n".as_bytes());
        assert_error(&parse("\"😀\\n\"".as_bytes(), limits).unwrap_err(), ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 4);
    }

    #[test]
    fn number_digit_limits_include_exponents_and_exclude_signs_and_punctuation() {
        let mut limits = Limits::strict_default();
        limits.max_number_digits = 4;
        assert!(parse(b"-1.2e+34", limits).is_ok());
        limits.max_number_digits = 3;
        assert_error(&parse(b"-1.2e+34", limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 7);
        limits.max_number_digits = 0;
        assert_error(&parse(b"-0", limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        limits.max_number_digits = u16::MAX;
        let mut digits = alloc::vec![b'1'; usize::from(u16::MAX)];
        assert!(parse(&digits, limits).is_ok());
        digits.push(b'1');
        assert_error(&parse(&digits, limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, u32::from(u16::MAX));
    }

    #[test]
    fn frame_limits_and_decoded_duplicate_keys_are_enforced() {
        let mut limits = Limits::strict_default();
        limits.max_depth = 0;
        assert!(parse(b"0", limits).is_ok());
        assert_error(&parse(b"[]", limits).unwrap_err(), ErrorCode::DepthLimitExceeded, ErrorSource::Syntax, 0);
        limits.max_depth = 1;
        assert!(parse(b"[]", limits).is_ok());
        assert_error(&parse(b"[[]]", limits).unwrap_err(), ErrorCode::DepthLimitExceeded, ErrorSource::Syntax, 1);
        limits.max_array_items = 0;
        assert!(parse(b"[]", limits).is_ok());
        assert_error(&parse(b"[0]", limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        limits.max_fields_per_object = 0;
        assert!(parse(b"{}", limits).is_ok());
        assert_error(&parse(b"{\"x\":0}", limits).unwrap_err(), ErrorCode::ConstraintViolation, ErrorSource::Syntax, 1);
        rejected(br#"{"a":0,"\u0061":1}"#, ErrorCode::DuplicateObjectKey, ErrorSource::Syntax, 7);
        assert!(parse(br#"{"a":0,"\u0061":1}"#, Limits::permissive_default()).is_ok());
        assert!(parse(br#"[{"a":0},{"a":1}]"#, Limits::strict_default()).is_ok());
    }

    #[test]
    fn rejected_strings_never_leave_partial_or_unpublished_arena_bytes() {
        for document in [br#""\n\q""#.as_slice(), br#""\n\uD800""#.as_slice(), b"\"\\n\xff\""] {
            let mut state = SyntaxState::new(Limits::strict_default());
            push(&mut state, document).unwrap();
            assert!(next(&mut state).is_err());
            assert!(state.decoded.is_empty());
            assert_eq!(state.byte_offset, 0);
        }
        let mut state = SyntaxState::new(Limits::strict_default());
        push(&mut state, br#"{"a":0,"\u0061":1}"#).unwrap();
        let error = drain(&mut state, &mut Vec::new()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
        assert!(state.decoded.is_empty());
        assert_eq!(state.frames[0].count(), 1);
        assert_eq!(state.frames[0].phase(), FramePhase::ObjectKeyOrEnd);
        assert_eq!(state.byte_offset, 7);
    }

    #[test]
    fn malformed_inputs_have_identical_errors_at_every_chunk_split() {
        let documents: &[&[u8]] = &[
            br#""\q""#, br#""\uD800""#, b"\"\xe0\x80\x80\"",
            b"\"bad\n", b"01", b"1e+", b"truex", b"[0,]", b"{\"a\":",
            br#"{"a":0,"\u0061":1}"#, b"[] ?", b"[\"a\"\"b\"]",
        ];
        for &document in documents {
            let expected = parse(document, Limits::strict_default()).unwrap_err();
            for split in 0..=document.len() {
                let mut state = SyntaxState::new(Limits::strict_default());
                push(&mut state, &document[..split]).unwrap();
                let first = drain(&mut state, &mut Vec::new());
                let actual = match first {
                    Err(error) => error,
                    Ok(ScanStep::NeedMore) => {
                        push(&mut state, &document[split..]).unwrap();
                        state.final_input = true;
                        drain(&mut state, &mut Vec::new()).unwrap_err()
                    }
                    _ => panic!("stream completed without EOF"),
                };
                assert_eq!(actual, expected, "split {split}");
                assert_eq!(next(&mut state), Err(expected.clone()));
                assert_eq!(push(&mut state, b""), Err(expected.clone()));
            }
        }
    }

    #[test]
    fn first_failure_including_context_wins_over_flags_and_new_bytes() {
        let mut state = SyntaxState::new(Limits::strict_default());
        let error = Error::new(ErrorCode::InvalidUtf8, ErrorSource::Utf8, 9, "prior failure")
            .with_schema(21, 34);
        state.fail(error.clone());
        state.final_input = true;
        state.finished = true;
        assert_eq!(next(&mut state), Err(error.clone()));
        assert_eq!(push(&mut state, b"null"), Err(error.clone()));
        assert_eq!(push(&mut state, b""), Err(error.clone()));
        assert_eq!(state.failure(), Some(&error));
        assert!(state.input.is_empty());
        assert!(state.decoded.is_empty());
    }

    #[test]
    fn reservation_and_arena_arithmetic_boundaries_are_fallible() {
        let mut storage = alloc::vec![42u8];
        let capacity = storage.capacity();
        assert_error(&reserve(&mut storage, usize::MAX, 17).unwrap_err(), ErrorCode::BudgetExhausted, ErrorSource::Syntax, 17);
        assert_eq!(storage, [42]);
        assert_eq!(storage.capacity(), capacity);
        reserve(&mut storage, 0, 0).unwrap();
        assert_eq!(arena_growth(0, 0, 0, 0), Ok(0));
        assert_eq!(arena_growth(u32::MAX - 1, 1, u32::MAX, 9), Ok(u32::MAX));
        assert_eq!(arena_growth(u32::MAX, 0, u32::MAX, 9), Ok(u32::MAX));
        for (current, additional, limit) in [(u32::MAX, 1, u32::MAX), (2, 1, 2), (1, 0, 0)] {
            assert_error(&arena_growth(current, additional, limit, 9).unwrap_err(), ErrorCode::DecodedArenaOverflow, ErrorSource::Syntax, 9);
        }
    }

    #[test]
    fn inconsistent_internal_carry_and_cursor_fail_without_indexing() {
        let mut cursor = SyntaxState::new(Limits::strict_default());
        cursor.byte_offset = 1;
        assert_error(&next(&mut cursor).unwrap_err(), ErrorCode::InternalError, ErrorSource::Syntax, 1);
        for carry in [
            PendingToken { kind: PendingKind::Number, start: 1, scan_offset: 1, escaped: false },
            PendingToken { kind: PendingKind::String, start: 0, scan_offset: 0, escaped: false },
            PendingToken { kind: PendingKind::Number, start: 0, scan_offset: 9, escaped: false },
        ] {
            let mut state = SyntaxState::new(Limits::strict_default());
            push(&mut state, b"0").unwrap();
            state.pending = Some(carry);
            assert_error(&next(&mut state).unwrap_err(), ErrorCode::InternalError, ErrorSource::Syntax, 0);
        }
    }
}
