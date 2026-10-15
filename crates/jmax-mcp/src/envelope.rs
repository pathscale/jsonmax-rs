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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    fn payload(text: &str) -> Payload {
        Payload::from_slice(text.as_bytes())
    }

    fn decode(text: &str) -> McpResult<McpEnvelope> {
        decode_message(payload(text), EnvelopeLimits::default())
    }

    fn validate(text: &str) -> Result<()> {
        validate_json(&payload(text), EnvelopeLimits::default())
    }

    fn helper_error(text: &str, code: ErrorCode, offset: u32) {
        let error = validate(text).unwrap_err();
        assert_eq!(error.code, code, "input: {text:?}");
        assert_eq!(error.source, ErrorSource::Wire);
        assert_eq!(error.byte_offset, offset, "input: {text:?}");
        assert_eq!((error.schema_id, error.field_id), (0, 0));
    }

    #[test]
    fn requests_and_notifications_have_distinct_id_presence() {
        let notification = decode(r#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        assert!(notification.is_notification());
        assert_eq!(notification.id, None);
        assert_eq!(notification.method, "ping");
        assert!(notification.params.is_none());

        let request = decode(r#"{"id":null,"method":"ping","jsonrpc":"2.0"}"#).unwrap();
        assert!(!request.is_notification());
        assert_eq!(request.id, Some(McpId::Null));
    }

    #[test]
    fn exact_signed_integer_id_boundaries() {
        for number in [i64::MIN, -9007199254740993, -1, 0, 1, 9007199254740993, i64::MAX] {
            let input = format!(r#"{{"jsonrpc":"2.0","method":"ping","id":{number}}}"#);
            assert_eq!(decode(&input).unwrap().id, Some(McpId::Number(number)));
        }
    }

    #[test]
    fn string_ids_are_decoded_without_numeric_coercion() {
        for (raw, expected) in [
            (r#""""#, ""),
            (r#""42""#, "42"),
            (r#""-0""#, "-0"),
            (r#""9223372036854775808""#, "9223372036854775808"),
            (r#""a\u0000\uD83D\uDE80""#, "a\0🚀"),
        ] {
            let input = format!(r#"{{"jsonrpc":"2.0","method":"ping","id":{raw}}}"#);
            assert_eq!(decode(&input).unwrap().id, Some(McpId::String(String::from(expected))));
        }
    }

    #[test]
    fn invalid_id_kinds_and_noninteger_forms_use_null_failure_id() {
        for raw in [
            "-0", "-0.0", "0.0", "1.5", "1e0", "1E+0", "1e99999999999999999999",
            "9223372036854775808", "-9223372036854775809", "true", "false", "[]", "{}",
        ] {
            let input = format!(r#"{{"jsonrpc":"2.0","method":"ping","id":{raw}}}"#);
            let failure = decode(&input).unwrap_err();
            assert_eq!(failure.code, INVALID_REQUEST, "id: {raw}");
            assert_eq!(failure.id, McpId::Null, "id: {raw}");
        }
    }

    #[test]
    fn params_retain_exact_raw_value_in_independent_storage() {
        for raw in [r#"{ "x" : [1, {"y":"\u0061"}] }"#, "[ true , null , -0.5e+2 ]", "{}", "[]"] {
            let input = payload(&format!(" {{\"jsonrpc\":\"2.0\",\"params\":  {raw}  ,\"method\":\"tools/call\",\"id\":7}} \n"));
            let input_clone = input.clone();
            let params = decode_message(input, EnvelopeLimits::default()).unwrap().params.unwrap();
            assert_eq!(params.as_slice(), raw.as_bytes());
            let start = input_clone.as_slice().windows(raw.len())
                .position(|candidate| candidate == raw.as_bytes()).unwrap();
            assert_ne!(params.as_slice().as_ptr(), input_clone.as_slice()[start..].as_ptr());
            drop(input_clone);
            assert_eq!(params.as_slice(), raw.as_bytes());
        }
    }

    #[test]
    fn invalid_envelopes_retain_an_unambiguous_valid_id() {
        for fields in [
            r#""method":"ping""#,
            r#""jsonrpc":"1.0","method":"ping""#,
            r#""jsonrpc":2.0,"method":"ping""#,
            r#""jsonrpc":"2.0""#,
            r#""jsonrpc":"2.0","method":"""#,
            r#""jsonrpc":"2.0","method":null"#,
            r#""jsonrpc":"2.0","method":[]"#,
            r#""jsonrpc":"2.0","method":"ping","params":null"#,
            r#""jsonrpc":"2.0","method":"ping","params":17"#,
            r#""jsonrpc":"2.0","method":"ping","params":"{}""#,
            r#""jsonrpc":"2.0","method":"ping","extra":true"#,
            r#""jsonrpc":"2.0","result":{}"#,
            r#""jsonrpc":"2.0","error":{"code":-1}"#,
        ] {
            let input = format!(r#"{{{fields},"id":"request\u002d7"}}"#);
            let failure = decode(&input).unwrap_err();
            assert_eq!(failure.code, INVALID_REQUEST, "input: {input}");
            assert_eq!(failure.id, McpId::String(String::from("request-7")));
        }
    }

    #[test]
    fn valid_nonobjects_and_batches_are_invalid_requests() {
        for input in ["[]", "[{}]", "null", "true", "123", r#""hello""#] {
            let failure = decode(input).unwrap_err();
            assert_eq!(failure.code, INVALID_REQUEST);
            assert_eq!(failure.id, McpId::Null);
        }
    }

    #[test]
    fn decoded_envelope_keys_and_values_are_recognized() {
        let message = decode(r#"{"\u006asonrpc":"\u0032.0","metho\u0064":"p\u0069ng","\u0069d":"é","p\u0061rams":{}}"#).unwrap();
        assert_eq!(message.method, "ping");
        assert_eq!(message.id, Some(McpId::String(String::from("é"))));
        assert_eq!(message.params.unwrap().as_slice(), b"{}");
    }

    #[test]
    fn all_duplicate_top_level_keys_invalidate_id_trust() {
        for input in [
            r#"{"id":7,"id":7,"jsonrpc":"2.0","method":"ping"}"#,
            r#"{"id":7,"\u0069d":8,"jsonrpc":"2.0","method":"ping"}"#,
            r#"{"id":7,"jsonrpc":"2.0","method":"ping","method":"ping"}"#,
            r#"{"id":7,"jsonrpc":"2.0","method":"ping","x":0,"x":0}"#,
            r#"{"id":7,"jsonrpc":"2.0","method":"ping","params":{},"params":{}}"#,
        ] {
            let failure = decode(input).unwrap_err();
            assert_eq!(failure.code, INVALID_REQUEST);
            assert_eq!(failure.id, McpId::Null);
        }
    }

    #[test]
    fn nested_duplicate_keys_reject_envelope_without_ambiguating_root_id() {
        let failure = decode(r#"{"jsonrpc":"2.0","method":"ping","params":[{"a":1,"\u0061":2}],"id":19}"#).unwrap_err();
        assert_eq!(failure.code, INVALID_REQUEST);
        assert_eq!(failure.id, McpId::Number(19));
        helper_error(r#"{"a":1,"\u0061":2}"#, ErrorCode::DuplicateObjectKey, 7);
        helper_error(r#"[{"é":1,"\u00e9":2}]"#, ErrorCode::DuplicateObjectKey, 9);
        helper_error(r#"{"x":{"a":1,"a":2},"z":0,"z":1}"#, ErrorCode::DuplicateObjectKey, 12);
        assert!(validate(r#"[{"a":1},{"a":2}]"#).is_ok());
        assert!(validate(r#"{"a":1,"A":2,"é":3,"e\u0301":4}"#).is_ok());
    }

    #[test]
    fn malformed_json_always_uses_parse_error_and_null_id() {
        for input in [
            "", " \t\r\n", "{", "[", "[1,]", "{\"x\":1,}",
            r#"{"jsonrpc":"2.0","method":"ping","id":7"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":7} false"#,
            r#"{"id":7,"id":8,"jsonrpc":"2.0","method":"ping",}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":7,"params":{"x":"\q"}}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":01}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":1.}"#,
        ] {
            let failure = decode(input).unwrap_err();
            assert_eq!(failure.code, PARSE_ERROR, "input: {input:?}");
            assert_eq!(failure.id, McpId::Null);
        }
    }

    #[test]
    fn generic_json_accepts_all_value_kinds_and_number_grammar() {
        for input in [
            "null", "true", "false", "0", "-0", "123456789012345678901234567890",
            "-12.34e-56", "1E+20", "1e999999999999999999999", "0.0", "-0E0",
            r#""""#, "{}", "[]", " {\"unknown\":[true,false,null,{},[],\"value\"]} \t\r\n",
        ] {
            assert!(validate(input).is_ok(), "input: {input:?}");
        }
    }

    #[test]
    fn malformed_numbers_and_literals_have_precise_offsets() {
        for (input, offset) in [
            ("01", 1), ("-01", 2), ("-", 1), ("1.", 2), ("1e", 2),
            ("1E+", 3), ("1e-", 3), ("[0.e1]", 3),
        ] {
            helper_error(input, ErrorCode::InvalidNumber, offset);
        }
        helper_error("+1", ErrorCode::InvalidToken, 0);
        helper_error("NaN", ErrorCode::InvalidToken, 0);
        helper_error("Infinity", ErrorCode::InvalidToken, 0);
        helper_error("truX", ErrorCode::InvalidToken, 3);
        helper_error("nul", ErrorCode::UnexpectedEof, 3);
        helper_error("falsex", ErrorCode::TrailingContent, 5);
        helper_error("1 2", ErrorCode::TrailingContent, 2);
        helper_error("{}\u{a0}", ErrorCode::TrailingContent, 2);
        helper_error("\u{feff}null", ErrorCode::InvalidToken, 0);
    }

    #[test]
    fn syntax_and_eof_offsets_are_reported_without_echoing_input() {
        for (input, code, offset) in [
            ("", ErrorCode::UnexpectedEof, 0),
            (" \n", ErrorCode::UnexpectedEof, 2),
            ("{", ErrorCode::UnexpectedEof, 1),
            ("[", ErrorCode::UnexpectedEof, 1),
            ("[1", ErrorCode::UnexpectedEof, 2),
            ("[1,]", ErrorCode::InvalidToken, 3),
            (r#"{"a":1,}"#, ErrorCode::InvalidSyntax, 7),
            (r#"{"a" 1}"#, ErrorCode::InvalidSyntax, 5),
            (r#"{"a":1 "b":2}"#, ErrorCode::InvalidSyntax, 7),
            ("[1 2]", ErrorCode::InvalidSyntax, 3),
            ("[] {}", ErrorCode::TrailingContent, 3),
        ] {
            helper_error(input, code, offset);
        }
        let failure = decode(r#"{"jsonrpc":"2.0","id":"secret-id","method":"ping","params":"secret-params"}"#).unwrap_err();
        assert_eq!(failure.id, McpId::String(String::from("secret-id")));
        assert!(!failure.message.contains("secret"));
        assert!(failure.message.len() <= 64);
    }

    #[test]
    fn strings_decode_all_escapes_and_unicode_boundaries() {
        for (raw, expected) in [
            (r#""\"\\\/\b\f\n\r\t""#, "\"\\/\u{08}\u{0c}\n\r\t"),
            (r#""\u0000\u001f\u007f""#, "\0\u{1f}\u{7f}"),
            (r#""\uD800\uDC00""#, "\u{10000}"),
            (r#""\udbff\udfff""#, "\u{10ffff}"),
            (r#""\uD7FF\uE000\uFFFF""#, "\u{d7ff}\u{e000}\u{ffff}"),
            (r#""héllo 世界 🚀""#, "héllo 世界 🚀"),
        ] {
            assert_eq!(decode_string(&payload(raw), EnvelopeLimits::default()).unwrap(), expected);
        }
        assert_eq!(decode_string(&payload(" \t\"a\"\r\n"), EnvelopeLimits::default()).unwrap(), "a");
    }

    #[test]
    fn invalid_string_escapes_surrogates_and_control_bytes_are_rejected() {
        for (input, code, offset) in [
            ("\"abc", ErrorCode::UnclosedString, 4),
            ("\"\\", ErrorCode::UnexpectedEof, 2),
            (r#""\q""#, ErrorCode::InvalidEscape, 2),
            (r#""\u12x4""#, ErrorCode::InvalidEscape, 5),
            (r#""\u123"#, ErrorCode::UnexpectedEof, 6),
            (r#""\uDC00""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800""#, ErrorCode::LoneSurrogate, 1),
            (r#""\uD800\u0041""#, ErrorCode::LoneSurrogate, 7),
            (r#""\uD800\uD800""#, ErrorCode::LoneSurrogate, 7),
            (r#""\uD800x""#, ErrorCode::LoneSurrogate, 1),
        ] {
            helper_error(input, code, offset);
        }
        for byte in 0..0x20 {
            let input = Payload::from_vec(vec![b'"', byte, b'"']);
            let error = validate_json(&input, EnvelopeLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::ControlCharInString);
            assert_eq!(error.byte_offset, 1);
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_at_the_first_invalid_byte() {
        for bytes in [
            &b"\"\xff\""[..], &b"\"\xc0\xaf\""[..], &b"\"\xed\xa0\x80\""[..],
            &b"\"\xf4\x90\x80\x80\""[..], &b"\"\x80\""[..], &b"\"\xe2\x82"[..],
        ] {
            let input = Payload::from_slice(bytes);
            let error = validate_json(&input, EnvelopeLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidUtf8);
            assert_eq!(error.byte_offset, 1);
            let failure = decode_message(input, EnvelopeLimits::default()).unwrap_err();
            assert_eq!(failure.code, PARSE_ERROR);
            assert_eq!(failure.id, McpId::Null);
        }
    }

    #[test]
    fn object_field_validates_the_whole_object_and_matches_decoded_keys() {
        let input = payload(r#" {"before":false,"t\u0061rget":  [1, {"x":"a\\b"}]  ,"after":null} "#);
        let field = object_field(&input, "target", EnvelopeLimits::default()).unwrap().unwrap();
        assert_eq!(field.as_slice(), br#"[1, {"x":"a\\b"}]"#);
        assert!(object_field(&input, "TARGET", EnvelopeLimits::default()).unwrap().is_none());
        drop(input);
        assert!(validate_json(&field, EnvelopeLimits::default()).is_ok());
        let empty_key = object_field(&payload(r#"{"":null}"#), "", EnvelopeLimits::default()).unwrap().unwrap();
        assert_eq!(empty_key.as_slice(), b"null");
        for input in [r#"{"target":1,"other":{"x":1,"x":2}}"#, r#"{"target":1,"target":2}"#] {
            assert_eq!(object_field(&payload(input), "target", EnvelopeLimits::default()).unwrap_err().code, ErrorCode::DuplicateObjectKey);
        }
        assert_eq!(object_field(&payload(r#"{"target":1} x"#), "target", EnvelopeLimits::default()).unwrap_err().code, ErrorCode::TrailingContent);
        assert_eq!(object_field(&payload(r#"{"other":0,"other":1}"#), "absent", EnvelopeLimits::default()).unwrap_err().code, ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn typed_helpers_validate_syntax_before_reporting_wrong_kind() {
        for input in ["[]", "null", "true", "0", r#""x""#] {
            let error = object_field(&payload(&format!("  {input}")), "x", EnvelopeLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::TypeMismatch);
            assert_eq!(error.byte_offset, 2);
        }
        for input in ["{}", "[]", "null", "true", "0"] {
            let error = decode_string(&payload(&format!("  {input}")), EnvelopeLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::TypeMismatch);
            assert_eq!(error.byte_offset, 2);
        }
        assert_eq!(decode_string(&payload("0 trailing"), EnvelopeLimits::default()).unwrap_err().code, ErrorCode::TrailingContent);
        assert_eq!(object_field(&payload("[1,]"), "x", EnvelopeLimits::default()).unwrap_err().code, ErrorCode::InvalidToken);
    }

    #[test]
    fn object_field_preserves_every_scalar_lexeme_and_string_escape() {
        for raw in ["null", "false", "true", "-0", "1.00e+03", r#""a\u0062\\c""#] {
            let input = payload(&format!(r#"{{"value":  {raw}  }}"#));
            let value = object_field(&input, "value", EnvelopeLimits::default()).unwrap().unwrap();
            assert_eq!(value.as_slice(), raw.as_bytes());
        }
        assert!(object_field(&payload("{}"), "absent", EnvelopeLimits::default()).unwrap().is_none());
    }

    #[test]
    fn duplicate_diagnostics_choose_the_first_repeated_key_in_source_order() {
        helper_error(r#"{"z":0,"a":0,"z":1,"a":1,"z":2}"#, ErrorCode::DuplicateObjectKey, 13);
        helper_error(r#"{"a":0,"a":{"z":0,"z":1}}"#, ErrorCode::DuplicateObjectKey, 7);
        let error = decode_string(&payload(r#"{"a":0,"a":1}"#), EnvelopeLimits::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn raw_byte_limit_is_inclusive_and_covers_whitespace() {
        let input = payload(" null ");
        let mut limits = EnvelopeLimits { max_payload_bytes: 6, ..EnvelopeLimits::default() };
        assert!(validate_json(&input, limits).is_ok());
        limits.max_payload_bytes = 5;
        let error = validate_json(&input, limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(decode_message(input, limits).unwrap_err().code, INTERNAL_ERROR);
    }

    #[test]
    fn decoded_budget_counts_keys_and_all_string_values_in_utf8_bytes() {
        let input = payload(r#"{"a":"é","b":["\uD83D\uDE80",{"c":""}]}"#);
        let mut limits = EnvelopeLimits { max_decoded_bytes: 9, ..EnvelopeLimits::default() };
        assert!(validate_json(&input, limits).is_ok());
        limits.max_decoded_bytes = 8;
        assert_eq!(validate_json(&input, limits).unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert_eq!(decode_message(input, limits).unwrap_err().code, INTERNAL_ERROR);
        limits.max_decoded_bytes = 4;
        assert_eq!(decode_string(&payload(r#""\uD83D\uDE80""#), limits).unwrap(), "🚀");
        limits.max_decoded_bytes = 3;
        assert_eq!(decode_string(&payload(r#""\uD83D\uDE80""#), limits).unwrap_err().code, ErrorCode::PayloadTooLarge);
        limits.max_decoded_bytes = 1;
        assert!(validate_json(&payload(r#"{"":""}"#), limits).is_ok());
    }

    #[test]
    fn nesting_limit_counts_containers_and_is_inclusive() {
        let limits = EnvelopeLimits { max_depth: 1, ..EnvelopeLimits::default() };
        for input in ["[]", "{}", "[0]", r#"{"a":null}"#, r#""s""#] {
            assert!(validate_json(&payload(input), limits).is_ok());
        }
        for (input, offset) in [("[[]]", 1), (r#"{"a":{}}"#, 5), ("[{\"x\":0}]", 1)] {
            let error = validate_json(&payload(input), limits).unwrap_err();
            assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
            assert_eq!(error.byte_offset, offset);
        }
        let limits = EnvelopeLimits { max_depth: 3, ..EnvelopeLimits::default() };
        assert!(validate_json(&payload("[{\"x\":[]}]"), limits).is_ok());
        assert_eq!(validate_json(&payload("[{\"x\":[{}]}]"), limits).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        assert_eq!(decode_message(payload("[[[[]]]]"), limits).unwrap_err().code, INTERNAL_ERROR);
    }

    #[test]
    fn deep_input_does_not_use_recursive_traversal_or_drop() {
        let depth = 4096;
        let input = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let limits = EnvelopeLimits { max_depth: depth as u16, ..EnvelopeLimits::default() };
        assert!(validate_json(&payload(&input), limits).is_ok());
        let limits = EnvelopeLimits { max_depth: depth as u16 - 1, ..limits };
        assert_eq!(validate_json(&payload(&input), limits).unwrap_err().code, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn field_limit_is_per_object_and_arrays_have_no_field_count() {
        let limits = EnvelopeLimits { max_fields: 1, ..EnvelopeLimits::default() };
        assert!(validate_json(&payload(r#"{"a":{"b":0}}"#), limits).is_ok());
        assert!(validate_json(&payload(r#"[{"a":0},{"a":1},0,1,2,3]"#), limits).is_ok());
        let error = validate_json(&payload(r#"{"a":0,"b":1}"#), limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.byte_offset, 7);
        assert_eq!(decode_message(payload(r#"{"jsonrpc":"2.0","method":"ping"}"#), limits).unwrap_err().code, INTERNAL_ERROR);
    }

    #[test]
    fn zero_limits_are_invalid_configuration_for_every_helper() {
        let baseline = EnvelopeLimits::default();
        for limits in [
            EnvelopeLimits { max_payload_bytes: 0, ..baseline },
            EnvelopeLimits { max_decoded_bytes: 0, ..baseline },
            EnvelopeLimits { max_depth: 0, ..baseline },
            EnvelopeLimits { max_fields: 0, ..baseline },
        ] {
            assert_eq!(validate_json(&payload("null"), limits).unwrap_err().code, ErrorCode::ConstraintViolation);
            assert_eq!(object_field(&payload("{}"), "x", limits).unwrap_err().code, ErrorCode::ConstraintViolation);
            assert_eq!(decode_string(&payload(r#""x""#), limits).unwrap_err().code, ErrorCode::ConstraintViolation);
            let failure = decode_message(payload(r#"{"jsonrpc":"2.0","method":"ping"}"#), limits).unwrap_err();
            assert_eq!(failure.code, INTERNAL_ERROR);
            assert_eq!(failure.id, McpId::Null);
        }
    }
}
