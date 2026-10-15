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
