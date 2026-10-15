//! Bounded, owning decoders for the endpoint RPC envelope grammar.
//!
//! Opaque application values are parsed with an explicit container stack. They
//! receive the same syntax, Unicode, and duplicate-key checks as envelope data,
//! without acquiring a schema proof or an extra machine-number size limit.

#![forbid(unsafe_code)]

use super::*;

const KIND: usize = 0;
const SEQ: usize = 1;
const CODE: usize = 2;
const PARAMS: usize = 3;
const METHOD: usize = 4;
const RESULT: usize = 5;
const INDEX: usize = 6;
const ERROR: usize = 7;
const LEVEL: usize = 8;
const MESSAGE: usize = 9;
const TARGET: usize = 10;
const REASON: usize = 11;

const ENVELOPE_KEYS: [&[u8]; 12] = [
    b"kind", b"seq", b"code", b"params", b"method", b"result", b"index",
    b"error", b"level", b"message", b"target", b"reason",
];
const ERROR_KEYS: [&[u8]; 3] = [b"code", b"message", b"retryable"];

/// Decodes exactly one endpoint request, retaining its original payload.
pub fn decode_request(payload: Payload, limits: EnvelopeLimits) -> Result<OwnedEnvelope> {
    let envelope = decode_envelope(payload, limits)?;
    if envelope.view.kind != EnvelopeKind::EndpointRequest {
        return Err(wire_error(ErrorCode::TypeMismatch, "expected a request envelope"));
    }
    Ok(envelope)
}

/// Decodes exactly one endpoint response of any of the six response kinds.
pub fn decode_response(payload: Payload, limits: EnvelopeLimits) -> Result<OwnedEnvelope> {
    let envelope = decode_envelope(payload, limits)?;
    if envelope.view.kind == EnvelopeKind::EndpointRequest {
        return Err(wire_error(ErrorCode::TypeMismatch, "expected a response envelope"));
    }
    Ok(envelope)
}

/// Validates a complete endpoint envelope and owns every referenced byte.
pub fn decode_envelope(payload: Payload, limits: EnvelopeLimits) -> Result<OwnedEnvelope> {
    if limits.max_payload_bytes == 0 || limits.max_decoded_bytes == 0
        || limits.max_depth == 0 || limits.max_fields == 0
    {
        return Err(wire_error(ErrorCode::ConstraintViolation, "envelope limits must be nonzero"));
    }
    if u32::try_from(payload.len()).map_or(true, |len| len > limits.max_payload_bytes) {
        return Err(wire_error(ErrorCode::PayloadTooLarge, "envelope payload exceeds limit"));
    }
    if let Err(error) = core::str::from_utf8(payload.as_slice()) {
        // Input length was checked above, so this offset is representable.
        return Err(Error::new(ErrorCode::InvalidUtf8, ErrorSource::Utf8,
            error.valid_up_to() as u32, "payload is not UTF-8"));
    }

    let (view, decoded) = {
        let mut parser = Parser { bytes: payload.as_slice(), position: 0,
            decoded: Vec::new(), limits };
        let record = parser.record(0, &ENVELOPE_KEYS, Some(ERROR))?;
        parser.whitespace();
        if parser.position != parser.bytes.len() {
            return Err(parser.syntax(ErrorCode::TrailingContent, parser.position,
                "content follows the envelope"));
        }
        let view = parser.envelope_view(&record)?;
        (view, parser.decoded.into_boxed_slice())
    };
    Ok(OwnedEnvelope { payload, decoded, view })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueKind { Object, Array, String, Number, Bool, Null }

#[derive(Clone, Copy, Debug)]
struct Value {
    kind: ValueKind,
    span: Span32,
    string: Option<StrRef>,
}

#[derive(Clone, Copy, Debug)]
struct Member {
    key_offset: u32,
    value: Value,
}

struct Record<const N: usize> {
    members: [Option<Member>; N],
    span: Span32,
}

impl<const N: usize> Record<N> {
    fn required(&self, index: usize) -> Result<Value> {
        self.members.get(index).and_then(|member| *member).map(|member| member.value)
            .ok_or_else(|| local_error(ErrorCode::MissingRequiredField, self.span.offset,
                "required envelope field is absent"))
    }

    fn optional(&self, index: usize) -> Option<Value> {
        self.members.get(index).and_then(|member| *member).map(|member| member.value)
    }
}

#[derive(Clone, Copy)]
struct ObjectKey {
    reference: StrRef,
    offset: u32,
}

#[derive(Clone, Copy)]
enum Phase { ObjectFirst, ObjectKey, ObjectAfter, ArrayFirst, ArrayValue, ArrayAfter }

struct Container {
    phase: Phase,
    keys: Vec<ObjectKey>,
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
    decoded: Vec<u8>,
    limits: EnvelopeLimits,
}

fn local_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Wire, offset, detail)
}

impl Parser<'_> {
    // All positions are bounded by an input length checked to fit u32, and all
    // arena positions are checked against a u32 ceiling before appending.
    fn syntax(&self, code: ErrorCode, offset: usize, detail: &'static str) -> Error {
        Error::new(code, ErrorSource::Syntax, offset as u32, detail)
    }

    fn lexical(&self, code: ErrorCode, offset: usize, detail: &'static str) -> Error {
        Error::new(code, ErrorSource::Lexer, offset as u32, detail)
    }

    fn eof(&self) -> Error {
        self.syntax(ErrorCode::UnexpectedEof, self.bytes.len(), "incomplete JSON value")
    }

    fn whitespace(&mut self) {
        while matches!(self.bytes.get(self.position), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }

    fn span(&self, start: usize) -> Span32 {
        Span32::new(start as u32, (self.position - start) as u32)
    }

    fn string_bytes(&self, reference: StrRef) -> Option<&[u8]> {
        match reference.source {
            SpanSource::RawPayload => reference.span.slice(self.bytes),
            SpanSource::DecodedArena => reference.span.slice(&self.decoded),
        }
    }

    fn expect_string(&self, value: Value) -> Result<StrRef> {
        if value.kind != ValueKind::String {
            return Err(local_error(ErrorCode::TypeMismatch, value.span.offset,
                "expected a string field"));
        }
        value.string.ok_or_else(|| local_error(ErrorCode::InternalError,
            value.span.offset, "string reference is missing"))
    }

    fn text(&self, value: Value) -> Result<&[u8]> {
        let reference = self.expect_string(value)?;
        self.string_bytes(reference).ok_or_else(|| local_error(ErrorCode::InternalError,
            value.span.offset, "string reference is outside its owner"))
    }

    fn check_depth(&self, depth: usize, offset: usize) -> Result<()> {
        if depth > usize::from(self.limits.max_depth) {
            return Err(self.syntax(ErrorCode::DepthLimitExceeded, offset,
                "JSON container depth exceeds limit"));
        }
        Ok(())
    }

    fn punctuation(&mut self, expected: u8) -> Result<()> {
        self.whitespace();
        match self.bytes.get(self.position) {
            Some(byte) if *byte == expected => { self.position += 1; Ok(()) }
            None => Err(self.eof()),
            _ => Err(self.syntax(ErrorCode::InvalidSyntax, self.position,
                "unexpected JSON punctuation")),
        }
    }

    /// Reads a closed object. Only the outer envelope can invoke the nested
    /// error-object reader, so call depth here is bounded by two objects.
    fn record<const N: usize>(
        &mut self, enclosing_depth: usize, names: &[&[u8]; N], error_field: Option<usize>,
    ) -> Result<Record<N>> {
        self.whitespace();
        let start = self.position;
        match self.bytes.get(start) {
            Some(b'{') => {},
            None => return Err(self.eof()),
            _ => {
                // Validate a wrong-type value before reporting its type. This
                // keeps malformed JSON on the syntax/lexer error path.
                let value = self.value(enclosing_depth)?;
                return Err(local_error(ErrorCode::TypeMismatch, value.span.offset,
                    "expected an envelope object"));
            }
        }
        let depth = enclosing_depth.checked_add(1).ok_or_else(||
            self.syntax(ErrorCode::DepthLimitExceeded, start, "container depth overflow"))?;
        self.check_depth(depth, start)?;
        self.position += 1;
        let mut members = [None; N];
        let mut count = 0u32;
        let mut first = true;
        loop {
            self.whitespace();
            if self.bytes.get(self.position) == Some(&b'}') && first {
                self.position += 1;
                break;
            }
            if self.position == self.bytes.len() { return Err(self.eof()); }
            if self.bytes[self.position] != b'"' {
                return Err(self.syntax(ErrorCode::InvalidSyntax, self.position,
                    "expected an object key"));
            }
            if count == self.limits.max_fields {
                return Err(self.syntax(ErrorCode::ConstraintViolation, self.position,
                    "object field count exceeds limit"));
            }
            count += 1;
            let key = self.string()?;
            let key_text = self.text(key)?;
            let index = names.iter().position(|name| *name == key_text).ok_or_else(||
                local_error(ErrorCode::UnknownObjectKey, key.span.offset,
                    "unknown envelope object key"))?;
            if members[index].is_some() {
                return Err(local_error(ErrorCode::DuplicateObjectKey, key.span.offset,
                    "duplicate decoded object key"));
            }
            self.punctuation(b':')?;
            let value = if error_field == Some(index) {
                self.rpc_error(depth)?
            } else {
                self.value(depth)?
            };
            members[index] = Some(Member { key_offset: key.span.offset, value });
            self.whitespace();
            match self.bytes.get(self.position) {
                Some(b'}') => { self.position += 1; break; }
                Some(b',') => { self.position += 1; first = false; }
                None => return Err(self.eof()),
                _ => return Err(self.syntax(ErrorCode::InvalidSyntax, self.position,
                    "expected an object comma or closing brace")),
            }
        }
        Ok(Record { members, span: self.span(start) })
    }

    fn rpc_error(&mut self, enclosing_depth: usize) -> Result<Value> {
        let record = self.record(enclosing_depth, &ERROR_KEYS, None)?;
        self.signed_code(record.required(0)?)?;
        self.expect_string(record.required(1)?)?;
        let retryable = record.required(2)?;
        if retryable.kind != ValueKind::Bool {
            return Err(local_error(ErrorCode::TypeMismatch, retryable.span.offset,
                "error retryable must be a boolean"));
        }
        Ok(Value { kind: ValueKind::Object, span: record.span, string: None })
    }

    fn envelope_view(&self, record: &Record<12>) -> Result<WireEnvelopeView> {
        let kind_value = record.required(KIND)?;
        let (kind, required, allowed) = match self.text(kind_value)? {
            b"request" => (EnvelopeKind::EndpointRequest,
                bits(&[KIND, SEQ, CODE, PARAMS]), bits(&[KIND, SEQ, CODE, PARAMS, METHOD])),
            b"immediate" => (EnvelopeKind::EndpointImmediate,
                bits(&[KIND, SEQ, CODE, RESULT]), bits(&[KIND, SEQ, CODE, RESULT])),
            b"stream" => (EnvelopeKind::EndpointStream,
                bits(&[KIND, SEQ, CODE, INDEX, RESULT]), bits(&[KIND, SEQ, CODE, INDEX, RESULT])),
            b"error" => (EnvelopeKind::EndpointError,
                bits(&[KIND, ERROR]), bits(&[KIND, ERROR, SEQ, CODE])),
            b"log" => (EnvelopeKind::EndpointLog,
                bits(&[KIND, LEVEL, MESSAGE]), bits(&[KIND, LEVEL, MESSAGE, SEQ])),
            b"forwarded" => (EnvelopeKind::EndpointForwarded,
                bits(&[KIND, SEQ, CODE, TARGET]), bits(&[KIND, SEQ, CODE, TARGET])),
            b"close" => (EnvelopeKind::EndpointClose,
                bits(&[KIND, REASON]), bits(&[KIND, REASON, SEQ])),
            _ => return Err(local_error(ErrorCode::InvalidToken, kind_value.span.offset,
                "unknown endpoint envelope kind")),
        };
        for (index, member) in record.members.iter().enumerate() {
            if let Some(member) = member {
                if allowed & (1u16 << index) == 0 {
                    return Err(local_error(ErrorCode::UnknownObjectKey, member.key_offset,
                        "field is not allowed for this envelope kind"));
                }
            } else if required & (1u16 << index) != 0 {
                return Err(local_error(ErrorCode::MissingRequiredField, record.span.offset,
                    "required envelope field is absent"));
            }
        }
        let seq = record.optional(SEQ).map(|value| self.unsigned(value, u64::MAX)).transpose()?;
        let code = record.optional(CODE).map(|value|
            self.unsigned(value, u64::from(u32::MAX)).map(|number| number as u32)).transpose()?;
        let method = record.optional(METHOD).map(|value| self.expect_string(value)).transpose()?;
        if let Some(params) = record.optional(PARAMS) {
            if params.kind != ValueKind::Object {
                return Err(local_error(ErrorCode::TypeMismatch, params.span.offset,
                    "request params must be an object"));
            }
        }
        if let Some(index) = record.optional(INDEX) { self.unsigned(index, u64::MAX)?; }
        if let Some(level) = record.optional(LEVEL) {
            if !matches!(self.text(level)?, b"trace" | b"debug" | b"info" | b"warn" | b"error") {
                return Err(local_error(ErrorCode::ConstraintViolation, level.span.offset,
                    "unknown log level"));
            }
        }
        if let Some(message) = record.optional(MESSAGE) { self.expect_string(message)?; }
        if let Some(target) = record.optional(TARGET) {
            if self.text(target)?.is_empty() {
                return Err(local_error(ErrorCode::ConstraintViolation, target.span.offset,
                    "forward target must not be empty"));
            }
        }
        if let Some(reason) = record.optional(REASON) { self.expect_string(reason)?; }
        Ok(WireEnvelopeView { kind, method, seq, code,
            params_span: record.optional(PARAMS).map(|value| value.span),
            result_span: record.optional(RESULT).map(|value| value.span),
            error_span: record.optional(ERROR).map(|value| value.span) })
    }

    fn integer_magnitude(&self, value: Value, signed: bool, max: u64) -> Result<(bool, u64)> {
        if value.kind != ValueKind::Number {
            return Err(local_error(ErrorCode::TypeMismatch, value.span.offset,
                "expected an unquoted integer field"));
        }
        let raw = value.span.slice(self.bytes).ok_or_else(||
            local_error(ErrorCode::InternalError, value.span.offset, "number span is invalid"))?;
        let negative = raw.first() == Some(&b'-');
        let digits = if negative { &raw[1..] } else { raw };
        if (negative && !signed) || digits.is_empty()
            || digits.iter().any(|byte| !byte.is_ascii_digit())
            || (negative && digits == b"0")
        {
            return Err(local_error(ErrorCode::InvalidNumber, value.span.offset,
                "field requires a strict integer lexeme"));
        }
        let mut number = 0u64;
        for (index, byte) in digits.iter().enumerate() {
            number = number.checked_mul(10).and_then(|number|
                number.checked_add(u64::from(*byte - b'0'))).filter(|number| *number <= max)
                .ok_or_else(|| local_error(ErrorCode::NumberOverflow,
                    value.span.offset + u32::from(negative) + index as u32,
                    "integer field is outside its range"))?;
        }
        Ok((negative, number))
    }

    fn unsigned(&self, value: Value, max: u64) -> Result<u64> {
        self.integer_magnitude(value, false, max).map(|(_, number)| number)
    }

    fn signed_code(&self, value: Value) -> Result<i32> {
        let negative = value.span.slice(self.bytes).and_then(|raw| raw.first()) == Some(&b'-');
        let max = if negative { 2_147_483_648 } else { 2_147_483_647 };
        let (negative, number) = self.integer_magnitude(value, true, max)?;
        // The magnitude was bounded above before either signed conversion.
        Ok(if negative { (-(number as i64)) as i32 } else { number as i32 })
    }

    fn append_decoded(&mut self, bytes: &[u8], raw_offset: usize) -> Result<()> {
        let needed = self.decoded.len().checked_add(bytes.len()).ok_or_else(||
            self.syntax(ErrorCode::DecodedArenaOverflow, raw_offset, "decoded arena size overflow"))?;
        if u32::try_from(needed).map_or(true, |len| len > self.limits.max_decoded_bytes) {
            return Err(self.syntax(ErrorCode::DecodedArenaOverflow, raw_offset,
                "decoded arena exceeds limit"));
        }
        if needed > self.decoded.capacity() {
            let ceiling = usize::try_from(self.limits.max_decoded_bytes).map_err(|_|
                self.syntax(ErrorCode::DecodedArenaOverflow, raw_offset,
                    "decoded limit exceeds address space"))?;
            let target = self.decoded.capacity().saturating_mul(2).min(ceiling).max(needed);
            self.decoded.try_reserve_exact(target - self.decoded.len()).map_err(|_|
                local_error(ErrorCode::BudgetExhausted, raw_offset as u32,
                    "decoded arena allocation failed"))?;
        }
        self.decoded.extend_from_slice(bytes);
        Ok(())
    }

    fn append_segment(&mut self, start: usize, end: usize) -> Result<()> {
        let remaining = self.limits.max_decoded_bytes as usize - self.decoded.len();
        let offset = if end - start > remaining { start + remaining } else { start };
        let bytes = self.bytes;
        self.append_decoded(&bytes[start..end], offset)
    }

    fn string(&mut self) -> Result<Value> {
        let start = self.position;
        self.position += 1; // Caller has established the opening quote.
        let content_start = self.position;
        let arena_start = self.decoded.len();
        let mut segment = self.position;
        let mut escaped = false;
        let mut scalar_len = 0u32;
        while let Some(&byte) = self.bytes.get(self.position) {
            match byte {
                b'"' => {
                    let end = self.position;
                    if escaped {
                        self.append_segment(segment, end)?;
                    }
                    self.position += 1;
                    let (source, span) = if escaped {
                        (SpanSource::DecodedArena, Span32::new(arena_start as u32,
                            (self.decoded.len() - arena_start) as u32))
                    } else {
                        (SpanSource::RawPayload,
                            Span32::new(content_start as u32, (end - content_start) as u32))
                    };
                    return Ok(Value { kind: ValueKind::String, span: self.span(start),
                        string: Some(StrRef { span, source, scalar_len }) });
                }
                b'\\' => {
                    self.append_segment(segment, self.position)?;
                    escaped = true;
                    let escape_start = self.position;
                    self.position += 1;
                    let escape = *self.bytes.get(self.position).ok_or_else(|| self.eof())?;
                    self.position += 1;
                    let decoded = match escape {
                        b'"' => '"', b'\\' => '\\', b'/' => '/', b'b' => '\u{8}',
                        b'f' => '\u{c}', b'n' => '\n', b'r' => '\r', b't' => '\t',
                        b'u' => {
                            let high = self.hex_quad()?;
                            let scalar = if (0xd800..=0xdbff).contains(&high) {
                                if self.bytes.get(self.position) != Some(&b'\\')
                                    || self.bytes.get(self.position + 1) != Some(&b'u')
                                {
                                    return Err(self.lexical(ErrorCode::LoneSurrogate, escape_start,
                                        "high surrogate requires a low surrogate"));
                                }
                                self.position += 2;
                                let low = self.hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(self.lexical(ErrorCode::LoneSurrogate, escape_start,
                                        "invalid surrogate pair"));
                                }
                                0x10000 + ((u32::from(high) - 0xd800) << 10)
                                    + u32::from(low) - 0xdc00
                            } else if (0xdc00..=0xdfff).contains(&high) {
                                return Err(self.lexical(ErrorCode::LoneSurrogate, escape_start,
                                    "unpaired low surrogate"));
                            } else { u32::from(high) };
                            core::char::from_u32(scalar).ok_or_else(||
                                self.lexical(ErrorCode::LoneSurrogate, escape_start,
                                    "escape is not a Unicode scalar"))?
                        }
                        _ => return Err(self.lexical(ErrorCode::InvalidEscape, self.position - 1,
                            "unknown JSON string escape")),
                    };
                    let mut encoded = [0u8; 4];
                    self.append_decoded(decoded.encode_utf8(&mut encoded).as_bytes(), escape_start)?;
                    scalar_len += 1;
                    segment = self.position;
                }
                0..=0x1f => return Err(self.lexical(ErrorCode::ControlCharInString, self.position,
                    "unescaped control character in string")),
                _ => {
                    // UTF-8 was validated once before parsing. Only leading
                    // bytes count as scalars, including in multibyte segments.
                    if byte & 0xc0 != 0x80 { scalar_len += 1; }
                    self.position += 1;
                }
            }
        }
        Err(self.lexical(ErrorCode::UnclosedString, start, "string has no closing quote"))
    }

    fn hex_quad(&mut self) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = *self.bytes.get(self.position).ok_or_else(|| self.eof())?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.lexical(ErrorCode::InvalidEscape, self.position,
                    "Unicode escape contains a non-hex digit")),
            };
            value = (value << 4) | u16::from(digit);
            self.position += 1;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.position;
        if self.bytes.get(self.position) == Some(&b'-') { self.position += 1; }
        match self.bytes.get(self.position) {
            Some(b'0') => {
                self.position += 1;
                if self.bytes.get(self.position).is_some_and(u8::is_ascii_digit) {
                    return Err(self.lexical(ErrorCode::InvalidNumber, self.position,
                        "JSON number has a leading zero"));
                }
            }
            Some(b'1'..=b'9') => self.digits(),
            None => return Err(self.eof()),
            _ => return Err(self.lexical(ErrorCode::InvalidNumber, self.position,
                "expected an integer digit")),
        }
        if self.bytes.get(self.position) == Some(&b'.') {
            self.position += 1;
            self.required_digits()?;
        }
        if matches!(self.bytes.get(self.position), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.bytes.get(self.position), Some(b'+' | b'-')) { self.position += 1; }
            self.required_digits()?;
        }
        if !self.token_boundary() {
            return Err(self.lexical(ErrorCode::InvalidNumber, self.position,
                "invalid character after JSON number"));
        }
        Ok(Value { kind: ValueKind::Number, span: self.span(start), string: None })
    }

    fn digits(&mut self) {
        while self.bytes.get(self.position).is_some_and(u8::is_ascii_digit) {
            self.position += 1;
        }
    }

    fn required_digits(&mut self) -> Result<()> {
        match self.bytes.get(self.position) {
            Some(b'0'..=b'9') => { self.digits(); Ok(()) }
            None => Err(self.eof()),
            _ => Err(self.lexical(ErrorCode::InvalidNumber, self.position,
                "expected a number digit")),
        }
    }

    fn token_boundary(&self) -> bool {
        matches!(self.bytes.get(self.position), None
            | Some(b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'[' | b']' | b',' | b':'))
    }

    fn literal(&mut self, expected: &[u8], kind: ValueKind) -> Result<Value> {
        let start = self.position;
        for expected_byte in expected {
            match self.bytes.get(self.position) {
                Some(byte) if byte == expected_byte => self.position += 1,
                None => return Err(self.eof()),
                _ => return Err(self.lexical(ErrorCode::InvalidToken, self.position,
                    "invalid JSON literal")),
            }
        }
        if !self.token_boundary() {
            return Err(self.lexical(ErrorCode::InvalidToken, self.position,
                "invalid character after JSON literal"));
        }
        Ok(Value { kind, span: self.span(start), string: None })
    }

    fn atom(&mut self, depth: usize) -> Result<(Value, Option<Container>)> {
        self.whitespace();
        let start = self.position;
        let (kind, phase) = match self.bytes.get(start) {
            Some(b'{') => (ValueKind::Object, Phase::ObjectFirst),
            Some(b'[') => (ValueKind::Array, Phase::ArrayFirst),
            Some(b'"') => return self.string().map(|value| (value, None)),
            Some(b'-' | b'0'..=b'9') => return self.number().map(|value| (value, None)),
            Some(b't') => return self.literal(b"true", ValueKind::Bool).map(|value| (value, None)),
            Some(b'f') => return self.literal(b"false", ValueKind::Bool).map(|value| (value, None)),
            Some(b'n') => return self.literal(b"null", ValueKind::Null).map(|value| (value, None)),
            None => return Err(self.eof()),
            Some(b'}' | b']' | b',' | b':') => return Err(self.syntax(ErrorCode::InvalidSyntax,
                start, "expected a JSON value")),
            _ => return Err(self.lexical(ErrorCode::InvalidToken, start, "invalid JSON value token")),
        };
        self.check_depth(depth.checked_add(1).ok_or_else(||
            self.syntax(ErrorCode::DepthLimitExceeded, start, "container depth overflow"))?, start)?;
        self.position += 1;
        Ok((Value { kind, span: self.span(start), string: None },
            Some(Container { phase, keys: Vec::new() })))
    }

    fn push_container(&self, stack: &mut Vec<Container>, container: Container) -> Result<()> {
        reserve_entry(stack, usize::from(self.limits.max_depth), self.position as u32,
            "JSON container stack allocation failed")?;
        stack.push(container);
        Ok(())
    }

    /// Fully validates one application value without recursive descent.
    fn value(&mut self, enclosing_depth: usize) -> Result<Value> {
        let (mut root, container) = self.atom(enclosing_depth)?;
        let Some(container) = container else { return Ok(root); };
        let mut stack = Vec::new();
        self.push_container(&mut stack, container)?;
        while let Some(frame) = stack.last_mut() {
            self.whitespace();
            let byte = *self.bytes.get(self.position).ok_or_else(|| self.eof())?;
            let mut close = false;
            let mut child = false;
            match frame.phase {
                Phase::ObjectFirst if byte == b'}' => close = true,
                Phase::ObjectFirst | Phase::ObjectKey => {
                    if byte != b'"' {
                        return Err(self.syntax(ErrorCode::InvalidSyntax, self.position,
                            "expected an object key"));
                    }
                    if u32::try_from(frame.keys.len()).map_or(true, |count|
                        count >= self.limits.max_fields)
                    {
                        return Err(self.syntax(ErrorCode::ConstraintViolation, self.position,
                            "object field count exceeds limit"));
                    }
                    let key = self.string()?;
                    let reference = self.expect_string(key)?;
                    let ceiling = usize::try_from(self.limits.max_fields).map_err(|_|
                        local_error(ErrorCode::BudgetExhausted, key.span.offset,
                            "object field limit exceeds address space"))?;
                    reserve_entry(&mut frame.keys, ceiling, key.span.offset,
                        "object key allocation failed")?;
                    frame.keys.push(ObjectKey { reference, offset: key.span.offset });
                    self.punctuation(b':')?;
                    frame.phase = Phase::ObjectAfter;
                    child = true;
                }
                Phase::ObjectAfter => match byte {
                    b'}' => close = true,
                    b',' => { self.position += 1; frame.phase = Phase::ObjectKey; }
                    _ => return Err(self.syntax(ErrorCode::InvalidSyntax, self.position,
                        "expected an object comma or closing brace")),
                },
                Phase::ArrayFirst if byte == b']' => close = true,
                Phase::ArrayFirst | Phase::ArrayValue => {
                    frame.phase = Phase::ArrayAfter;
                    child = true;
                }
                Phase::ArrayAfter => match byte {
                    b']' => close = true,
                    b',' => { self.position += 1; frame.phase = Phase::ArrayValue; }
                    _ => return Err(self.syntax(ErrorCode::InvalidSyntax, self.position,
                        "expected an array comma or closing bracket")),
                },
            }
            if close {
                self.unique_keys(&mut frame.keys)?;
                self.position += 1;
                stack.pop();
            } else if child {
                let depth = enclosing_depth.checked_add(stack.len()).ok_or_else(||
                    self.syntax(ErrorCode::DepthLimitExceeded, self.position,
                        "container depth overflow"))?;
                let (_, container) = self.atom(depth)?;
                if let Some(container) = container { self.push_container(&mut stack, container)?; }
            }
        }
        root.span = Span32::new(root.span.offset, self.position as u32 - root.span.offset);
        Ok(root)
    }

    fn unique_keys(&self, keys: &mut [ObjectKey]) -> Result<()> {
        // In-place sorting bounds duplicate detection by O(n log n), including
        // adversarial objects. String references remain valid as the arena grows.
        for key in keys.iter() {
            if self.string_bytes(key.reference).is_none() {
                return Err(local_error(ErrorCode::InternalError, key.offset,
                    "object key reference is invalid"));
            }
        }
        keys.sort_unstable_by(|left, right| self.string_bytes(left.reference)
            .cmp(&self.string_bytes(right.reference)).then(left.offset.cmp(&right.offset)));
        let duplicate = keys.windows(2).filter(|pair|
            self.string_bytes(pair[0].reference) == self.string_bytes(pair[1].reference))
            .map(|pair| pair[1].offset).min();
        if let Some(offset) = duplicate {
            return Err(local_error(ErrorCode::DuplicateObjectKey, offset,
                "duplicate decoded object key"));
        }
        Ok(())
    }
}

fn bits(fields: &[usize]) -> u16 {
    fields.iter().fold(0, |mask, index| mask | (1u16 << *index))
}

fn reserve_entry<T>(entries: &mut Vec<T>, ceiling: usize, offset: u32,
    detail: &'static str) -> Result<()>
{
    let needed = entries.len().checked_add(1).filter(|needed| *needed <= ceiling)
        .ok_or_else(|| local_error(ErrorCode::BudgetExhausted, offset, detail))?;
    if needed > entries.capacity() {
        let target = entries.capacity().saturating_mul(2).min(ceiling).max(needed);
        entries.try_reserve_exact(target - entries.len())
            .map_err(|_| local_error(ErrorCode::BudgetExhausted, offset, detail))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    fn decode(json: &str) -> Result<OwnedEnvelope> {
        decode_envelope(Payload::from_slice(json.as_bytes()), EnvelopeLimits::default())
    }

    fn reject(json: &str, code: ErrorCode) -> Error {
        let error = decode(json).unwrap_err();
        assert_eq!(error.code, code, "input: {json}; error: {error}");
        assert!(error.byte_offset as usize <= json.len());
        error
    }

    #[test]
    fn all_seven_kinds_decode_with_their_exact_metadata() {
        let vectors = [
            (r#"{"kind":"request","seq":0,"code":0,"params":{}}"#,
                EnvelopeKind::EndpointRequest, Some(0), Some(0)),
            (r#"{"kind":"immediate","seq":1,"code":2,"result":null}"#,
                EnvelopeKind::EndpointImmediate, Some(1), Some(2)),
            (r#"{"kind":"stream","seq":3,"code":4,"index":5,"result":[true,false]}"#,
                EnvelopeKind::EndpointStream, Some(3), Some(4)),
            (r#"{"kind":"error","error":{"code":-1,"message":"bad","retryable":false}}"#,
                EnvelopeKind::EndpointError, None, None),
            (r#"{"kind":"log","level":"info","message":"hello"}"#,
                EnvelopeKind::EndpointLog, None, None),
            (r#"{"kind":"forwarded","seq":6,"code":7,"target":"worker"}"#,
                EnvelopeKind::EndpointForwarded, Some(6), Some(7)),
            (r#"{"kind":"close","seq":8,"reason":"done"}"#,
                EnvelopeKind::EndpointClose, Some(8), None),
        ];
        for (json, kind, seq, code) in vectors {
            let envelope = decode(json).unwrap();
            assert_eq!(envelope.payload().as_slice(), json.as_bytes());
            assert_eq!(envelope.view().kind, kind);
            assert_eq!(envelope.view().seq, seq);
            assert_eq!(envelope.view().code, code);
            assert_eq!(envelope.method(), None);
            assert_eq!(envelope.view().params_span.is_some(), kind == EnvelopeKind::EndpointRequest);
            assert_eq!(envelope.view().result_span.is_some(),
                matches!(kind, EnvelopeKind::EndpointImmediate | EnvelopeKind::EndpointStream));
            assert_eq!(envelope.view().error_span.is_some(), kind == EnvelopeKind::EndpointError);
            assert!(envelope.decoded_bytes().is_empty());
        }
    }

    #[test]
    fn entry_points_enforce_request_response_direction() {
        let request = Payload::from_slice(br#"{"kind":"request","seq":1,"code":2,"params":{}}"#);
        assert!(decode_request(request.clone(), EnvelopeLimits::default()).is_ok());
        assert_eq!(decode_response(request, EnvelopeLimits::default()).unwrap_err().code,
            ErrorCode::TypeMismatch);
        let responses = [
            r#"{"kind":"immediate","seq":1,"code":2,"result":0}"#,
            r#"{"kind":"stream","seq":1,"code":2,"index":0,"result":0}"#,
            r#"{"kind":"error","error":{"code":0,"message":"","retryable":true}}"#,
            r#"{"kind":"log","level":"warn","message":""}"#,
            r#"{"kind":"forwarded","seq":1,"code":2,"target":"a"}"#,
            r#"{"kind":"close","reason":""}"#,
        ];
        for json in responses {
            let payload = Payload::from_slice(json.as_bytes());
            assert!(decode_response(payload.clone(), EnvelopeLimits::default()).is_ok());
            assert_eq!(decode_request(payload, EnvelopeLimits::default()).unwrap_err().code,
                ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn raw_spans_include_complete_values_and_exclude_surrounding_space() {
        let request = decode(" \n{\"params\" : { \"app\": [1,{\"x\":\"} ]\"}] } ,\"code\":2,\"seq\":1,\"kind\":\"request\"}\t ").unwrap();
        assert_eq!(request.resolve(request.view().params_span.unwrap()),
            Some(&br#"{ "app": [1,{"x":"} ]"}] }"#[..]));
        for raw in ["null", "true", "false", "-0", "1.25e-9", "\"a\\n\"", "[]", "{}",
            "[ 1, {\"a\": 2} ]"]
        {
            let json = format!("{{\"kind\":\"immediate\",\"seq\":1,\"code\":2,\"result\": \n{raw} \t}}");
            let envelope = decode(&json).unwrap();
            assert_eq!(envelope.resolve(envelope.view().result_span.unwrap()), Some(raw.as_bytes()));
        }
        let json = r#"{"error": { "retryable":true, "message":"x", "code":-2 },"kind":"error"}"#;
        let envelope = decode(json).unwrap();
        assert_eq!(envelope.resolve(envelope.view().error_span.unwrap()),
            Some(&br#"{ "retryable":true, "message":"x", "code":-2 }"#[..]));
    }

    #[test]
    fn method_references_preserve_unicode_and_both_address_spaces() {
        let raw = decode(r#"{"kind":"request","seq":1,"code":2,"params":{},"method":"svc.é😀"}"#).unwrap();
        assert_eq!(raw.method(), Some("svc.é😀"));
        let reference = raw.view().method.unwrap();
        assert_eq!(reference.source, SpanSource::RawPayload);
        assert_eq!(reference.scalar_len, 6);
        assert_eq!(reference.span.len, 10);
        let escaped = decode(r#"{"k\u0069nd":"requ\u0065st","seq":1,"code":2,"params":{"a":"\n"},"m\u0065thod":"svc.\u00e9\uD83D\uDE00"}"#).unwrap();
        assert_eq!(escaped.method(), Some("svc.é😀"));
        let reference = escaped.view().method.unwrap();
        assert_eq!(reference.source, SpanSource::DecodedArena);
        assert_eq!(reference.scalar_len, 6);
        assert_eq!(reference.span.len, 10);
        assert_eq!(reference.span.slice(escaped.decoded_bytes()), Some("svc.é😀".as_bytes()));
        let clone = escaped.clone();
        drop(escaped);
        assert_eq!(clone.method(), Some("svc.é😀"));
        let empty = decode(r#"{"kind":"request","seq":0,"code":0,"params":{},"method":""}"#).unwrap();
        assert_eq!(empty.method(), Some(""));
        assert_eq!(empty.view().method.unwrap().scalar_len, 0);
    }

    #[test]
    fn all_string_escapes_and_unicode_scalars_are_decoded() {
        let envelope = decode(r#"{"kind":"request","seq":0,"code":0,"params":{},"method":"a\"\\\/\b\f\n\r\t\u0000é\uD834\uDD1E"}"#).unwrap();
        let expected = "a\"\\/\u{8}\u{c}\n\r\t\0é𝄞";
        assert_eq!(envelope.method(), Some(expected));
        assert_eq!(envelope.view().method.unwrap().scalar_len, expected.chars().count() as u32);
        assert_eq!(envelope.view().method.unwrap().span.len, expected.len() as u32);
        let mixed = decode(r#"{"kind":"request","seq":0,"code":0,"params":{},"method":"é\u0061中\n😀"}"#).unwrap();
        assert_eq!(mixed.method(), Some("éa中\n😀"));
        assert_eq!(mixed.view().method.unwrap().scalar_len, 5);
    }

    #[test]
    fn unicode_escape_boundaries_preserve_scalar_counts() {
        let envelope = decode(r#"{"kind":"request","seq":0,"code":0,"params":{},"method":"\u0000\u007f\u0080\u07ff\u0800\ud7ff\ue000\uffff\ud800\udc00\udbff\udfff"}"#).unwrap();
        let expected = "\0\u{7f}\u{80}\u{7ff}\u{800}\u{d7ff}\u{e000}\u{ffff}\u{10000}\u{10ffff}";
        assert_eq!(envelope.method(), Some(expected));
        assert_eq!(envelope.view().method.unwrap().scalar_len, 10);
        assert_eq!(envelope.view().method.unwrap().span.len as usize, expected.len());
    }

    #[test]
    fn field_order_is_unrestricted_including_nested_errors() {
        let envelope = decode(r#"{"method":"a.b","params":{"kind":"application"},"code":2,"seq":1,"kind":"request"}"#).unwrap();
        assert_eq!(envelope.method(), Some("a.b"));
        let error = decode(r#"{"code":4294967295,"error":{"retryable":true,"message":"","code":-2147483648},"seq":18446744073709551615,"kind":"error"}"#).unwrap();
        assert_eq!(error.view().seq, Some(u64::MAX));
        assert_eq!(error.view().code, Some(u32::MAX));
    }

    #[test]
    fn exact_integer_boundaries_are_accepted() {
        let envelope = decode(r#"{"kind":"stream","seq":18446744073709551615,"code":4294967295,"index":18446744073709551615,"result":0}"#).unwrap();
        assert_eq!(envelope.view().seq, Some(u64::MAX));
        assert_eq!(envelope.view().code, Some(u32::MAX));
        for code in ["-2147483648", "2147483647", "0", "-1"] {
            assert!(decode(&format!("{{\"kind\":\"error\",\"error\":{{\"code\":{code},\"message\":\"\",\"retryable\":false}}}}")).is_ok());
        }
    }

    #[test]
    fn integer_overflow_and_non_integer_lexemes_are_rejected() {
        for (field, number) in [("seq", "18446744073709551616"), ("code", "4294967296"),
            ("index", "18446744073709551616")]
        {
            let (seq, code, index) = match field {
                "seq" => (number, "0", "0"), "code" => ("0", number, "0"),
                _ => ("0", "0", number),
            };
            let json = format!("{{\"kind\":\"stream\",\"seq\":{seq},\"code\":{code},\"index\":{index},\"result\":null}}");
            let error = reject(&json, ErrorCode::NumberOverflow);
            assert_eq!(error.source, ErrorSource::Wire);
        }
        for number in ["-2147483649", "2147483648"] {
            reject(&format!("{{\"kind\":\"error\",\"error\":{{\"code\":{number},\"message\":\"\",\"retryable\":true}}}}"),
                ErrorCode::NumberOverflow);
        }
        for number in ["-0", "-1", "0.0", "1e0", "1E+0"] {
            for field in ["seq", "code", "index"] {
                let mut values = ["0", "0", "0"];
                values[match field { "seq" => 0, "code" => 1, _ => 2 }] = number;
                reject(&format!("{{\"kind\":\"stream\",\"seq\":{},\"code\":{},\"index\":{},\"result\":null}}",
                    values[0], values[1], values[2]), ErrorCode::InvalidNumber);
            }
        }
        for number in ["-0", "1.0", "1e0"] {
            reject(&format!("{{\"kind\":\"error\",\"error\":{{\"code\":{number},\"message\":\"\",\"retryable\":true}}}}"),
                ErrorCode::InvalidNumber);
        }
        for value in ["\"1\"", "null", "true", "[]", "{}"] {
            reject(&format!("{{\"kind\":\"request\",\"seq\":{value},\"code\":0,\"params\":{{}}}}"),
                ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn opaque_numbers_have_json_grammar_without_machine_range_limits() {
        let integer = "9".repeat(65_536);
        let json = format!("{{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":{integer}}}");
        let envelope = decode(&json).unwrap();
        assert_eq!(envelope.view().result_span.unwrap().len, 65_536);
        for number in ["-0", "123456789012345678901234567890", "1e999999", "-2.5E-123456"] {
            assert!(decode(&format!("{{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":{number}}}")).is_ok());
        }
        for number in ["01", "-01", "1.", "1e", "1e+", "--1", "1x"] {
            reject(&format!("{{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":{number}}}"),
                ErrorCode::InvalidNumber);
        }
    }

    #[test]
    fn duplicate_decoded_keys_are_rejected_at_every_object_level() {
        for json in [
            r#"{"kind":"close","kind":"close","reason":""}"#,
            r#"{"kind":"close","k\u0069nd":"close","reason":""}"#,
            r#"{"kind":"request","seq":1,"code":2,"params":{"a":1,"\u0061":2}}"#,
            r#"{"kind":"immediate","seq":1,"code":2,"result":[{"é":1,"\u00e9":2}]}"#,
            r#"{"kind":"immediate","seq":1,"code":2,"result":{"😀":1,"\uD83D\uDE00":2}}"#,
            r#"{"kind":"immediate","seq":1,"code":2,"result":{"":1,"":2}}"#,
            r#"{"kind":"error","error":{"code":1,"message":"a","retryable":true,"co\u0064e":2}}"#,
        ] {
            let error = reject(json, ErrorCode::DuplicateObjectKey);
            assert_eq!(error.source, ErrorSource::Wire);
            assert_eq!(json.as_bytes()[error.byte_offset as usize], b'"');
        }
        // Comparison uses exact Unicode scalars, without normalization or folding.
        assert!(decode(r#"{"kind":"request","seq":1,"code":2,"params":{"a":1,"A":2,"é":3,"e\u0301":4,"left":{"a":1},"right":{"a":2}}}"#).is_ok());
    }

    #[test]
    fn duplicate_detection_reports_the_earliest_repeated_key() {
        let json = r#"{"kind":"request","seq":1,"code":2,"params":{"z":0,"a":0,"z":1,"a":1}}"#;
        let error = reject(json, ErrorCode::DuplicateObjectKey);
        assert_eq!(error.byte_offset as usize, json.find("\"z\":1").unwrap());
    }

    #[test]
    fn unknown_keys_and_cross_kind_fields_are_closed() {
        for json in [
            r#"{"kind":"close","reason":"","extra":1}"#,
            r#"{"kind":"request","seq":1,"code":2,"params":{},"result":null}"#,
            r#"{"kind":"immediate","seq":1,"code":2,"result":0,"params":{}}"#,
            r#"{"kind":"stream","seq":1,"code":2,"index":0,"result":0,"method":"x"}"#,
            r#"{"kind":"log","level":"info","message":"","code":0}"#,
            r#"{"kind":"close","reason":"","code":0}"#,
            r#"{"kind":"forwarded","seq":1,"code":2,"target":"x","index":0}"#,
            r#"{"kind":"error","error":{"code":1,"message":"","retryable":true,"extra":0}}"#,
        ] { reject(json, ErrorCode::UnknownObjectKey); }
        let error = reject(r#"{"kind":"close","reason":"","e\u0078tra":1}"#,
            ErrorCode::UnknownObjectKey);
        assert_eq!(error.byte_offset, 28);
    }

    #[test]
    fn every_required_field_is_enforced() {
        let variants: &[&[(&str, &str)]] = &[
            &[("kind", "\"request\""), ("seq", "0"), ("code", "0"), ("params", "{}")],
            &[("kind", "\"immediate\""), ("seq", "0"), ("code", "0"), ("result", "null")],
            &[("kind", "\"stream\""), ("seq", "0"), ("code", "0"), ("index", "0"), ("result", "0")],
            &[("kind", "\"error\""), ("error", "{\"code\":0,\"message\":\"\",\"retryable\":true}")],
            &[("kind", "\"log\""), ("level", "\"info\""), ("message", "\"\"")],
            &[("kind", "\"forwarded\""), ("seq", "0"), ("code", "0"), ("target", "\"x\"")],
            &[("kind", "\"close\""), ("reason", "\"\"")],
        ];
        for fields in variants {
            for missing in 0..fields.len() {
                let mut json = String::from("{");
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index == missing { continue; }
                    if json.len() > 1 { json.push(','); }
                    json.push_str(&format!("\"{key}\":{value}"));
                }
                json.push('}');
                reject(&json, ErrorCode::MissingRequiredField);
            }
        }
        for body in [r#"{"message":"","retryable":false}"#,
            r#"{"code":0,"retryable":false}"#, r#"{"code":0,"message":""}"#]
        {
            reject(&format!("{{\"kind\":\"error\",\"error\":{body}}}"), ErrorCode::MissingRequiredField);
        }
    }

    #[test]
    fn optional_fields_must_be_absent_or_have_the_exact_type() {
        for json in [
            r#"{"kind":"request","seq":0,"code":0,"params":{},"method":null}"#,
            r#"{"kind":"close","seq":null,"reason":""}"#,
            r#"{"kind":"error","seq":null,"error":{"code":0,"message":"","retryable":true}}"#,
            r#"{"kind":"error","code":null,"error":{"code":0,"message":"","retryable":true}}"#,
            r#"{"kind":"log","seq":null,"level":"info","message":""}"#,
            r#"{"kind":"request","seq":0,"code":0,"params":null}"#,
            r#"{"kind":"request","seq":0,"code":0,"params":[]}"#,
            r#"{"kind":"error","error":null}"#,
            r#"{"kind":"error","error":[]}"#,
            r#"{"kind":"error","error":{"code":0,"message":null,"retryable":true}}"#,
            r#"{"kind":"error","error":{"code":0,"message":"","retryable":1}}"#,
            r#"{"kind":"log","level":true,"message":""}"#,
            r#"{"kind":"log","level":"info","message":null}"#,
            r#"{"kind":"forwarded","seq":0,"code":0,"target":null}"#,
            r#"{"kind":"close","reason":null}"#,
            r#"{"kind":null,"reason":""}"#,
        ] { reject(json, ErrorCode::TypeMismatch); }
        assert!(decode(r#"{"kind":"error","seq":0,"code":0,"error":{"code":0,"message":"","retryable":false}}"#).is_ok());
        assert!(decode(r#"{"kind":"log","seq":0,"level":"trace","message":""}"#).is_ok());
    }

    #[test]
    fn metadata_constraints_and_unknown_kinds_are_enforced() {
        for level in ["trace", "debug", "info", "warn", "error"] {
            assert!(decode(&format!("{{\"kind\":\"log\",\"level\":\"{level}\",\"message\":\"\"}}")).is_ok());
        }
        for level in ["", "INFO", "warning", "fatal"] {
            reject(&format!("{{\"kind\":\"log\",\"level\":\"{level}\",\"message\":\"\"}}"),
                ErrorCode::ConstraintViolation);
        }
        assert!(decode(r#"{"kind":"log","level":"w\u0061rn","message":""}"#).is_ok());
        reject(r#"{"kind":"forwarded","seq":0,"code":0,"target":""}"#,
            ErrorCode::ConstraintViolation);
        assert!(decode(r#"{"kind":"forwarded","seq":0,"code":0,"target":"\u0000"}"#).is_ok());
        for kind in ["", "Request", "notification", "mcp", "mcp_request", "response"] {
            reject(&format!("{{\"kind\":\"{kind}\"}}"), ErrorCode::InvalidToken);
        }
    }

    #[test]
    fn complete_json_object_and_only_json_whitespace_are_required() {
        for json in ["null", "[]", "0", "\"x\"", "true"] {
            reject(json, ErrorCode::TypeMismatch);
        }
        for suffix in ["{}", "null", "x", "\u{b}", "\u{a0}"] {
            let json = format!("{{\"kind\":\"close\",\"reason\":\"\"}} \t{suffix}");
            let error = reject(&json, ErrorCode::TrailingContent);
            assert_eq!(error.byte_offset as usize, json.len() - suffix.len());
            assert_eq!(error.source, ErrorSource::Syntax);
        }
        assert!(decode("\t\r\n {\"kind\":\"close\",\"reason\":\"\"}\r\n\t ").is_ok());
        reject("\u{feff}{\"kind\":\"close\",\"reason\":\"\"}", ErrorCode::InvalidToken);
    }

    #[test]
    fn syntax_errors_in_opaque_values_are_never_skipped() {
        for raw in ["[1,]", "{\"a\":1,}", "[1 2]", "{\"a\" 1}", "{1:2}",
            "[}", "{]", "[,1]", "{,}", "[1:2]", "{\"a\":}"]
        {
            let json = format!("{{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":{raw}}}");
            reject(&json, ErrorCode::InvalidSyntax);
        }
        for raw in ["tru", "truex", "False", "undefined", "+1", ".1"] {
            let json = format!("{{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":{raw}}}");
            reject(&json, ErrorCode::InvalidToken);
        }
        reject(r#"{"kind":"close","reason":"",}"#, ErrorCode::InvalidSyntax);
        reject(r#"{"kind":"error","error":{"code":0,"message":"","retryable":true,}}"#,
            ErrorCode::InvalidSyntax);
    }

    #[test]
    fn incomplete_input_has_deterministic_eof_errors() {
        for json in ["", " \n", "{", "{\"kind\"", "{\"kind\":", "{\"kind\":\"close\",",
            "{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":[",
            "{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":tru",
            "{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":1e",
            "{\"kind\":\"close\",\"reason\":\"\\",
            "{\"kind\":\"close\",\"reason\":\"\\u12"]
        {
            let error = reject(json, ErrorCode::UnexpectedEof);
            assert_eq!(error.byte_offset as usize, json.len());
        }
        let json = "{\"kind\":\"close\",\"reason\":\"abc";
        let error = reject(json, ErrorCode::UnclosedString);
        assert_eq!(error.byte_offset as usize, json.find("\"abc").unwrap());
        let valid = r#"{"kind":"request","seq":12,"code":34,"params":{"x":[true,null,"a\nb"]}}"#;
        for end in 0..valid.len() {
            assert!(decode(&valid[..end]).is_err(), "prefix ending at {end}");
        }
    }

    #[test]
    fn string_error_paths_report_the_offending_byte() {
        let json = r#"{"kind":"close","reason":"\q"}"#;
        let error = reject(json, ErrorCode::InvalidEscape);
        assert_eq!(error.byte_offset as usize, json.find('q').unwrap());
        assert_eq!(error.source, ErrorSource::Lexer);
        let json = r#"{"kind":"close","reason":"\u12x4"}"#;
        let error = reject(json, ErrorCode::InvalidEscape);
        assert_eq!(error.byte_offset as usize, json.find('x').unwrap());
        for raw in [r#""\uD800""#, r#""\uDC00""#, r#""\uD800\u0041""#,
            r#""\uDBFFx""#, r#""\uD800\n""#]
        {
            let json = format!("{{\"kind\":\"close\",\"reason\":{raw}}}");
            let error = reject(&json, ErrorCode::LoneSurrogate);
            assert_eq!(error.byte_offset as usize, json.find('\\').unwrap());
        }
        for control in [0u8, 8, 9, 10, 13, 31] {
            let json = format!("{{\"kind\":\"close\",\"reason\":\"a{}b\"}}", char::from(control));
            let error = reject(&json, ErrorCode::ControlCharInString);
            assert_eq!(json.as_bytes()[error.byte_offset as usize], control);
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_in_strings_and_elsewhere() {
        for invalid in [&[0xff][..], &[0xc0, 0xaf], &[0xed, 0xa0, 0x80], &[0xf4, 0x90, 0x80, 0x80], &[0xe2, 0x82]] {
            let mut bytes = br#"{"kind":"close","reason":""#.to_vec();
            let offset = bytes.len();
            bytes.extend_from_slice(invalid);
            bytes.extend_from_slice(b"\"}");
            let error = decode_envelope(Payload::from_vec(bytes), EnvelopeLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidUtf8);
            assert_eq!(error.source, ErrorSource::Utf8);
            assert_eq!(error.byte_offset as usize, offset);
        }
        let error = decode_envelope(Payload::from_slice(&[0xff]), EnvelopeLimits::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidUtf8);
        assert_eq!(error.byte_offset, 0);
    }

    #[test]
    fn every_zero_limit_is_invalid_configuration() {
        let defaults = EnvelopeLimits::default();
        let limits = [
            EnvelopeLimits { max_payload_bytes: 0, ..defaults },
            EnvelopeLimits { max_decoded_bytes: 0, ..defaults },
            EnvelopeLimits { max_depth: 0, ..defaults },
            EnvelopeLimits { max_fields: 0, ..defaults },
        ];
        for limits in limits {
            let error = decode_envelope(Payload::from_slice(b"{}"), limits).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
            assert_eq!(error.source, ErrorSource::Wire);
            assert_eq!(error.byte_offset, 0);
        }
    }

    #[test]
    fn payload_limit_is_inclusive_and_precedes_parsing() {
        let json = r#"{"kind":"close","reason":""}"#;
        let limits = EnvelopeLimits { max_payload_bytes: json.len() as u32,
            ..EnvelopeLimits::default() };
        assert!(decode_envelope(Payload::from_slice(json.as_bytes()), limits).is_ok());
        let error = decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_payload_bytes: limits.max_payload_bytes - 1, ..limits }).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        let error = decode_envelope(Payload::from_slice(b"malformed"),
            EnvelopeLimits { max_payload_bytes: 1, ..limits }).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn decoded_arena_limit_is_inclusive_and_counts_all_escaped_strings() {
        let json = r#"{"kind":"request","seq":0,"code":0,"params":{"\u0061":"\u00e9"},"method":"\uD83D\uDE00"}"#;
        let limits = EnvelopeLimits { max_decoded_bytes: 7, ..EnvelopeLimits::default() };
        let envelope = decode_envelope(Payload::from_slice(json.as_bytes()), limits).unwrap();
        assert_eq!(envelope.decoded_bytes(), "aé😀".as_bytes());
        let error = decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_decoded_bytes: 6, ..limits }).unwrap_err();
        assert_eq!(error.code, ErrorCode::DecodedArenaOverflow);
        assert_eq!(error.byte_offset as usize, json.find("\\uD83D").unwrap());
        let raw = r#"{"kind":"close","reason":"a long unescaped string"}"#;
        assert!(decode_envelope(Payload::from_slice(raw.as_bytes()),
            EnvelopeLimits { max_decoded_bytes: 1, ..limits }).is_ok());
        let json = r#"{"kind":"close","reason":"\nabc"}"#;
        let error = decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_decoded_bytes: 3, ..limits }).unwrap_err();
        assert_eq!(error.code, ErrorCode::DecodedArenaOverflow);
        assert_eq!(json.as_bytes()[error.byte_offset as usize], b'c');
        let json = r#"{"kind":"close","reason":"abcd\n"}"#;
        let error = decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_decoded_bytes: 3, ..limits }).unwrap_err();
        assert_eq!(json.as_bytes()[error.byte_offset as usize], b'd');
    }

    #[test]
    fn depth_limit_counts_outer_and_error_objects_and_all_opaque_containers() {
        let defaults = EnvelopeLimits::default();
        let close = br#"{"kind":"close","reason":""}"#;
        assert!(decode_envelope(Payload::from_slice(close),
            EnvelopeLimits { max_depth: 1, ..defaults }).is_ok());
        let json = br#"{"kind":"request","seq":0,"code":0,"params":{}}"#;
        let error = decode_envelope(Payload::from_slice(json),
            EnvelopeLimits { max_depth: 1, ..defaults }).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
        assert!(decode_envelope(Payload::from_slice(json),
            EnvelopeLimits { max_depth: 2, ..defaults }).is_ok());
        let error_json = br#"{"kind":"error","error":{"code":0,"message":"","retryable":false}}"#;
        assert_eq!(decode_envelope(Payload::from_slice(error_json),
            EnvelopeLimits { max_depth: 1, ..defaults }).unwrap_err().code,
            ErrorCode::DepthLimitExceeded);
        assert!(decode_envelope(Payload::from_slice(error_json),
            EnvelopeLimits { max_depth: 2, ..defaults }).is_ok());
        let json = r#"{"kind":"immediate","seq":0,"code":0,"result":[{"x":[]}]}"#;
        assert!(decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_depth: 4, ..defaults }).is_ok());
        let error = decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_depth: 3, ..defaults }).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
        assert_eq!(error.byte_offset as usize, json.find("[]").unwrap());
    }

    #[test]
    fn deeply_nested_values_use_a_bounded_explicit_stack() {
        let depth = 1024usize;
        let json = format!("{{\"kind\":\"immediate\",\"seq\":0,\"code\":0,\"result\":{}0{}}}",
            "[".repeat(depth), "]".repeat(depth));
        let limits = EnvelopeLimits { max_depth: (depth + 1) as u16,
            ..EnvelopeLimits::default() };
        assert!(decode_envelope(Payload::from_slice(json.as_bytes()), limits).is_ok());
        let error = decode_envelope(Payload::from_slice(json.as_bytes()),
            EnvelopeLimits { max_depth: depth as u16, ..limits }).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn field_limit_applies_per_object_and_is_inclusive() {
        let defaults = EnvelopeLimits::default();
        let json = r#"{"kind":"request","seq":0,"code":0,"params":{"a":1,"b":2,"c":3,"d":4}}"#;
        let limits = EnvelopeLimits { max_fields: 4, ..defaults };
        assert!(decode_envelope(Payload::from_slice(json.as_bytes()), limits).is_ok());
        let json = r#"{"kind":"request","seq":0,"code":0,"params":{"a":1,"b":2,"c":3,"d":4,"e":5}}"#;
        let error = decode_envelope(Payload::from_slice(json.as_bytes()), limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.byte_offset as usize, json.find("\"e\"").unwrap());
        let json = br#"{"kind":"close","reason":""}"#;
        assert!(decode_envelope(Payload::from_slice(json),
            EnvelopeLimits { max_fields: 2, ..defaults }).is_ok());
        assert_eq!(decode_envelope(Payload::from_slice(json),
            EnvelopeLimits { max_fields: 1, ..defaults }).unwrap_err().code,
            ErrorCode::ConstraintViolation);
        let json = r#"{"kind":"request","seq":0,"code":0,"params":{"x":{"a":1,"b":2,"c":3,"d":4},"y":{"a":1,"b":2,"c":3,"d":4}}}"#;
        assert!(decode_envelope(Payload::from_slice(json.as_bytes()), limits).is_ok());
    }

    #[test]
    fn container_like_text_does_not_affect_nesting_or_key_scope() {
        let json = r#"{"kind":"request","seq":0,"code":0,"params":{"x":"[{}] , : \" \\","arr":[{"x":1},{"x":2}],"kind":"arbitrary","method":"app"}}"#;
        let envelope = decode(json).unwrap();
        assert_eq!(envelope.view().kind, EnvelopeKind::EndpointRequest);
        assert_eq!(envelope.method(), None);
        assert!(envelope.resolve(envelope.view().params_span.unwrap()).unwrap().ends_with(b"}"));
    }

    #[test]
    fn parser_storage_reservations_are_bounded_and_fallible() {
        let mut entries = Vec::<u8>::new();
        reserve_entry(&mut entries, 1, 9, "allocation failed").unwrap();
        assert!(entries.is_empty());
        assert!(entries.capacity() >= 1);
        entries.push(1);
        let error = reserve_entry(&mut entries, 1, 9, "allocation failed").unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 9);
        assert_eq!(entries, vec![1]);
        let mut entries = Vec::<[u8; 16]>::new();
        let error = reserve_entry(&mut entries, usize::MAX, 7, "allocation failed");
        // A one-entry reservation is valid even when the configured ceiling is
        // huge; the parser does not eagerly allocate the entire configured cap.
        assert!(error.is_ok());
        let error = entries.try_reserve_exact(usize::MAX);
        assert!(error.is_err());
    }

    #[test]
    fn error_metadata_never_includes_payload_secrets() {
        let error = reject(r#"{"kind":"close","reason":"secret","password":"sensitive"}"#,
            ErrorCode::UnknownObjectKey);
        assert!(!error.detail.contains("secret"));
        assert!(!error.to_string().contains("sensitive"));
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
    }
}
