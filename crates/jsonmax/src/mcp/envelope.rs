//! Bounded JSON parsing and owned JSON-RPC request envelopes.
//!
//! Container traversal uses an explicit stack. Object keys are compared after
//! decoding, and the entire document is checked before an envelope is trusted.

use super::*;

/// Decode one request or notification, translating ABI errors at this boundary.
pub fn decode_message(payload: Payload, limits: EnvelopeLimits) -> McpResult<McpEnvelope> {
    let parsed = parse(&payload, limits).map_err(|error| {
        let exhausted = matches!(
            error.code,
            ErrorCode::PayloadTooLarge
                | ErrorCode::DecodedArenaOverflow
                | ErrorCode::DepthLimitExceeded
                | ErrorCode::BudgetExhausted
                | ErrorCode::ConstraintViolation
                | ErrorCode::InternalError
        );
        protocol_failure(
            None,
            if exhausted { INTERNAL_ERROR } else { PARSE_ERROR },
            if exhausted { "JSON resource limit exceeded" } else { "Invalid JSON" },
        )
    })?;

    if parsed.value.kind != Kind::Object {
        return Err(protocol_failure(None, INVALID_REQUEST, "Expected a request object"));
    }

    let mut id = None;
    let mut method = None;
    let mut params = None;
    let mut version_ok = false;
    let mut invalid = parsed.duplicate.is_some();
    for field in parsed.fields {
        // The parser only completes fields after their whole value is valid.
        let Some(value) = field.value else {
            return Err(protocol_failure(None, INTERNAL_ERROR, "Incomplete request field"));
        };
        match field.name.as_str() {
            "jsonrpc" => version_ok = value.text.as_deref() == Some(JSONRPC_VERSION),
            "id" => match request_id(value, payload.as_slice()) {
                Some(value) => id = Some(value),
                None => invalid = true,
            },
            "method" => match value.text {
                Some(text) if !text.is_empty() => method = Some(text),
                _ => invalid = true,
            },
            "params" => {
                if matches!(value.kind, Kind::Object | Kind::Array) {
                    params = Some((value.start, value.end));
                } else {
                    invalid = true;
                }
            }
            _ => invalid = true,
        }
    }
    if parsed.top_duplicate {
        id = None;
    }
    if invalid || !version_ok || method.is_none() {
        return Err(protocol_failure(id, INVALID_REQUEST, "Invalid request envelope"));
    }
    let params = match params {
        Some((start, end)) => Some(copy_payload(&payload.as_slice()[start..end], start)
            .map_err(|_| protocol_failure(id.take(), INTERNAL_ERROR, "Request allocation failed"))?),
        None => None,
    };
    // Presence was checked above, without allocating another copy of the method.
    match method {
        Some(method) => Ok(McpEnvelope { id, method, params }),
        None => Err(protocol_failure(id, INTERNAL_ERROR, "Missing decoded method")),
    }
}

/// Validate exactly one JSON value, including all nested decoded object keys.
pub fn validate_json(payload: &Payload, limits: EnvelopeLimits) -> Result<()> {
    let parsed = parse(payload, limits)?;
    match parsed.duplicate {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Return an independent copy of a top-level field's complete raw JSON value.
pub fn object_field(payload: &Payload, name: &str, limits: EnvelopeLimits) -> Result<Option<Payload>> {
    let parsed = parse(payload, limits)?;
    if let Some(error) = parsed.duplicate {
        return Err(error);
    }
    if parsed.value.kind != Kind::Object {
        return Err(local_error(ErrorCode::TypeMismatch, parsed.value.start, "expected object"));
    }
    for field in parsed.fields {
        if field.name == name {
            let value = field.value.ok_or_else(|| {
                local_error(ErrorCode::InternalError, field.offset, "incomplete object field")
            })?;
            return copy_payload(&payload.as_slice()[value.start..value.end], value.start).map(Some);
        }
    }
    Ok(None)
}

/// Decode a complete JSON string, including UTF-16 surrogate-pair escapes.
pub fn decode_string(payload: &Payload, limits: EnvelopeLimits) -> Result<String> {
    let parsed = parse(payload, limits)?;
    if let Some(error) = parsed.duplicate {
        return Err(error);
    }
    parsed.value.text.ok_or_else(|| {
        local_error(ErrorCode::TypeMismatch, parsed.value.start, "expected string")
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    String,
    Number,
    Null,
    Bool,
}

struct Value {
    kind: Kind,
    start: usize,
    end: usize,
    text: Option<String>,
}

struct Field {
    name: String,
    offset: usize,
    value: Option<Value>,
}

#[derive(Clone, Copy)]
enum Phase {
    Key { allow_end: bool },
    Colon,
    ObjectValue,
    ObjectAfter,
    ArrayValue { allow_end: bool },
    ArrayAfter,
}

struct Frame {
    kind: Kind,
    start: usize,
    phase: Phase,
    fields: Vec<Field>,
}

struct Parsed {
    value: Value,
    fields: Vec<Field>,
    duplicate: Option<Error>,
    top_duplicate: bool,
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
    decoded: usize,
    limits: EnvelopeLimits,
    stack: Vec<Frame>,
    duplicate: Option<Error>,
    top_duplicate: bool,
}

fn local_error(code: ErrorCode, offset: usize, detail: &'static str) -> Error {
    // Input length is checked against a u32 limit before any parser offset is used.
    Error::new(code, ErrorSource::Wire, offset as u32, detail)
}

fn protocol_failure(id: Option<McpId>, code: i32, message: &'static str) -> McpFailure {
    McpFailure { id: id.unwrap_or(McpId::Null), code, message: String::from(message) }
}

fn copy_payload(bytes: &[u8], offset: usize) -> Result<Payload> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(bytes.len()).map_err(|_| {
        local_error(ErrorCode::BudgetExhausted, offset, "payload allocation failed")
    })?;
    owned.extend_from_slice(bytes);
    Ok(Payload::from_vec(owned))
}

fn request_id(value: Value, bytes: &[u8]) -> Option<McpId> {
    match value.kind {
        Kind::String => value.text.map(McpId::String),
        Kind::Null => Some(McpId::Null),
        Kind::Number => {
            let raw = &bytes[value.start..value.end];
            if raw == b"-0" || raw.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                return None;
            }
            core::str::from_utf8(raw).ok()?.parse::<i64>().ok().map(McpId::Number)
        }
        _ => None,
    }
}

fn parse(payload: &Payload, limits: EnvelopeLimits) -> Result<Parsed> {
    if limits.max_payload_bytes == 0 || limits.max_decoded_bytes == 0
        || limits.max_depth == 0 || limits.max_fields == 0
    {
        return Err(local_error(ErrorCode::ConstraintViolation, 0, "JSON limits must be nonzero"));
    }
    if payload.len() > limits.max_payload_bytes as usize {
        return Err(local_error(ErrorCode::PayloadTooLarge, 0, "JSON payload exceeds limit"));
    }
    let bytes = payload.as_slice();
    core::str::from_utf8(bytes).map_err(|error| {
        local_error(ErrorCode::InvalidUtf8, error.valid_up_to(), "invalid JSON UTF-8")
    })?;
    Parser {
        bytes,
        position: 0,
        decoded: 0,
        limits,
        stack: Vec::new(),
        duplicate: None,
        top_duplicate: false,
    }.document()
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn error(&self, code: ErrorCode, detail: &'static str) -> Error {
        local_error(code, self.position, detail)
    }

    fn document(mut self) -> Result<Parsed> {
        self.whitespace();
        let mut completed = self.value()?;
        loop {
            if let Some(value) = completed.take() {
                if self.stack.is_empty() {
                    return self.finish(value, Vec::new());
                }
                self.attach(value)?;
            }
            self.whitespace();
            let phase = self.stack.last().ok_or_else(|| {
                self.error(ErrorCode::InternalError, "missing JSON container")
            })?.phase;
            match phase {
                Phase::Key { allow_end } => {
                    if self.peek() == Some(b'}') && allow_end {
                        let (value, fields) = self.close()?;
                        if self.stack.is_empty() {
                            return self.finish(value, fields);
                        }
                        completed = Some(value);
                    } else {
                        if self.peek() != Some(b'"') {
                            return Err(self.expected("expected object key"));
                        }
                        let offset = self.position;
                        if self.stack.last().map_or(0, |frame| frame.fields.len())
                            >= self.limits.max_fields as usize
                        {
                            return Err(self.error(ErrorCode::PayloadTooLarge, "object field limit exceeded"));
                        }
                        let name = self.string()?;
                        let frame = self.stack.last_mut().ok_or_else(|| {
                            local_error(ErrorCode::InternalError, offset, "missing object")
                        })?;
                        frame.fields.try_reserve(1).map_err(|_| {
                            local_error(ErrorCode::BudgetExhausted, offset, "object allocation failed")
                        })?;
                        frame.fields.push(Field { name, offset, value: None });
                        frame.phase = Phase::Colon;
                    }
                }
                Phase::Colon => {
                    if self.peek() != Some(b':') {
                        return Err(self.expected("expected colon"));
                    }
                    self.position += 1;
                    self.set_phase(Phase::ObjectValue)?;
                }
                Phase::ObjectValue => completed = self.value()?,
                Phase::ArrayValue { allow_end } => {
                    if self.peek() == Some(b']') && allow_end {
                        let (value, fields) = self.close()?;
                        if self.stack.is_empty() {
                            return self.finish(value, fields);
                        }
                        completed = Some(value);
                    } else {
                        completed = self.value()?;
                    }
                }
                Phase::ObjectAfter | Phase::ArrayAfter => {
                    let object = matches!(phase, Phase::ObjectAfter);
                    let end = if object { b'}' } else { b']' };
                    if self.peek() == Some(end) {
                        let (value, fields) = self.close()?;
                        if self.stack.is_empty() {
                            return self.finish(value, fields);
                        }
                        completed = Some(value);
                    } else if self.peek() == Some(b',') {
                        self.position += 1;
                        self.set_phase(if object {
                            Phase::Key { allow_end: false }
                        } else {
                            Phase::ArrayValue { allow_end: false }
                        })?;
                    } else {
                        return Err(self.expected("expected comma or container end"));
                    }
                }
            }
        }
    }

    fn expected(&self, detail: &'static str) -> Error {
        self.error(if self.peek().is_none() { ErrorCode::UnexpectedEof } else { ErrorCode::InvalidSyntax }, detail)
    }

    fn set_phase(&mut self, phase: Phase) -> Result<()> {
        let offset = self.position;
        let frame = self.stack.last_mut().ok_or_else(|| {
            local_error(ErrorCode::InternalError, offset, "missing container")
        })?;
        frame.phase = phase;
        Ok(())
    }

    fn attach(&mut self, value: Value) -> Result<()> {
        let offset = value.start;
        let top_level = self.stack.len() == 1;
        let frame = self.stack.last_mut().ok_or_else(|| {
            local_error(ErrorCode::InternalError, offset, "missing value parent")
        })?;
        match frame.phase {
            Phase::ObjectValue => {
                // Only the root object's values are needed after validation.
                if top_level {
                    let field = frame.fields.last_mut().ok_or_else(|| {
                        local_error(ErrorCode::InternalError, offset, "missing value key")
                    })?;
                    field.value = Some(value);
                }
                frame.phase = Phase::ObjectAfter;
            }
            Phase::ArrayValue { .. } => frame.phase = Phase::ArrayAfter,
            _ => return Err(local_error(ErrorCode::InternalError, offset, "unexpected value parent")),
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(Value, Vec<Field>)> {
        let mut frame = self.stack.pop().ok_or_else(|| {
            self.error(ErrorCode::InternalError, "missing closing container")
        })?;
        self.position += 1;
        // In-place sorting avoids a second key arena and quadratic comparisons.
        frame.fields.sort_unstable_by(|a, b| {
            a.name.cmp(&b.name).then_with(|| a.offset.cmp(&b.offset))
        });
        for pair in frame.fields.windows(2) {
            if pair[0].name == pair[1].name {
                let offset = pair[1].offset;
                if self.stack.is_empty() {
                    self.top_duplicate = true;
                }
                if self.duplicate.as_ref().map_or(true, |error| offset < error.byte_offset as usize) {
                    self.duplicate = Some(local_error(
                        ErrorCode::DuplicateObjectKey, offset, "duplicate decoded object key",
                    ));
                }
            }
        }
        let value = Value { kind: frame.kind, start: frame.start, end: self.position, text: None };
        Ok((value, frame.fields))
    }

    fn finish(mut self, value: Value, fields: Vec<Field>) -> Result<Parsed> {
        self.whitespace();
        if self.position != self.bytes.len() {
            return Err(self.error(ErrorCode::TrailingContent, "trailing JSON content"));
        }
        Ok(Parsed { value, fields, duplicate: self.duplicate, top_duplicate: self.top_duplicate })
    }

    fn value(&mut self) -> Result<Option<Value>> {
        let start = self.position;
        let mut text = None;
        let kind = match self.peek() {
            Some(b'{' | b'[') => {
                if self.stack.len() >= self.limits.max_depth as usize {
                    return Err(self.error(ErrorCode::DepthLimitExceeded, "JSON nesting limit exceeded"));
                }
                let object = self.peek() == Some(b'{');
                self.stack.try_reserve(1).map_err(|_| {
                    self.error(ErrorCode::BudgetExhausted, "parser stack allocation failed")
                })?;
                self.stack.push(Frame {
                    kind: if object { Kind::Object } else { Kind::Array },
                    start,
                    phase: if object { Phase::Key { allow_end: true } } else { Phase::ArrayValue { allow_end: true } },
                    fields: Vec::new(),
                });
                self.position += 1;
                return Ok(None);
            }
            Some(b'"') => {
                text = Some(self.string()?);
                Kind::String
            }
            Some(b'-' | b'0'..=b'9') => {
                self.number()?;
                Kind::Number
            }
            Some(b'n') => { self.literal(b"null")?; Kind::Null }
            Some(b't') => { self.literal(b"true")?; Kind::Bool }
            Some(b'f') => { self.literal(b"false")?; Kind::Bool }
            None => return Err(self.error(ErrorCode::UnexpectedEof, "expected JSON value")),
            _ => return Err(self.error(ErrorCode::InvalidToken, "expected JSON value")),
        };
        Ok(Some(Value { kind, start, end: self.position, text }))
    }

    fn literal(&mut self, expected: &[u8]) -> Result<()> {
        for &byte in expected {
            match self.peek() {
                Some(actual) if actual == byte => self.position += 1,
                None => return Err(self.error(ErrorCode::UnexpectedEof, "incomplete JSON literal")),
                _ => return Err(self.error(ErrorCode::InvalidToken, "invalid JSON literal")),
            }
        }
        Ok(())
    }

    fn number(&mut self) -> Result<()> {
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.position += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error(ErrorCode::InvalidNumber, "leading zero in JSON number"));
                }
            }
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.error(ErrorCode::InvalidNumber, "expected integer digits")),
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error(ErrorCode::InvalidNumber, "expected fractional digits"));
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error(ErrorCode::InvalidNumber, "expected exponent digits"));
            }
            self.digits();
        }
        Ok(())
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
    }

    fn append(&mut self, output: &mut String, text: &str, offset: usize) -> Result<()> {
        let decoded = self.decoded.checked_add(text.len()).ok_or_else(|| {
            local_error(ErrorCode::PayloadTooLarge, offset, "decoded JSON size overflow")
        })?;
        if decoded > self.limits.max_decoded_bytes as usize {
            return Err(local_error(ErrorCode::PayloadTooLarge, offset, "decoded JSON exceeds limit"));
        }
        output.try_reserve(text.len()).map_err(|_| {
            local_error(ErrorCode::BudgetExhausted, offset, "string allocation failed")
        })?;
        output.push_str(text);
        self.decoded = decoded;
        Ok(())
    }

    fn string(&mut self) -> Result<String> {
        self.position += 1; // The caller has checked the opening quote.
        let mut output = String::new();
        loop {
            let start = self.position;
            while matches!(self.peek(), Some(byte) if byte >= 0x20 && byte != b'"' && byte != b'\\') {
                self.position += 1;
            }
            if self.position != start {
                let bytes = self.bytes;
                let text = core::str::from_utf8(&bytes[start..self.position]).map_err(|error| {
                    local_error(ErrorCode::InvalidUtf8, start + error.valid_up_to(), "invalid string UTF-8")
                })?;
                self.append(&mut output, text, start)?;
            }
            match self.peek() {
                Some(b'"') => {
                    self.position += 1;
                    return Ok(output);
                }
                Some(b'\\') => {
                    let escape = self.position;
                    self.position += 1;
                    let escaped = self.peek().ok_or_else(|| {
                        self.error(ErrorCode::UnexpectedEof, "incomplete string escape")
                    })?;
                    self.position += 1;
                    let scalar = match escaped {
                        b'"' => '"', b'\\' => '\\', b'/' => '/',
                        b'b' => '\u{08}', b'f' => '\u{0c}', b'n' => '\n', b'r' => '\r', b't' => '\t',
                        b'u' => {
                            let first = self.hex_quad()?;
                            let code = if (0xd800..=0xdbff).contains(&first) {
                                if !self.bytes.get(self.position..).map_or(false, |tail| tail.starts_with(b"\\u")) {
                                    return Err(local_error(ErrorCode::LoneSurrogate, escape, "unpaired high surrogate"));
                                }
                                let low_escape = self.position;
                                self.position += 2;
                                let low = self.hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(local_error(ErrorCode::LoneSurrogate, low_escape, "expected low surrogate"));
                                }
                                0x10000 + ((first as u32 - 0xd800) << 10) + (low as u32 - 0xdc00)
                            } else if (0xdc00..=0xdfff).contains(&first) {
                                return Err(local_error(ErrorCode::LoneSurrogate, escape, "unpaired low surrogate"));
                            } else {
                                first as u32
                            };
                            core::char::from_u32(code).ok_or_else(|| {
                                local_error(ErrorCode::LoneSurrogate, escape, "invalid Unicode scalar")
                            })?
                        }
                        _ => return Err(local_error(ErrorCode::InvalidEscape, self.position - 1, "invalid string escape")),
                    };
                    let mut encoded = [0; 4];
                    self.append(&mut output, scalar.encode_utf8(&mut encoded), escape)?;
                }
                Some(_) => return Err(self.error(ErrorCode::ControlCharInString, "unescaped string control character")),
                None => return Err(self.error(ErrorCode::UnclosedString, "unclosed JSON string")),
            }
        }
    }

    fn hex_quad(&mut self) -> Result<u16> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(b'0'..=b'9') => self.bytes[self.position] - b'0',
                Some(b'a'..=b'f') => self.bytes[self.position] - b'a' + 10,
                Some(b'A'..=b'F') => self.bytes[self.position] - b'A' + 10,
                None => return Err(self.error(ErrorCode::UnexpectedEof, "incomplete Unicode escape")),
                _ => return Err(self.error(ErrorCode::InvalidEscape, "invalid Unicode hex digit")),
            };
            value = (value << 4) | digit as u16;
            self.position += 1;
        }
        Ok(value)
    }
}
