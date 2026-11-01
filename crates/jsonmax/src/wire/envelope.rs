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
const TYPE: usize = 12;
const ORIGINAL_SEQ: usize = 13;
const STREAM_SEQ: usize = 14;
const STREAM_CODE: usize = 15;
const DATA: usize = 16;
const LOG_ID: usize = 17;

const ENVELOPE_KEYS: [&[u8]; 18] = [
    b"kind", b"seq", b"code", b"params", b"method", b"result", b"index",
    b"error", b"level", b"message", b"target", b"reason", b"type",
    b"original_seq", b"stream_seq", b"stream_code", b"data", b"log_id",
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

    let (view, method_code, stream_seq, stream_code, decoded) = {
        let mut parser = Parser { bytes: payload.as_slice(), position: 0,
            decoded: Vec::new(), limits };
        let record = parser.record(0, &ENVELOPE_KEYS, Some(ERROR))?;
        parser.whitespace();
        if parser.position != parser.bytes.len() {
            return Err(parser.syntax(ErrorCode::TrailingContent, parser.position,
                "content follows the envelope"));
        }
        let (view, method_code) = parser.envelope_view(&record)?;
        let stream_seq = record.optional(STREAM_SEQ)
            .map(|value| parser.unsigned(value, u64::from(u32::MAX))).transpose()?;
        let stream_code = record.optional(STREAM_CODE)
            .map(|value| parser.unsigned(value, u64::from(u32::MAX)).map(|code| code as u32)).transpose()?;
        (view, method_code, stream_seq, stream_code, parser.decoded.into_boxed_slice())
    };
    Ok(OwnedEnvelope { payload, decoded, view, method_code, stream_seq, stream_code })
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
            self.whitespace();
            let value = if error_field == Some(index)
                && self.bytes.get(self.position) == Some(&b'{') {
                self.rpc_error(depth)?
            } else { self.value(depth)? };
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

    fn envelope_view(&self, record: &Record<18>) -> Result<(WireEnvelopeView, Option<u32>)> {
        let kind_value = record.optional(KIND).or_else(|| record.optional(TYPE));
        let tagged = record.optional(TYPE).is_some();
        let (kind, required, allowed) = if kind_value.is_none() && record.optional(PARAMS).is_some() {
            (EnvelopeKind::EndpointRequest, bits(&[SEQ, PARAMS]),
                bits(&[KIND, TYPE, SEQ, CODE, PARAMS, METHOD]))
        } else {
        let discriminator = self.text(kind_value.ok_or_else(|| local_error(
            ErrorCode::MissingRequiredField, record.span.offset, "envelope kind or type is absent"))?)?;
        match discriminator {
            b"request" => (EnvelopeKind::EndpointRequest,
                bits(&[SEQ, PARAMS]), bits(&[KIND, TYPE, SEQ, CODE, PARAMS, METHOD])),
            b"immediate" => (EnvelopeKind::EndpointImmediate,
                bits(&[SEQ, CODE, RESULT]), bits(&[KIND, SEQ, CODE, RESULT])),
            b"stream" => (EnvelopeKind::EndpointStream,
                bits(&[SEQ, CODE, INDEX, RESULT]), bits(&[KIND, SEQ, CODE, INDEX, RESULT])),
            b"error" => (EnvelopeKind::EndpointError,
                bits(&[ERROR]), bits(&[KIND, TYPE, ERROR, SEQ, CODE, MESSAGE, LOG_ID])),
            b"log" => (EnvelopeKind::EndpointLog,
                bits(&[LEVEL, MESSAGE]), bits(&[KIND, TYPE, LEVEL, MESSAGE, SEQ])),
            b"forwarded" => (EnvelopeKind::EndpointForwarded,
                bits(&[SEQ, CODE, TARGET]), bits(&[KIND, TYPE, SEQ, CODE, TARGET])),
            b"close" => (EnvelopeKind::EndpointClose,
                bits(&[REASON]), bits(&[KIND, TYPE, REASON, SEQ])),
            b"Immediate" => (EnvelopeKind::EndpointImmediate,
                0, bits(&[TYPE, METHOD, SEQ, ORIGINAL_SEQ, PARAMS, DATA, RESULT, LOG_ID])),
            b"Stream" => (EnvelopeKind::EndpointStream,
                bits(&[ORIGINAL_SEQ, STREAM_SEQ, STREAM_CODE, DATA]),
                bits(&[TYPE, METHOD, ORIGINAL_SEQ, STREAM_SEQ, STREAM_CODE, DATA])),
            b"Error" => (EnvelopeKind::EndpointError,
                bits(&[CODE]), bits(&[TYPE, METHOD, SEQ, ORIGINAL_SEQ, CODE, PARAMS, MESSAGE, ERROR, LOG_ID])),
            b"Log" => (EnvelopeKind::EndpointLog,
                0, bits(&[TYPE, SEQ, MESSAGE, LEVEL, LOG_ID])),
            b"Forwarded" => (EnvelopeKind::EndpointForwarded,
                0, bits(&[TYPE, METHOD, SEQ, ORIGINAL_SEQ, CODE, TARGET, DATA, LOG_ID])),
            b"Close" => (EnvelopeKind::EndpointClose,
                0, bits(&[TYPE, SEQ, ORIGINAL_SEQ, REASON, LOG_ID])),
            _ => return Err(local_error(ErrorCode::InvalidToken, kind_value.map_or(record.span.offset, |value| value.span.offset),
                "unknown endpoint envelope kind")),
        }};
        if kind == EnvelopeKind::EndpointImmediate && tagged
            && record.optional(PARAMS).is_none()
            && record.optional(DATA).is_none() && record.optional(RESULT).is_none() {
            return Err(local_error(ErrorCode::MissingRequiredField, record.span.offset,
                "tagged immediate response data is absent"));
        }
        if kind == EnvelopeKind::EndpointImmediate && tagged
            && record.optional(SEQ).is_none() && record.optional(ORIGINAL_SEQ).is_none() {
            return Err(local_error(ErrorCode::MissingRequiredField, record.span.offset,
                "tagged immediate response sequence is absent"));
        }
        if kind == EnvelopeKind::EndpointError && tagged
            && record.optional(PARAMS).is_none()
            && record.optional(MESSAGE).is_none() && record.optional(ERROR).is_none() {
            return Err(local_error(ErrorCode::MissingRequiredField, record.span.offset,
                "tagged error message is absent"));
        }
        if tagged && record.optional(KIND).is_some() {
            return Err(local_error(ErrorCode::ConstraintViolation, record.span.offset,
                "kind and type discriminators cannot be combined"));
        }
        for (index, member) in record.members.iter().enumerate() {
            if let Some(member) = member {
                if allowed & (1u32 << index) == 0 {
                    return Err(local_error(ErrorCode::UnknownObjectKey, member.key_offset,
                        "field is not allowed for this envelope kind"));
                }
            } else if required & (1u32 << index) != 0 {
                return Err(local_error(ErrorCode::MissingRequiredField, record.span.offset,
                    "required envelope field is absent"));
            }
        }
        let seq_value = record.optional(SEQ).or_else(|| record.optional(ORIGINAL_SEQ));
        let numeric_request = kind == EnvelopeKind::EndpointRequest
            && record.optional(METHOD).is_some_and(|value| value.kind == ValueKind::Number);
        let sequence_max = if tagged || numeric_request { u64::from(u32::MAX) } else { u64::MAX };
        let seq = seq_value.map(|value| self.unsigned(value, sequence_max)).transpose()?;
        let code_value = record.optional(CODE).or_else(|| record.optional(STREAM_CODE));
        let code = if kind == EnvelopeKind::EndpointError && tagged {
            // Tagged status codes are distinct from method IDs. Accept the
            // endpoint-libs u32 domain and the signed native error extension.
            if let Some(value) = code_value {
                if value.span.slice(self.bytes).and_then(|raw| raw.first()) == Some(&b'-') {
                    self.signed_code(value)?;
                } else { self.unsigned(value, u64::from(u32::MAX))?; }
            }
            None
        } else {
            code_value.map(|value|
                self.unsigned(value, u64::from(u32::MAX)).map(|number| number as u32)).transpose()?
        };
        let (method, method_code) = if let Some(value) = record.optional(METHOD) {
            match value.kind {
                ValueKind::String if !tagged => (Some(self.expect_string(value)?), None),
                ValueKind::Number => (None, Some(self.unsigned(value, u64::from(u32::MAX))? as u32)),
                _ => return Err(local_error(ErrorCode::TypeMismatch, value.span.offset,
                    "method must be a string or unsigned 32-bit integer")),
            }
        } else { (None, None) };
        let code = if tagged { method_code.or(code) } else { code.or(method_code) };
        if let (Some(explicit_code), Some(method_code)) =
            (record.optional(CODE).filter(|_| kind == EnvelopeKind::EndpointRequest), method_code) {
            if self.unsigned(explicit_code, u64::from(u32::MAX))? != u64::from(method_code) {
                return Err(local_error(ErrorCode::ConstraintViolation, explicit_code.span.offset,
                    "numeric method disagrees with request code"));
            }
        }
        if kind == EnvelopeKind::EndpointRequest && (record.optional(METHOD).is_none() || code.is_none()) {
            return Err(local_error(ErrorCode::MissingRequiredField, record.span.offset,
                "request method is absent"));
        }
        if let Some(params) = record.optional(PARAMS).filter(|_| kind == EnvelopeKind::EndpointRequest) {
            if params.kind != ValueKind::Object {
                return Err(local_error(ErrorCode::TypeMismatch, params.span.offset,
                    "request params must be an object"));
            }
        }
        let index_value = record.optional(INDEX).or_else(|| record.optional(STREAM_SEQ));
        if let Some(index) = index_value { self.unsigned(index, sequence_max)?; }
        if let Some(level) = record.optional(LEVEL) {
            let text = self.text(level)?;
            if !matches!(text, b"trace" | b"debug" | b"info" | b"warn" | b"error")
                && !(tagged && matches!(text, b"off" | b"detail")) {
                return Err(local_error(ErrorCode::ConstraintViolation, level.span.offset,
                    "unknown log level"));
            }
        }
        if let Some(message) = record.optional(MESSAGE) { self.expect_string(message)?; }
        if let Some(log_id) = record.optional(LOG_ID) {
            if tagged && kind == EnvelopeKind::EndpointError { self.expect_string(log_id)?; }
            else { self.unsigned(log_id, u64::MAX)?; }
        }
        if let Some(target) = record.optional(TARGET) {
            if self.text(target)?.is_empty() {
                return Err(local_error(ErrorCode::ConstraintViolation, target.span.offset,
                    "forward target must not be empty"));
            }
        }
        if let Some(reason) = record.optional(REASON) { self.expect_string(reason)?; }
        let result = record.optional(RESULT).or_else(|| record.optional(DATA))
            .or_else(|| record.optional(PARAMS).filter(|_| kind == EnvelopeKind::EndpointImmediate));
        let error_span = if kind == EnvelopeKind::EndpointError {
            if let Some(error) = record.optional(ERROR) {
                if error.kind != ValueKind::Object {
                    self.expect_string(error)?;
                }
                Some(error.span)
            } else if let Some(message) = record.optional(MESSAGE) {
                self.expect_string(message)?;
                Some(message.span)
            } else { record.optional(PARAMS).map(|value| value.span) }
        } else { record.optional(ERROR).map(|value| value.span) };
        Ok((WireEnvelopeView { kind, method, seq, code,
            params_span: record.optional(PARAMS).map(|value| value.span),
            result_span: result.map(|value| value.span), error_span }, method_code))
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

fn bits(fields: &[usize]) -> u32 {
    fields.iter().fold(0, |mask, index| mask | (1u32 << *index))
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
