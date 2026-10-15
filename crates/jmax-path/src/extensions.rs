//! Bounded, nonrecursive wildcard and scalar-equality path queries.
//!
//! Traversal keeps one prefix state per value and one frame per open container.
//! Filter decisions can arrive after descendant matches: each filtered object
//! owns a checkpoint into the provisional result list, which is rolled back if
//! its first predicate member does not match. Nested checkpoints compose without
//! a DOM, secondary scans, or Cartesian expansion.

use super::*;
use jmax_cursor::{Cursor, Event, ScalarKind, ScanStep};

/// Validate and own a bounded extension program, including strict number text.
pub fn compile(segments: &[ExtensionSegment], limits: PathLimits) -> Result<ExtensionPath> {
    validate(segments, limits)?;
    let mut owned = Vec::new();
    reserve(&mut owned, segments.len(), 0)?;
    for segment in segments {
        owned.push(match segment {
            ExtensionSegment::Exact(Segment::Key(key)) => {
                ExtensionSegment::Exact(Segment::Key(copy_text(key)?))
            }
            ExtensionSegment::Exact(Segment::Index(index)) => {
                ExtensionSegment::Exact(Segment::Index(*index))
            }
            ExtensionSegment::Wildcard => ExtensionSegment::Wildcard,
            ExtensionSegment::FilterEq { key, value } => ExtensionSegment::FilterEq {
                key: copy_text(key)?,
                value: match value {
                    FilterValue::Null => FilterValue::Null,
                    FilterValue::Bool(value) => FilterValue::Bool(*value),
                    FilterValue::String(value) => FilterValue::String(copy_text(value)?),
                    FilterValue::Number(value) => FilterValue::Number(copy_text(value)?),
                },
            },
        });
    }
    Ok(ExtensionPath { segments: owned, limits })
}

fn validate(segments: &[ExtensionSegment], limits: PathLimits) -> Result<()> {
    if segments.len() > limits.max_segments as usize {
        return Err(budget(0, "extension segment limit exceeded"));
    }
    for segment in segments {
        let key = match segment {
            ExtensionSegment::Exact(Segment::Key(key)) => Some(key),
            ExtensionSegment::FilterEq { key, value } => {
                match value {
                    FilterValue::String(text) if text.len() > limits.max_key_bytes as usize => {
                        return Err(budget(0, "filter string byte limit exceeded"));
                    }
                    FilterValue::Number(text) => {
                        if text.len() > limits.max_path_bytes as usize {
                            return Err(budget(0, "filter number byte limit exceeded"));
                        }
                        if number_end(text.as_bytes(), 0) != Ok(text.len()) {
                            return Err(path_error(ErrorCode::InvalidNumber, 0,
                                "filter number must be a complete strict JSON number"));
                        }
                    }
                    _ => {}
                }
                Some(key)
            }
            _ => None,
        };
        if key.is_some_and(|key| key.len() > limits.max_key_bytes as usize) {
            return Err(budget(0, "extension key byte limit exceeded"));
        }
    }
    Ok(())
}

fn copy_text(text: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(text.len())
        .map_err(|_| budget(0, "extension string allocation failed"))?;
    owned.push_str(text);
    Ok(owned)
}

fn budget(at: u32, detail: &'static str) -> Error {
    path_error(ErrorCode::BudgetExhausted, at, detail)
}

fn internal(at: u32, detail: &'static str) -> Error {
    path_error(ErrorCode::InternalError, at, detail)
}

fn reserve<T>(values: &mut Vec<T>, additional: usize, at: u32) -> Result<()> {
    values.try_reserve_exact(additional)
        .map_err(|_| budget(at, "extension storage allocation failed"))
}

fn push<T>(values: &mut Vec<T>, value: T, at: u32) -> Result<()> {
    // Fallible amortized growth; logical sizes are bounded by segments, depth,
    // or the shared visit budget, rather than allocator capacity.
    values.try_reserve(1).map_err(|_| budget(at, "extension storage allocation failed"))?;
    values.push(value);
    Ok(())
}

/// Parse exact selectors, immediate wildcards, and `[?(@.KEY==LITERAL)]`.
/// Strings use JSON escapes. Numbers retain their exact lexical spelling.
pub fn parse(input: &str, limits: PathLimits) -> Result<ExtensionPath> {
    let length = u32::try_from(input.len()).map_err(|_| path_error(
        ErrorCode::PayloadTooLarge, 0, "extension path exceeds the offset range"))?;
    if length > limits.max_path_bytes {
        return Err(budget(limits.max_path_bytes, "extension path byte limit exceeded"));
    }
    let mut parser = Parser { input, position: 0, limits };
    let mut segments = Vec::new();
    if parser.peek() == Some(b'$') {
        parser.position += 1;
        if parser.peek().is_some() && !matches!(parser.peek(), Some(b'.' | b'[')) {
            return Err(parser.error(ErrorCode::InvalidSyntax, parser.position,
                "expected dot or bracket after root"));
        }
    } else if parser.peek() == Some(b'.') {
        return Err(if input.starts_with("..") { parser.unsupported(0) } else {
            parser.error(ErrorCode::InvalidSyntax, 0, "leading dot requires root marker")
        });
    }
    let mut first = true;
    while parser.peek().is_some() {
        match parser.peek() {
            Some(b'.') => {
                if parser.input.as_bytes().get(parser.position + 1) == Some(&b'.') {
                    return Err(parser.unsupported(parser.position));
                }
                parser.position += 1;
                if parser.peek().is_none() { return Err(parser.eof()); }
                if parser.peek() == Some(b'[') {
                    return Err(parser.error(ErrorCode::InvalidSyntax, parser.position,
                        "dot requires a nonempty bare component"));
                }
            }
            Some(b'[') => {}
            _ if first => {}
            _ => return Err(parser.error(ErrorCode::InvalidSyntax, parser.position,
                "expected dot or bracket after selector")),
        }
        if segments.len() >= limits.max_segments as usize {
            return Err(budget(parser.position as u32, "extension segment limit exceeded"));
        }
        let segment = match parser.peek() {
            Some(b'[') => parser.bracket()?,
            Some(b'*') => {
                let at = parser.position;
                parser.position += 1;
                if !matches!(parser.peek(), None | Some(b'.' | b'[')) {
                    return Err(parser.error(ErrorCode::InvalidSyntax, at,
                        "wildcard must occupy a complete component"));
                }
                ExtensionSegment::Wildcard
            }
            _ => ExtensionSegment::Exact(Segment::Key(parser.bare(false)?)),
        };
        push(&mut segments, segment, parser.position as u32)?;
        first = false;
    }
    validate(&segments, limits)?;
    Ok(ExtensionPath { segments, limits })
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
    limits: PathLimits,
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn error(&self, code: ErrorCode, at: usize, detail: &'static str) -> Error {
        path_error(code, at as u32, detail)
    }

    fn eof(&self) -> Error {
        self.error(ErrorCode::UnexpectedEof, self.input.len(), "incomplete extension path")
    }

    fn unsupported(&self, at: usize) -> Error {
        self.error(ErrorCode::UnsupportedSchemaFeature, at,
            "selector construct is outside the bounded extension subset")
    }

    fn expect(&mut self, expected: u8, extension: bool) -> Result<()> {
        match self.peek() {
            Some(byte) if byte == expected => { self.position += 1; Ok(()) }
            None => Err(self.eof()),
            Some(byte) if extension && !whitespace(byte) && !matches!(byte, b')' | b']') => {
                Err(self.unsupported(self.position))
            }
            _ => Err(self.error(ErrorCode::InvalidSyntax, self.position,
                "unexpected extension path punctuation")),
        }
    }

    fn space(&mut self) {
        while self.peek().is_some_and(whitespace) { self.position += 1; }
    }

    fn append(&self, value: &mut String, character: char, at: usize) -> Result<()> {
        let maximum = self.limits.max_key_bytes as usize;
        let width = character.len_utf8();
        if width > maximum.saturating_sub(value.len()) {
            return Err(budget(at as u32, "decoded extension string byte limit exceeded"));
        }
        if value.len() + width > value.capacity() {
            let target = value.capacity().saturating_mul(2).max(16)
                .max(value.len() + width).min(maximum);
            value.try_reserve_exact(target - value.len())
                .map_err(|_| budget(at as u32, "extension string allocation failed"))?;
        }
        value.push(character);
        Ok(())
    }

    fn bare(&mut self, filter: bool) -> Result<String> {
        let start = self.position;
        let mut key = String::new();
        let mut operator = None;
        while let Some(character) = self.input[self.position..].chars().next() {
            if filter && (self.input[self.position..].starts_with("==")
                || (self.peek() == Some(b'=') && self.position + 1 == self.input.len())
                || self.peek().is_some_and(whitespace)) { break; }
            if character == '.' || character == '[' {
                if filter { return Err(self.unsupported(self.position)); }
                break;
            }
            let at = self.position;
            self.position += character.len_utf8();
            if character == '\\' {
                let escaped = self.input[self.position..].chars().next().ok_or_else(|| self.eof())?;
                if escaped.is_control() {
                    return Err(self.error(ErrorCode::InvalidSyntax, self.position,
                        "control character in bare key"));
                }
                self.position += escaped.len_utf8();
                self.append(&mut key, escaped, at)?;
                continue;
            }
            // These characters remain legal in keys when followed eventually
            // by ==. Without equality they identify an unsupported operator.
            if filter && matches!(character, '!' | '<' | '>' | '=' | '|' | '&')
                && operator.is_none() { operator = Some(at); }
            if matches!(character, ']' | '$' | '*' | '?')
                || character.is_whitespace() || character.is_control() {
                if let Some(at) = operator { return Err(self.unsupported(at)); }
                return Err(self.error(ErrorCode::InvalidSyntax, at,
                    "bare key character requires escaping"));
            }
            self.append(&mut key, character, at)?;
        }
        if key.is_empty() {
            return Err(if self.peek().is_none() { self.eof() } else {
                self.error(ErrorCode::InvalidSyntax, start, "expected nonempty bare key")
            });
        }
        if filter && self.peek().is_none() {
            if let Some(at) = operator { return Err(self.unsupported(at)); }
        }
        Ok(key)
    }

    fn bracket(&mut self) -> Result<ExtensionSegment> {
        self.position += 1;
        match self.peek() {
            None => Err(self.eof()),
            Some(b'*') => {
                self.position += 1;
                if matches!(self.peek(), Some(b',' | b':')) {
                    return Err(self.unsupported(self.position));
                }
                self.expect(b']', false)?;
                Ok(ExtensionSegment::Wildcard)
            }
            Some(b'?') => self.filter(),
            Some(b'"') => {
                let key = self.quoted()?;
                if matches!(self.peek(), Some(b',' | b':')) {
                    return Err(self.unsupported(self.position));
                }
                self.expect(b']', false)?;
                Ok(ExtensionSegment::Exact(Segment::Key(key)))
            }
            Some(b'0'..=b'9') => {
                let start = self.position;
                let mut index = 0u32;
                while let Some(digit @ b'0'..=b'9') = self.peek() {
                    if self.position > start && self.input.as_bytes()[start] == b'0' {
                        return Err(self.error(ErrorCode::InvalidSyntax, self.position,
                            "array index has a leading zero"));
                    }
                    index = index.checked_mul(10)
                        .and_then(|index| index.checked_add(u32::from(digit - b'0')))
                        .ok_or_else(|| self.error(ErrorCode::NumberOverflow, self.position,
                            "array index exceeds u32"))?;
                    self.position += 1;
                }
                if matches!(self.peek(), Some(b',' | b':')) {
                    return Err(self.unsupported(self.position));
                }
                self.expect(b']', false)?;
                Ok(ExtensionSegment::Exact(Segment::Index(index)))
            }
            Some(b'-' | b'+' | b',' | b':') => Err(self.unsupported(self.position)),
            _ => Err(self.error(ErrorCode::InvalidSyntax, self.position,
                "expected index, quoted key, wildcard, or equality filter")),
        }
    }

    fn filter(&mut self) -> Result<ExtensionSegment> {
        self.position += 1;
        self.expect(b'(', true)?;
        self.expect(b'@', true)?;
        self.expect(b'.', true)?;
        let key = self.bare(true)?;
        self.space();
        self.expect(b'=', true)?;
        self.expect(b'=', true)?;
        self.space();
        // Diagnose an identifier followed by a call before keyword parsing, so
        // function names starting with n/t/f are not mistaken for bad literals.
        let mut name_end = self.position;
        while self.input.as_bytes().get(name_end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
            name_end += 1;
        }
        if name_end > self.position
            && self.input.as_bytes().get(self.position).is_some_and(u8::is_ascii_alphabetic) {
            let mut call = name_end;
            while self.input.as_bytes().get(call).copied().is_some_and(whitespace) { call += 1; }
            if self.input.as_bytes().get(call) == Some(&b'(') {
                return Err(self.unsupported(self.position));
            }
        }
        let value = match self.peek() {
            None => return Err(self.eof()),
            Some(b'"') => FilterValue::String(self.quoted()?),
            Some(b'n') => { self.keyword("null")?; FilterValue::Null }
            Some(b't') => { self.keyword("true")?; FilterValue::Bool(true) }
            Some(b'f') => { self.keyword("false")?; FilterValue::Bool(false) }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.position;
                self.position = number_end(self.input.as_bytes(), start).map_err(|at| {
                    if at == self.input.len() { self.eof() } else {
                        self.error(ErrorCode::InvalidNumber, at, "invalid filter number")
                    }
                })?;
                FilterValue::Number(copy_text(&self.input[start..self.position])
                    .map_err(|_| budget(start as u32, "filter number allocation failed"))?)
            }
            Some(b'[' | b'{' | b'@' | b'/' | b'+' | b'!' | b'(' | b'=' | b'<' | b'>') => {
                return Err(self.unsupported(self.position));
            }
            _ => return Err(self.error(ErrorCode::InvalidSyntax, self.position,
                "expected JSON scalar filter literal")),
        };
        self.space();
        // Additional operators, expressions, or a nested selector are outside
        // the subset. Missing/misordered closing delimiters are ordinary errors.
        if matches!(self.peek(), Some(b'=' | b'!' | b'<' | b'>' | b'&' | b'|'
            | b'+' | b'-' | b'*' | b'/' | b'.' | b'[' | b'(' | b',' | b':')) {
            return Err(self.unsupported(self.position));
        }
        self.expect(b')', false)?;
        self.expect(b']', false)?;
        Ok(ExtensionSegment::FilterEq { key, value })
    }

    fn keyword(&mut self, expected: &str) -> Result<()> {
        for byte in expected.bytes() { self.expect(byte, false)?; }
        Ok(())
    }

    fn quoted(&mut self) -> Result<String> {
        self.expect(b'"', false)?;
        let mut value = String::new();
        loop {
            let at = self.position;
            let character = self.input[self.position..].chars().next().ok_or_else(|| self.eof())?;
            self.position += character.len_utf8();
            match character {
                '"' => return Ok(value),
                '\\' => {
                    let decoded = self.escape(at)?;
                    self.append(&mut value, decoded, at)?;
                }
                '\u{0}'..='\u{1f}' => return Err(self.error(ErrorCode::ControlCharInString,
                    at, "unescaped control in extension string")),
                _ => self.append(&mut value, character, at)?,
            }
        }
    }

    fn escape(&mut self, at: usize) -> Result<char> {
        let byte = self.peek().ok_or_else(|| self.eof())?;
        let decoded = match byte {
            b'"' => '"', b'\\' => '\\', b'/' => '/', b'b' => '\u{8}',
            b'f' => '\u{c}', b'n' => '\n', b'r' => '\r', b't' => '\t',
            b'u' => {
                self.position += 1;
                let first = self.hex()?;
                let scalar = match first {
                    0xd800..=0xdbff => {
                        if self.peek().is_none() { return Err(self.eof()); }
                        if self.peek() != Some(b'\\') {
                            return Err(self.error(ErrorCode::LoneSurrogate, at,
                                "high surrogate requires a low surrogate escape"));
                        }
                        let second_at = self.position;
                        self.position += 1;
                        match self.peek() {
                            None => return Err(self.eof()),
                            Some(b'u') => self.position += 1,
                            _ => return Err(self.error(ErrorCode::LoneSurrogate, at,
                                "high surrogate requires a low surrogate escape")),
                        }
                        let second = self.hex()?;
                        if !(0xdc00..=0xdfff).contains(&second) {
                            return Err(self.error(ErrorCode::LoneSurrogate, second_at,
                                "expected low surrogate"));
                        }
                        0x10000 + ((u32::from(first) - 0xd800) << 10)
                            + (u32::from(second) - 0xdc00)
                    }
                    0xdc00..=0xdfff => return Err(self.error(ErrorCode::LoneSurrogate, at,
                        "low surrogate without a high surrogate")),
                    _ => u32::from(first),
                };
                return char::from_u32(scalar).ok_or_else(|| self.error(
                    ErrorCode::InvalidEscape, at, "invalid Unicode scalar"));
            }
            _ => return Err(self.error(ErrorCode::InvalidEscape, self.position,
                "invalid JSON string escape")),
        };
        self.position += 1;
        Ok(decoded)
    }

    fn hex(&mut self) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.peek().ok_or_else(|| self.eof())? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error(ErrorCode::InvalidEscape, self.position,
                    "expected hexadecimal escape digit")),
            };
            value = (value << 4) | u16::from(digit);
            self.position += 1;
        }
        Ok(value)
    }
}

/// Return the first byte after a JSON number, or the first missing/invalid digit.
/// Callers additionally require the appropriate token boundary or end of text.
fn number_end(bytes: &[u8], start: usize) -> core::result::Result<usize, usize> {
    let mut at = start;
    if bytes.get(at) == Some(&b'-') { at += 1; }
    match bytes.get(at) {
        Some(b'0') => {
            at += 1;
            if matches!(bytes.get(at), Some(b'0'..=b'9')) { return Err(at); }
        }
        Some(b'1'..=b'9') => {
            at += 1;
            while matches!(bytes.get(at), Some(b'0'..=b'9')) { at += 1; }
        }
        _ => return Err(at),
    }
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        if !matches!(bytes.get(at), Some(b'0'..=b'9')) { return Err(at); }
        while matches!(bytes.get(at), Some(b'0'..=b'9')) { at += 1; }
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) { at += 1; }
        if !matches!(bytes.get(at), Some(b'0'..=b'9')) { return Err(at); }
        while matches!(bytes.get(at), Some(b'0'..=b'9')) { at += 1; }
    }
    Ok(at)
}

#[derive(Debug)]
struct Frame {
    kind: ValueKind,
    start: u32,
    prefix: bool,
    pending_key: Option<StrRef>,
    next_index: u64,
    filter: Option<usize>,
    predicate_seen: bool,
    predicate_pending: bool,
    predicate_matches: bool,
    checkpoint: usize,
}

struct Visits { used: u32, maximum: u32 }

impl Visits {
    fn take(&mut self, at: u32) -> Result<()> {
        if self.used == self.maximum {
            return Err(budget(at, "extension event and candidate visit limit exceeded"));
        }
        self.used += 1;
        Ok(())
    }
}

/// Return distinct matches in starting-offset order, retaining their owners.
///
/// Each consumed event and each matching prefix candidate costs one visit. A
/// terminal's eventual result reuses that candidate, including container ends.
/// Provisional results count against visits until all enclosing filters decide;
/// only surviving results count against `max_matches`. Strict traversal reaches
/// `Finished`; query traversal stops at the completed root and conservatively
/// reports `fully_validated == false`.
pub fn query(
    payload: Payload,
    path: &ExtensionPath,
    mode: SkipMode,
    limits: Limits,
) -> Result<QueryResults> {
    let mut cursor = jmax_cursor::walk::from_payload(payload.clone(), limits, mode)?;
    let mut visits = Visits { used: 0, maximum: path.limits.max_visits };
    let mut frames: Vec<Frame> = Vec::new();
    let mut matches = Vec::new();
    let mut root_started = false;
    let mut root_complete = false;
    let fully_validated;
    loop {
        if root_complete && mode == SkipMode::Query {
            fully_validated = false;
            break;
        }
        // Once the root closes, next can only finish or diagnose trailing
        // content. It consumes no further events, so EOF itself costs no visit.
        if !root_complete { visits.take(cursor.state().byte_offset)?; }
        let event = match jmax_cursor::walk::next(&mut cursor)? {
            ScanStep::Emit(event) if !root_complete => event,
            ScanStep::Emit(_) => return Err(internal(cursor.state().byte_offset,
                "cursor emitted a second root value")),
            ScanStep::Finished if root_complete && frames.is_empty() => {
                fully_validated = true;
                break;
            }
            ScanStep::Finished => return Err(internal(cursor.state().byte_offset,
                "cursor finished before extension root completion")),
            ScanStep::NeedMore => return Err(path_error(ErrorCode::UnexpectedEof,
                cursor.state().byte_offset, "extension query requires complete input")),
        };
        let at = cursor.state().byte_offset;
        match event {
            Event::Key { str_ref } => {
                let key = cursor.resolve_str(str_ref)
                    .ok_or_else(|| internal(at, "invalid cursor key reference"))?;
                let frame = frames.last_mut()
                    .ok_or_else(|| internal(at, "key has no containing extension frame"))?;
                if frame.kind != ValueKind::Object || frame.pending_key.is_some() {
                    return Err(internal(at, "key is out of extension traversal order"));
                }
                frame.pending_key = Some(str_ref);
                if let Some(index) = frame.filter {
                    if let ExtensionSegment::FilterEq { key: expected, .. } = &path.segments[index] {
                        if !frame.predicate_seen && key.as_bytes() == expected.as_bytes() {
                            frame.predicate_seen = true;
                            frame.predicate_pending = true;
                        }
                    }
                }
            }
            Event::ObjectEnd { offset } | Event::ArrayEnd { offset } => {
                let frame = frames.pop()
                    .ok_or_else(|| internal(at, "container end has no extension frame"))?;
                if !matches!((frame.kind, event),
                    (ValueKind::Object, Event::ObjectEnd { .. })
                    | (ValueKind::Array, Event::ArrayEnd { .. }))
                    || frame.pending_key.is_some() || frame.predicate_pending {
                    return Err(internal(at, "container end disagrees with extension frame"));
                }
                let accepted = frame.filter.is_none() || frame.predicate_matches;
                if !accepted { matches.truncate(frame.checkpoint); }
                if accepted && frame.prefix && frames.len() == path.segments.len() {
                    let end = offset.checked_add(1)
                        .ok_or_else(|| internal(at, "container end exceeds span range"))?;
                    let len = end.checked_sub(frame.start).filter(|len| *len >= 2)
                        .ok_or_else(|| internal(at, "invalid container delimiter order"))?;
                    let span = Span32::new(frame.start, len);
                    if span.slice(payload.as_slice()).is_none() {
                        return Err(internal(at, "container span exceeds payload"));
                    }
                    push(&mut matches, SelectedValue { kind: frame.kind, span, decoded: None }, at)?;
                }
                if frames.is_empty() { root_complete = true; }
            }
            Event::ObjectStart { .. } | Event::ArrayStart { .. } | Event::Scalar { .. } => {
                let depth = frames.len();
                let kind = event_kind(event, at)?;
                let prefix = if let Some(parent) = frames.last_mut() {
                    if parent.predicate_pending {
                        let index = parent.filter.ok_or_else(|| internal(at,
                            "pending predicate has no filter"))?;
                        if let ExtensionSegment::FilterEq { value, .. } = &path.segments[index] {
                            parent.predicate_matches = scalar_equals(&cursor, event, value, at)?;
                        }
                        parent.predicate_pending = false;
                    }
                    let key = parent.pending_key.take();
                    if parent.kind == ValueKind::Object && key.is_none() {
                        return Err(internal(at, "object value has no key"));
                    }
                    let index = parent.next_index;
                    if parent.kind == ValueKind::Array { parent.next_index += 1; }
                    parent.prefix && match path.segments.get(depth - 1) {
                        Some(ExtensionSegment::Wildcard) => true,
                        Some(ExtensionSegment::Exact(Segment::Index(expected))) => {
                            parent.kind == ValueKind::Array && index == u64::from(*expected)
                        }
                        Some(ExtensionSegment::Exact(Segment::Key(expected))) => {
                            parent.kind == ValueKind::Object && key
                                .and_then(|key| cursor.resolve_str(key))
                                .is_some_and(|key| key.as_bytes() == expected.as_bytes())
                        }
                        Some(ExtensionSegment::FilterEq { .. }) => {
                            parent.kind == ValueKind::Array && kind == ValueKind::Object
                        }
                        None => false,
                    }
                } else {
                    if root_started { return Err(internal(at, "multiple extension roots")); }
                    root_started = true;
                    true
                };
                if prefix { visits.take(at)?; }
                match event {
                    Event::Scalar { span, decoded, .. } => {
                        if span.is_empty() || span.slice(payload.as_slice()).is_none() {
                            return Err(internal(at, "invalid cursor scalar span"));
                        }
                        if kind == ValueKind::String
                            && decoded.and_then(|reference| cursor.resolve_str(reference)).is_none() {
                            return Err(internal(at, "invalid cursor string reference"));
                        }
                        if prefix && depth == path.segments.len() {
                            push(&mut matches, SelectedValue { kind, span, decoded }, at)?;
                        }
                        if frames.is_empty() { root_complete = true; }
                    }
                    Event::ObjectStart { offset } | Event::ArrayStart { offset } => {
                        let filter = if prefix && depth > 0 && matches!(
                            path.segments.get(depth - 1), Some(ExtensionSegment::FilterEq { .. })) {
                            Some(depth - 1)
                        } else { None };
                        push(&mut frames, Frame {
                            kind, start: offset, prefix, pending_key: None, next_index: 0,
                            filter, predicate_seen: false, predicate_pending: false,
                            predicate_matches: false, checkpoint: matches.len(),
                        }, at)?;
                    }
                    _ => return Err(internal(at, "unexpected extension value event")),
                }
            }
        }
    }
    matches.sort_unstable_by_key(|value| value.span.offset);
    matches.dedup_by_key(|value| value.span.offset);
    if matches.len() > path.limits.max_matches as usize {
        return Err(budget(cursor.state().byte_offset, "extension result count limit exceeded"));
    }
    let mut decoded = Vec::new();
    reserve(&mut decoded, cursor.decoded_bytes().len(), cursor.state().byte_offset)?;
    decoded.extend_from_slice(cursor.decoded_bytes());
    Ok(QueryResults { payload, decoded, matches, fully_validated })
}

fn event_kind(event: Event, at: u32) -> Result<ValueKind> {
    Ok(match event {
        Event::ObjectStart { .. } => ValueKind::Object,
        Event::ArrayStart { .. } => ValueKind::Array,
        Event::Scalar { kind: ScalarKind::String, .. } => ValueKind::String,
        Event::Scalar { kind: ScalarKind::Number, .. } => ValueKind::Number,
        Event::Scalar { kind: ScalarKind::True | ScalarKind::False, .. } => ValueKind::Bool,
        Event::Scalar { kind: ScalarKind::Null, .. } => ValueKind::Null,
        Event::Key { .. } | Event::ObjectEnd { .. } | Event::ArrayEnd { .. } => {
            return Err(internal(at, "expected a value start or scalar event"));
        }
    })
}

fn scalar_equals(cursor: &Cursor, event: Event, expected: &FilterValue, at: u32) -> Result<bool> {
    Ok(match (event, expected) {
        (Event::Scalar { kind: ScalarKind::Null, .. }, FilterValue::Null) => true,
        (Event::Scalar { kind: ScalarKind::True, .. }, FilterValue::Bool(true))
        | (Event::Scalar { kind: ScalarKind::False, .. }, FilterValue::Bool(false)) => true,
        (Event::Scalar { kind: ScalarKind::Number, span, .. }, FilterValue::Number(text)) => {
            span.slice(cursor.raw_bytes()).ok_or_else(|| internal(at,
                "invalid predicate number span"))? == text.as_bytes()
        }
        (Event::Scalar { kind: ScalarKind::String, decoded, .. }, FilterValue::String(text)) => {
            let reference = decoded.ok_or_else(|| internal(at, "predicate string lacks reference"))?;
            cursor.resolve_str(reference).ok_or_else(|| internal(at,
                "invalid predicate string reference"))?.as_bytes() == text.as_bytes()
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    fn exact(key: &str) -> ExtensionSegment {
        ExtensionSegment::Exact(Segment::Key(String::from(key)))
    }

    fn filter(key: &str, value: FilterValue) -> ExtensionSegment {
        ExtensionSegment::FilterEq { key: String::from(key), value }
    }

    fn run(input: &[u8], text: &str) -> QueryResults {
        let path = parse(text, PathLimits::default()).unwrap();
        query(Payload::from_slice(input), &path, SkipMode::Strict,
            Limits::strict_default()).unwrap()
    }

    fn assert_raw(results: &QueryResults, expected: &[&[u8]]) {
        assert_eq!(results.matches().len(), expected.len());
        let mut previous = None;
        for (index, bytes) in expected.iter().enumerate() {
            let value = results.matches()[index];
            assert_eq!(results.raw(index), Some(*bytes));
            assert_eq!(value.span.slice(results.payload().as_slice()), Some(*bytes));
            assert!(core::ptr::eq(results.raw(index).unwrap().as_ptr(),
                results.payload().as_slice()[value.span.offset as usize..].as_ptr()));
            if let Some(previous) = previous { assert!(previous < value.span.offset); }
            previous = Some(value.span.offset);
        }
        for index in [expected.len(), expected.len() + 1, usize::MAX] {
            assert_eq!(results.raw(index), None);
            assert_eq!(results.string(index), None);
        }
    }

    fn assert_error(error: Error, code: ErrorCode, at: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!(error.byte_offset, at);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn exact_grammar_matches_the_basic_parser_and_root_aliases() {
        for text in ["", "$", "a.b", "$.a.b", "a[0][1].b", "$[4294967295]",
            r#"[""]["a.b"]"#, r"a\.b.\*", r"a\\b.\u0061", "é.界.😀"] {
            let basic = super::super::parse::parse(text, PathLimits::default()).unwrap();
            let extended = parse(text, PathLimits::default()).unwrap();
            let expected: Vec<ExtensionSegment> = basic.segments().iter().cloned()
                .map(ExtensionSegment::Exact).collect();
            assert_eq!(extended.segments(), expected.as_slice(), "{text}");
            assert_eq!(extended.limits(), PathLimits::default());
        }
    }

    #[test]
    fn wildcard_forms_compose_and_literal_stars_remain_exact() {
        for text in ["*", "$.*", "$[*]", "[*]"] {
            assert_eq!(parse(text, PathLimits::default()).unwrap().segments(),
                &[ExtensionSegment::Wildcard]);
        }
        assert_eq!(parse("rows[*].*.x[0]", PathLimits::default()).unwrap().segments(),
            &[exact("rows"), ExtensionSegment::Wildcard, ExtensionSegment::Wildcard,
                exact("x"), ExtensionSegment::Exact(Segment::Index(0))]);
        for text in [r"\*", r#"["*"]"#, r#"$["\u002a"]"#] {
            assert_eq!(parse(text, PathLimits::default()).unwrap().segments(), &[exact("*")]);
        }
        for text in ["a*", "*a", "a.*b", "**"] {
            assert_eq!(parse(text, PathLimits::default()).unwrap_err().code,
                ErrorCode::InvalidSyntax, "{text}");
        }
    }

    #[test]
    fn filter_literals_decode_escapes_and_preserve_number_spelling() {
        let cases = [
            ("[?(@.x==null)]", filter("x", FilterValue::Null)),
            ("[?(@.x == true )]", filter("x", FilterValue::Bool(true))),
            ("[?(@.x\t==\r\nfalse\t)]", filter("x", FilterValue::Bool(false))),
            ("[?(@.x==-0.00E+02)]", filter("x", FilterValue::Number(String::from("-0.00E+02")))),
            (r#"[?(@.a\.b=="\u00e9\uD83D\uDE00\n")]"#,
                filter("a.b", FilterValue::String(String::from("é😀\n")))),
            (r#"[?(@.\*=="")]"#, filter("*", FilterValue::String(String::new()))),
            ("[?(@.a=b==1)]", filter("a=b", FilterValue::Number(String::from("1")))),
        ];
        for (text, expected) in cases {
            assert_eq!(parse(text, PathLimits::default()).unwrap().segments(), &[expected], "{text}");
        }
    }

    #[test]
    fn exact_parser_errors_keep_the_basic_code_source_and_byte_offset() {
        for text in [".a", "a..b", "a.", "a.[0]", "a[", "[01]", "[4294967296]",
            "[1,2]", "[1:2]", "a b", "a\\", "[\"\\q\"]", "[\"\\uD800\"]",
            "[\"\\uDC00\"]", "[\"\\uD800\\u0041\"]", "[\"\\u0x00\"]",
            "[\"\n\"]", "[\"é\" x]", "$.é]"] {
            let expected = super::super::parse::parse(text, PathLimits::default()).unwrap_err();
            let actual = parse(text, PathLimits::default()).unwrap_err();
            assert_eq!((actual.code, actual.source, actual.byte_offset),
                (expected.code, expected.source, expected.byte_offset), "{text}");
        }
    }

    #[test]
    fn proper_truncated_extension_prefixes_report_eof_at_the_end() {
        for text in ["$.", "[*", "[?", "[?(", "[?(@", "[?(@.", "[?(@.x",
            "[?(@.x=", "[?(@.x==", "[?(@.x==n", "[?(@.x==tru",
            "[?(@.x==-", "[?(@.x==1e+", "[?(@.x==\"", "[?(@.x==\"\\uD800",
            "[?(@.x==true", "[?(@.x==true)"] {
            assert_error(parse(text, PathLimits::default()).unwrap_err(),
                ErrorCode::UnexpectedEof, text.len() as u32);
        }
    }

    #[test]
    fn recognized_out_of_subset_selectors_are_rejected() {
        for text in ["$..a", "..a", "a[-1]", "[+1]", "[1,2]", "[:2]", "[1:]",
            "[*,0]", r#"["a","b"]"#, "[?(@.a.b==1)]", "[?(@.a[0]==1)]",
            "[?(@.a!=1)]", "[?(@.a>1)]", "[?(@.a <= 1)]", "[?(@.a=~1)]",
            "[?(@.a==1+2)]", "[?(@.a==true&&false)]", "[?(@.a===1)]",
            "[?(@.a==@.b)]", "[?(@.a==[1])]", "[?(@.a=={})]", "[?(@.a==/x/)]",
            "[?(@.a==floor(1))]", "[?(@.a==null())]", "[?(@.a==sum (1))]",
            "[?(length(@.a)==1)]", "[?(@['a']==1)]"] {
            let error = parse(text, PathLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature, "{text}: {error:?}");
            assert_eq!(error.source, ErrorSource::Cursor);
            assert!((error.byte_offset as usize) < text.len());
        }
    }

    #[test]
    fn whitespace_and_malformed_filter_punctuation_are_not_silently_accepted() {
        for text in [" *", "* ", "[ *]", "[* ]", "[? (@.a==1)]", "[?( @.a==1)]",
            "[?(@ .a==1)]", "[?(@. a==1)]", "[?(@.a==1) ]", "[?(@.a==)]",
            "[?(@.a==01)]", "[?(@.a==1.)]", "[?(@.a==1e)]", "[?(@.a==true ]",
            "[?(@.a==true\u{a0})]", "[?(@.a==\"\\q\")]", "[?(@.a==\"\n\")]"] {
            let error = parse(text, PathLimits::default()).unwrap_err();
            assert_eq!(error.source, ErrorSource::Cursor);
            assert!((error.byte_offset as usize) <= text.len());
            assert_ne!(error.code, ErrorCode::BudgetExhausted);
        }
        assert_error(parse("[?(@.x==01)]", PathLimits::default()).unwrap_err(),
            ErrorCode::InvalidNumber, 9);
        assert_error(parse("[?(@.x==\"\\q\")]", PathLimits::default()).unwrap_err(),
            ErrorCode::InvalidEscape, 10);
    }

    #[test]
    fn programmatic_numbers_require_complete_strict_json_grammar() {
        for text in ["0", "-0", "1", "-12", "1.0", "1e0", "1E+02", "-0.001e-9999",
            "18446744073709551616", "9007199254740993"] {
            let segments = [filter("n", FilterValue::Number(String::from(text)))];
            assert!(compile(&segments, PathLimits::default()).is_ok(), "{text}");
        }
        for text in ["", "-", "+1", "01", "-01", ".1", "1.", "1e", "1e+", "1e-",
            "NaN", "Infinity", "0x10", "1_000", " 1", "1 ", "1\n", "1+2", "1,2", "1true"] {
            let segments = [filter("n", FilterValue::Number(String::from(text)))];
            assert_error(compile(&segments, PathLimits::default()).unwrap_err(),
                ErrorCode::InvalidNumber, 0);
        }
    }

    #[test]
    fn compile_makes_independent_owned_copies_and_retains_limits() {
        let limits = PathLimits { max_paths: 0, max_trie_nodes: 0, ..PathLimits::default() };
        let mut segments = vec![exact("rows"), filter("tag", FilterValue::String(String::from("é"))),
            ExtensionSegment::Wildcard, ExtensionSegment::Exact(Segment::Index(u32::MAX))];
        let program = compile(&segments, limits).unwrap();
        if let ExtensionSegment::Exact(Segment::Key(key)) = &segments[0] {
            if let ExtensionSegment::Exact(Segment::Key(copy)) = &program.segments()[0] {
                assert_ne!(key.as_ptr(), copy.as_ptr());
            } else { panic!("compiled exact key changed kind"); }
        }
        segments.clear();
        drop(segments);
        assert_eq!(program.segments(), &[exact("rows"),
            filter("tag", FilterValue::String(String::from("é"))), ExtensionSegment::Wildcard,
            ExtensionSegment::Exact(Segment::Index(u32::MAX))]);
        assert_eq!(program.limits(), limits);
    }

    #[test]
    fn compile_limits_are_inclusive_and_zero_is_a_real_bound() {
        let limits = PathLimits { max_segments: 1, max_key_bytes: 2, max_path_bytes: 3,
            ..PathLimits::default() };
        for segment in [exact("é"), filter("é", FilterValue::String(String::from("é"))),
            filter("", FilterValue::Number(String::from("1e0")))] {
            assert!(compile(&[segment], limits).is_ok());
        }
        for segments in [vec![exact("éa")], vec![filter("éa", FilterValue::Null)],
            vec![filter("", FilterValue::String(String::from("éa")))],
            vec![filter("", FilterValue::Number(String::from("1e00")))],
            vec![ExtensionSegment::Wildcard, ExtensionSegment::Wildcard]] {
            assert_error(compile(&segments, limits).unwrap_err(), ErrorCode::BudgetExhausted, 0);
        }
        let zero = PathLimits { max_segments: 0, max_key_bytes: 0, max_path_bytes: 0, ..limits };
        assert!(compile(&[], zero).is_ok());
        assert_error(compile(&[ExtensionSegment::Wildcard], zero).unwrap_err(),
            ErrorCode::BudgetExhausted, 0);
        let one = PathLimits { max_segments: 1, ..zero };
        for segment in [exact(""), filter("", FilterValue::String(String::new())),
            ExtensionSegment::Wildcard, ExtensionSegment::Exact(Segment::Index(u32::MAX))] {
            assert!(compile(&[segment], one).is_ok());
        }
        assert_error(compile(&[filter("", FilterValue::Number(String::from("0")))], one)
            .unwrap_err(), ErrorCode::BudgetExhausted, 0);
    }

    #[test]
    fn textual_limits_count_source_bytes_and_decoded_utf8_bytes() {
        let text = r#"[?(@.é=="\u00e9")]"#;
        let limits = PathLimits { max_path_bytes: text.len() as u32, max_key_bytes: 2,
            max_segments: 1, ..PathLimits::default() };
        assert!(parse(text, limits).is_ok());
        assert_error(parse(text, PathLimits { max_path_bytes: text.len() as u32 - 1, ..limits })
            .unwrap_err(), ErrorCode::BudgetExhausted, text.len() as u32 - 1);
        assert_eq!(parse(text, PathLimits { max_key_bytes: 1, ..limits }).unwrap_err().code,
            ErrorCode::BudgetExhausted);
        assert_eq!(parse("*.*", limits).unwrap_err().code, ErrorCode::BudgetExhausted);
        let zero = PathLimits { max_path_bytes: 0, max_segments: 0, max_key_bytes: 0, ..limits };
        assert!(parse("", zero).is_ok());
        assert_error(parse("$", zero).unwrap_err(), ErrorCode::BudgetExhausted, 0);
        assert_error(parse("*", PathLimits { max_path_bytes: 1, ..zero }).unwrap_err(),
            ErrorCode::BudgetExhausted, 0);
        assert!(parse(r#"[""]"#, PathLimits { max_path_bytes: 4, max_segments: 1, ..zero }).is_ok());
    }

    #[test]
    fn wildcard_returns_immediate_values_in_document_order_without_keys() {
        let input = br#" {"z":1,"a":{"nested":2},"empty":[],"s":"x","n":null} "#;
        let results = run(input, "*");
        assert_raw(&results, &[b"1", br#"{"nested":2}"#, b"[]", br#""x""#, b"null"]);
        assert_eq!(results.matches().iter().map(|value| value.kind).collect::<Vec<_>>(),
            vec![ValueKind::Number, ValueKind::Object, ValueKind::Array,
                ValueKind::String, ValueKind::Null]);
        assert_eq!(results.string(3), Some("x"));
        assert_eq!(results.string(0), None);
        assert!(results.fully_validated());
        assert_eq!(results.payload().as_slice(), input);
        let results = run(br#"[0,[1,2],{},"x",false]"#, "[*]");
        assert_raw(&results, &[b"0", b"[1,2]", b"{}", br#""x""#, b"false"]);
    }

    #[test]
    fn exact_and_wildcard_compositions_have_no_implicit_recursion() {
        let input = br#"{"rows":[{"v":[1,2]},{"v":[3]},{"v":4},{"x":{"v":[5]}}]}"#;
        assert_raw(&run(input, "rows[*].v[*]"), &[b"1", b"2", b"3"]);
        assert_raw(&run(input, "rows[*].v[0]"), &[b"1", b"3"]);
        assert_raw(&run(input, "rows[3].x.v[*]"), &[b"5"]);
        for (input, path) in [(b"null".as_slice(), "*"), (b"1", "[0]"),
            (b"{}", "*"), (b"[]", "[*]"), (b"[1]", "[1]"), (b"[1]", "[4294967295]"),
            (br#"{"0":1}"#, "[0]"), (b"[1]", r#"["0"]"#)] {
            assert_raw(&run(input, path), &[]);
        }
    }

    #[test]
    fn filters_compare_scalar_kind_and_exact_number_lexemes() {
        let input = br#"[{"x":null,"id":0},{"x":false,"id":1},{"x":true,"id":2},{"x":"1","id":3},{"x":1,"id":4},{"x":1.0,"id":5},{"x":1e0,"id":6},{"x":-0,"id":7},{"x":0,"id":8},{"x":{},"id":9},{"x":[],"id":10},{"id":11},null,1,[]]"#;
        for (literal, expected) in [("null", b"0".as_slice()), ("false", b"1"),
            ("true", b"2"), (r#""1""#, b"3"), ("1", b"4"), ("1.0", b"5"),
            ("1e0", b"6"), ("-0", b"7"), ("0", b"8")] {
            assert_raw(&run(input, &format!("[?(@.x=={literal})].id")), &[expected]);
        }
        assert_raw(&run(input, "[?(@.x==2)].id"), &[]);
        assert_raw(&run(br#"{"x":1}"#, "[?(@.x==1)]"), &[]);
    }

    #[test]
    fn filter_numbers_never_round_or_normalize_exponents() {
        let input = br#"[{"n":9007199254740992},{"n":9007199254740993},{"n":1e9999},{"n":1E9999},{"n":1.00}]"#;
        for literal in ["9007199254740992", "9007199254740993", "1e9999", "1E9999", "1.00"] {
            assert_raw(&run(input, &format!("[?(@.n=={literal})].n")), &[literal.as_bytes()]);
        }
        assert_raw(&run(input, "[?(@.n==1.0)].n"), &[]);
    }

    #[test]
    fn filter_keys_and_strings_match_decoded_bytes_without_normalization() {
        let input = br#"[{"a\u002eb":"\u00e9","id":0},{"a.b":"e\u0301","id":1},{"a.b":"\uD83D\uDE00\n","id":2}]"#;
        for (literal, expected) in [(r#""é""#, b"0".as_slice()),
            (r#""e\u0301""#, b"1"), (r#""\uD83D\uDE00\n""#, b"2")] {
            assert_raw(&run(input, &format!(r"[?(@.a\.b=={literal})].id")), &[expected]);
        }
        let path = compile(&[filter("", FilterValue::String(String::from("\0\n")))],
            PathLimits::default()).unwrap();
        let result = query(Payload::from_slice(br#"[{"":"\u0000\n"},{"":""}]"#),
            &path, SkipMode::Strict, Limits::strict_default()).unwrap();
        assert_raw(&result, &[br#"{"":"\u0000\n"}"#]);
    }

    #[test]
    fn filters_use_only_the_first_immediate_occurrence_of_the_key() {
        let input = br#"[{"x":0,"x":1,"id":0},{"x":1,"x":0,"id":1},{"\u0078":0,"x":1,"id":2},{"x":{},"x":1,"id":3},{"nested":{"x":1},"id":4},{"x":1,"nested":{"x":0},"id":5}]"#;
        let path = parse("[?(@.x==1)].id", PathLimits::default()).unwrap();
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let result = query(Payload::from_slice(input), &path, mode, limits).unwrap();
            assert_raw(&result, &[b"1", b"5"]);
        }
        let error = query(Payload::from_slice(input), &path, SkipMode::Strict,
            Limits::strict_default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn delayed_filter_decisions_accept_or_rollback_earlier_descendant_matches() {
        let input = br#"[{"values":[1,2],"ok":false},{"values":[3,4],"ok":true},{"values":[5]},{"ok":true,"values":[6]}]"#;
        assert_raw(&run(input, "[?(@.ok==true)].values[*]"), &[b"3", b"4", b"6"]);
        let input = br#"[{"x":{"ok":true},"values":[1]},{"values":[2],"ok":null},{"values":[3],"ok":true}]"#;
        assert_raw(&run(input, "[?(@.ok==true)].values[*]"), &[b"3"]);
    }

    #[test]
    fn nested_filter_checkpoints_compose_across_siblings() {
        let input = br#"[{"items":[{"v":[1],"yes":true},{"v":[2],"yes":false}],"ok":false},{"items":[{"v":[3,4],"yes":false},{"v":[5,6],"yes":true},{"v":[7]}],"ok":true},{"ok":true,"items":[{"yes":true,"v":[8]}]}]"#;
        assert_raw(&run(input, "[?(@.ok==true)].items[?(@.yes==true)].v[*]"),
            &[b"5", b"6", b"8"]);
    }

    #[test]
    fn terminal_filters_capture_complete_objects_and_preserve_container_spelling() {
        let input = br#"[ { "data" : [ 1, {"s":"a\"}][b"} ], "ok" : true }, {"ok":false} ]"#;
        let expected = br#"{ "data" : [ 1, {"s":"a\"}][b"} ], "ok" : true }"#;
        let results = run(input, "[?(@.ok==true)]");
        assert_raw(&results, &[expected]);
        assert_eq!(results.matches()[0].kind, ValueKind::Object);
        assert_eq!(results.matches()[0].span, Span32::new(2, expected.len() as u32));
        assert_raw(&run(input, "[?(@.ok==true)].data[*]"),
            &[b"1", br#"{"s":"a\"}][b"}"#]);
    }

    #[test]
    fn duplicate_members_are_distinct_values_and_results_are_never_replayed() {
        let input = br#"{"a":1,"a":1,"a":2,"b":{"a":3}}"#;
        let limits = Limits { reject_duplicate_keys: false, ..Limits::strict_default() };
        for text in ["a", "*"] {
            let path = parse(text, PathLimits::default()).unwrap();
            let results = query(Payload::from_slice(input), &path, SkipMode::Strict, limits).unwrap();
            let expected: &[&[u8]] = if text == "a" { &[b"1", b"1", b"2"] }
                else { &[b"1", b"1", b"2", br#"{"a":3}"#] };
            assert_raw(&results, expected);
        }
    }

    #[test]
    fn cooked_matches_keep_tagged_arenas_and_complete_payload_owners_alive() {
        let input = br#" {"s":["plain","\n\uD83D\uDE00","",null]} trailing"#;
        let path = parse("s[*]", PathLimits::default()).unwrap();
        for payload in [Payload::from_slice(input), Payload::from_vec(Vec::from(&input[..]))] {
            let results = query(payload, &path, SkipMode::Query, Limits::strict_default()).unwrap();
            let copy = results.clone();
            drop(results);
            assert_raw(&copy, &[br#""plain""#, br#""\n\uD83D\uDE00""#, br#""""#, b"null"]);
            assert_eq!(copy.string(0), Some("plain"));
            assert_eq!(copy.string(1), Some("\n😀"));
            assert_eq!(copy.string(2), Some(""));
            assert_eq!(copy.string(3), None);
            assert_eq!(copy.payload().as_slice(), input);
            assert!(!copy.fully_validated());
            for index in 0..3 {
                let reference = copy.matches()[index].decoded.unwrap();
                let owner = match reference.source {
                    SpanSource::RawPayload => copy.payload().as_slice(),
                    SpanSource::DecodedArena => copy.decoded_bytes(),
                };
                assert_eq!(reference.span.slice(owner), Some(copy.string(index).unwrap().as_bytes()));
                assert!(core::ptr::eq(copy.string(index).unwrap().as_ptr(),
                    owner[reference.span.offset as usize..].as_ptr()));
            }
            assert_eq!(copy.matches()[0].decoded.unwrap().source, SpanSource::RawPayload);
            assert_eq!(copy.matches()[1].decoded.unwrap().source, SpanSource::DecodedArena);
        }
    }

    #[test]
    fn root_paths_select_each_complete_value_kind_with_outer_whitespace_excluded() {
        for token in ["null", "true", "false", "-0.1e+2", "\"é\"", "[]", "{\"a\":0}"] {
            let text = format!(" \n{token} \t");
            for root in ["", "$"] {
                let results = run(text.as_bytes(), root);
                assert_raw(&results, &[token.as_bytes()]);
                assert_eq!(results.matches()[0].span, Span32::new(2, token.len() as u32));
                assert!(results.fully_validated());
            }
        }
    }

    #[test]
    fn strict_mode_validates_eof_and_query_mode_finishes_the_whole_root() {
        for text in ["*", "missing", ""] {
            let path = parse(text, PathLimits::default()).unwrap();
            let input = br#"{"hit":0} trailing"#;
            let results = query(Payload::from_slice(input), &path, SkipMode::Query,
                Limits::strict_default()).unwrap();
            assert!(!results.fully_validated());
            assert_eq!(results.payload().as_slice(), input);
            assert_eq!(query(Payload::from_slice(input), &path, SkipMode::Strict,
                Limits::strict_default()).unwrap_err().code, ErrorCode::TrailingContent);
        }
        let path = parse("hit", PathLimits::default()).unwrap();
        for input in [br#"{"hit":0,"tail":["\q"]}"#.as_slice(),
            br#"{"hit":0,"tail":[1,]}"#, br#"{"hit":0,"tail": ["#] {
            assert!(query(Payload::from_slice(input), &path, SkipMode::Query,
                Limits::strict_default()).is_err());
        }
        let complete = query(Payload::from_slice(br#"{"hit":0}"#), &path, SkipMode::Query,
            Limits::strict_default()).unwrap();
        assert!(!complete.fully_validated());
        assert_raw(&complete, &[b"0"]);
    }

    #[test]
    fn visit_limits_count_events_and_prefix_states_with_inclusive_boundaries() {
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let make = |maximum| parse("", PathLimits { max_visits: maximum,
                ..PathLimits::default() }).unwrap();
            let results = query(Payload::from_slice(b"null"), &make(2), mode,
                Limits::strict_default()).unwrap();
            assert_raw(&results, &[b"null"]);
            assert_error(query(Payload::from_slice(b"null"), &make(1), mode,
                Limits::strict_default()).unwrap_err(), ErrorCode::BudgetExhausted, 4);
            assert_error(query(Payload::from_slice(b"null"), &make(0), mode,
                Limits::strict_default()).unwrap_err(), ErrorCode::BudgetExhausted, 0);
            let make = |maximum| parse("[*]", PathLimits { max_visits: maximum,
                ..PathLimits::default() }).unwrap();
            assert_raw(&query(Payload::from_slice(b"[0]"), &make(5), mode,
                Limits::strict_default()).unwrap(), &[b"0"]);
            assert_error(query(Payload::from_slice(b"[0]"), &make(4), mode,
                Limits::strict_default()).unwrap_err(), ErrorCode::BudgetExhausted, 2);
        }
        let mut visits = Visits { used: u32::MAX - 1, maximum: u32::MAX };
        visits.take(7).unwrap();
        assert_eq!(visits.used, u32::MAX);
        assert_error(visits.take(9).unwrap_err(), ErrorCode::BudgetExhausted, 9);
    }

    #[test]
    fn visits_also_bound_unmatched_events_and_provisional_filter_work() {
        let path = parse("missing", PathLimits { max_visits: 4, ..PathLimits::default() }).unwrap();
        assert_error(query(Payload::from_slice(b"[0,1]"), &path, SkipMode::Strict,
            Limits::strict_default()).unwrap_err(), ErrorCode::BudgetExhausted, 4);
        let path = parse("[?(@.ok==true)].x", PathLimits { max_visits: 6,
            ..PathLimits::default() }).unwrap();
        let input = br#"[{"x":0,"ok":false}]"#;
        let error = query(Payload::from_slice(input), &path, SkipMode::Strict,
            Limits::strict_default()).unwrap_err();
        assert_error(error, ErrorCode::BudgetExhausted, 7);
    }

    #[test]
    fn match_limits_reject_overflow_and_ignore_rolled_back_candidates() {
        let make = |text: &str, maximum| parse(text, PathLimits { max_matches: maximum,
            ..PathLimits::default() }).unwrap();
        for mode in [SkipMode::Strict, SkipMode::Query] {
            assert_raw(&query(Payload::from_slice(b"[0,1]"), &make("*", 2), mode,
                Limits::strict_default()).unwrap(), &[b"0", b"1"]);
            assert_error(query(Payload::from_slice(b"[0,1]"), &make("*", 1), mode,
                Limits::strict_default()).unwrap_err(), ErrorCode::BudgetExhausted, 5);
            assert_error(query(Payload::from_slice(b"null"), &make("", 0), mode,
                Limits::strict_default()).unwrap_err(), ErrorCode::BudgetExhausted, 4);
            assert_raw(&query(Payload::from_slice(b"null"), &make("*", 0), mode,
                Limits::strict_default()).unwrap(), &[]);
            let input = br#"[{"x":[1,2,3],"ok":false},{"x":[4,5]}]"#;
            assert_raw(&query(Payload::from_slice(input), &make("[?(@.ok==true)].x[*]", 0),
                mode, Limits::strict_default()).unwrap(), &[]);
        }
    }

    #[test]
    fn exact_unique_queries_agree_with_prior_wave_selector_results() {
        let input = br#"{"a":{"b":[null,"\n",{"c":3}]},"z":false}"#;
        for text in ["", "$", "a", "a.b", "a.b[0]", "a.b[1]", "a.b[2].c", "z", "missing"] {
            let paths = super::super::compile::compile_strings(&[text], PathLimits::default()).unwrap();
            let basic = super::super::query::execute(Payload::from_slice(input), &paths,
                SkipMode::Strict, Limits::strict_default()).unwrap();
            let extended = run(input, text);
            assert_eq!(extended.matches().first(), basic.slots()[0].as_ref());
            assert_eq!(extended.raw(0), basic.raw(0));
            assert_eq!(extended.string(0), basic.string(0));
            assert_eq!(extended.decoded_bytes(), basic.decoded_bytes());
            assert_eq!(extended.fully_validated(), basic.fully_validated());
        }
    }

    #[test]
    fn syntax_errors_propagate_unchanged_without_publishing_partial_matches() {
        let path = parse("*", PathLimits::default()).unwrap();
        for input in [b"".as_slice(), b" \n", b"[", b"[1,]", b"{\"x\":}", b"[}",
            b"[0,01]", b"[0,1e]", br#"[0,"\q"]"#, br#"[0,"\uD800"]"#,
            b"[0,\"\xff\"]", b"[0,\"\n\"]", br#"{"a":0,"\u0061":1}"#,
            b"null true"] {
            let mut cursor = jmax_cursor::walk::from_payload(Payload::from_slice(input),
                Limits::strict_default(), SkipMode::Strict).unwrap();
            let expected = loop {
                match jmax_cursor::walk::next(&mut cursor) {
                    Err(error) => break error,
                    Ok(ScanStep::Emit(_)) => {}
                    Ok(_) => panic!("invalid vector unexpectedly completed"),
                }
            };
            let actual = query(Payload::from_slice(input), &path, SkipMode::Strict,
                Limits::strict_default()).unwrap_err();
            assert_eq!(actual, expected, "{input:?}");
        }
    }

    #[test]
    fn syntax_resource_boundaries_and_all_supplied_payload_bytes_are_enforced() {
        let defaults = Limits::strict_default();
        let cases: &[(Limits, &[u8], &[u8], ErrorCode)] = &[
            (Limits { max_depth: 2, ..defaults }, b"[[]]", b"[[[]]]", ErrorCode::DepthLimitExceeded),
            (Limits { max_array_items: 1, ..defaults }, b"[0]", b"[0,1]", ErrorCode::ConstraintViolation),
            (Limits { max_fields_per_object: 1, ..defaults }, br#"{"a":0}"#,
                br#"{"a":0,"b":1}"#, ErrorCode::ConstraintViolation),
            (Limits { max_string_bytes: 1, ..defaults }, br#"["a"]"#,
                br#"["ab"]"#, ErrorCode::ConstraintViolation),
            (Limits { max_number_digits: 2, ..defaults }, b"[1e2]", b"[1e23]", ErrorCode::ConstraintViolation),
            (Limits { max_decoded_bytes: 1, ..defaults }, br#"["\n"]"#,
                br#"["\n","\t"]"#, ErrorCode::DecodedArenaOverflow),
        ];
        let path = parse("", PathLimits::default()).unwrap();
        for &(limits, allowed, denied, code) in cases {
            assert!(query(Payload::from_slice(allowed), &path, SkipMode::Strict, limits).is_ok());
            assert_eq!(query(Payload::from_slice(denied), &path, SkipMode::Strict, limits)
                .unwrap_err().code, code);
        }
        let input = b"[] trailing";
        for mode in [SkipMode::Strict, SkipMode::Query] {
            let error = query(Payload::from_slice(input), &path, mode,
                Limits { max_payload_bytes: input.len() as u32 - 1, ..defaults }).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        }
        let results = query(Payload::from_slice(input), &path, SkipMode::Query,
            Limits { max_payload_bytes: input.len() as u32, ..defaults }).unwrap();
        assert_raw(&results, &[b"[]"]);
        assert_eq!(results.payload().as_slice(), input);
    }

    #[test]
    fn deep_paths_and_many_siblings_use_bounded_iterative_traversal() {
        let depth = 64;
        let mut input = String::new();
        let mut text = String::new();
        for _ in 0..depth { input.push('['); text.push_str("[*]"); }
        input.push_str("17");
        for _ in 0..depth { input.push(']'); }
        assert_raw(&run(input.as_bytes(), &text), &[b"17"]);
        let mut input = String::from("[");
        for index in 0..128 {
            if index != 0 { input.push(','); }
            input.push_str(&format!("{{\"value\":{index},\"ok\":{}}}", index % 2 == 0));
        }
        input.push(']');
        let result = run(input.as_bytes(), "[?(@.ok==true)].value");
        assert_eq!(result.matches().len(), 64);
        for index in 0..64 {
            assert_eq!(result.raw(index), Some(format!("{}", index * 2).as_bytes()));
        }
    }
}
