//! The bounded basic path grammar. Offsets always refer to the original text.

use super::*;

/// Parse an exact path, decoding keys without Unicode normalization.
///
/// Empty text and `$` select the root. Wildcards, filters, recursive descent,
/// and index unions or slices are deliberately outside this grammar.
pub fn parse(input: &str, limits: PathLimits) -> Result<Path> {
    check_input_length(input.len(), limits.max_path_bytes)?;
    let mut parser = Parser { input, position: 0, limits };
    let mut segments = Vec::new();
    if input.is_empty() {
        return Ok(Path { segments });
    }

    let rooted = parser.peek() == Some(b'$');
    if rooted {
        parser.position += 1;
        if parser.at_end() {
            return Ok(Path { segments });
        }
    }

    match parser.peek() {
        Some(b'.') if rooted => parser.dot()?,
        Some(b'.') => return Err(parser.dot_without_root()),
        Some(b'[') => {},
        _ if rooted => return Err(parser.error(
            ErrorCode::InvalidSyntax, parser.position, "expected dot or bracket after root")),
        _ => {},
    }
    parser.segment(&mut segments)?;

    while !parser.at_end() {
        match parser.peek() {
            Some(b'.') => parser.dot()?,
            Some(b'[') => {},
            _ => return Err(parser.error(
                ErrorCode::InvalidSyntax, parser.position, "expected dot or bracket after segment")),
        }
        parser.segment(&mut segments)?;
    }
    Ok(Path { segments })
}

/// Parse in input order, returning no paths if any input fails.
///
/// The path count is checked before inspecting any text. Parse error offsets
/// are relative to the individual failing input.
pub fn parse_many(inputs: &[&str], limits: PathLimits) -> Result<Vec<Path>> {
    if inputs.len() > limits.max_paths as usize {
        return Err(path_error(ErrorCode::BudgetExhausted, 0, "path count limit exceeded"));
    }
    let mut paths = Vec::new();
    paths.try_reserve_exact(inputs.len()).map_err(|_| path_error(
        ErrorCode::BudgetExhausted, 0, "path list allocation failed"))?;
    for input in inputs {
        paths.push(parse(input, limits)?);
    }
    Ok(paths)
}

fn check_input_length(length: usize, limit: u32) -> Result<()> {
    let length = u32::try_from(length).map_err(|_| path_error(
        ErrorCode::PayloadTooLarge, 0, "path length exceeds the offset range"))?;
    if length > limit {
        return Err(path_error(ErrorCode::BudgetExhausted, limit, "path byte limit exceeded"));
    }
    Ok(())
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
    limits: PathLimits,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn at_end(&self) -> bool {
        self.position == self.input.len()
    }

    fn error(&self, code: ErrorCode, position: usize, detail: &'static str) -> Error {
        // Input length is checked before this parser is constructed, and every
        // position is either an input byte offset or exactly the input length.
        path_error(code, position as u32, detail)
    }

    fn eof(&self) -> Error {
        self.error(ErrorCode::UnexpectedEof, self.input.len(), "incomplete path")
    }

    fn unsupported(&self, position: usize) -> Error {
        self.error(ErrorCode::UnsupportedSchemaFeature, position,
            "extension syntax is not supported by basic paths")
    }

    fn dot_without_root(&self) -> Error {
        if self.input.as_bytes().get(self.position + 1) == Some(&b'.') {
            self.unsupported(self.position)
        } else {
            self.error(ErrorCode::InvalidSyntax, self.position, "leading dot requires root marker")
        }
    }

    fn dot(&mut self) -> Result<()> {
        if self.input.as_bytes().get(self.position + 1) == Some(&b'.') {
            return Err(self.unsupported(self.position));
        }
        self.position += 1;
        match self.peek() {
            None => Err(self.eof()),
            Some(b'[') => Err(self.error(ErrorCode::InvalidSyntax, self.position,
                "dot must be followed by a nonempty bare key")),
            _ => Ok(()),
        }
    }

    fn segment(&mut self, segments: &mut Vec<Segment>) -> Result<()> {
        let start = self.position;
        if segments.len() >= self.limits.max_segments as usize {
            return Err(self.error(ErrorCode::BudgetExhausted, start, "path segment limit exceeded"));
        }
        let segment = if self.peek() == Some(b'[') {
            self.bracket()?
        } else {
            Segment::Key(self.bare_key()?)
        };
        if segments.len() == segments.capacity() {
            let target = segments.capacity().saturating_mul(2).max(4)
                .min(self.limits.max_segments as usize);
            segments.try_reserve_exact(target - segments.len()).map_err(|_| self.error(
                ErrorCode::BudgetExhausted, start, "path segment allocation failed"))?;
        }
        segments.push(segment);
        Ok(())
    }

    fn bare_key(&mut self) -> Result<String> {
        let start = self.position;
        let mut key = String::new();
        while let Some(character) = self.input[self.position..].chars().next() {
            if character == '.' || character == '[' {
                break;
            }
            let source = self.position;
            self.position += character.len_utf8();
            if character == '\\' {
                let escaped = self.input[self.position..].chars().next().ok_or_else(|| self.eof())?;
                if escaped.is_control() {
                    return Err(self.error(ErrorCode::InvalidSyntax, self.position,
                        "control character in bare key"));
                }
                self.position += escaped.len_utf8();
                self.append(&mut key, escaped, source)?;
            } else {
                if character == '*' && source == start
                    && matches!(self.peek(), None | Some(b'.' | b'[')) {
                    return Err(self.unsupported(source));
                }
                if matches!(character, ']' | '$' | '*' | '?')
                    || character.is_whitespace() || character.is_control() {
                    return Err(self.error(ErrorCode::InvalidSyntax, source,
                        "bare key character requires escaping"));
                }
                self.append(&mut key, character, source)?;
            }
        }
        if self.position == start {
            return Err(if self.at_end() { self.eof() } else {
                self.error(ErrorCode::InvalidSyntax, start, "expected nonempty bare key")
            });
        }
        Ok(key)
    }

    fn bracket(&mut self) -> Result<Segment> {
        self.position += 1;
        match self.peek() {
            None => Err(self.eof()),
            Some(b'"') => {
                let key = self.quoted_key()?;
                match self.peek() {
                    Some(b']') => {
                        self.position += 1;
                        Ok(Segment::Key(key))
                    }
                    None => Err(self.eof()),
                    _ => Err(self.error(ErrorCode::InvalidSyntax, self.position,
                        "expected closing bracket after quoted key")),
                }
            }
            Some(b'0'..=b'9') => self.index().map(Segment::Index),
            Some(b'?') | Some(b',' | b':') => Err(self.unsupported(self.position)),
            Some(b'*') if self.input.as_bytes().get(self.position + 1) == Some(&b']') => {
                Err(self.unsupported(self.position))
            }
            _ => Err(self.error(ErrorCode::InvalidSyntax, self.position,
                "expected unsigned index or double-quoted key")),
        }
    }

    fn index(&mut self) -> Result<u32> {
        let start = self.position;
        let mut value = 0u32;
        while let Some(digit @ b'0'..=b'9') = self.peek() {
            if self.position > start && self.input.as_bytes()[start] == b'0' {
                return Err(self.error(ErrorCode::InvalidSyntax, self.position,
                    "array index has a leading zero"));
            }
            value = value.checked_mul(10).and_then(|value| value.checked_add(u32::from(digit - b'0')))
                .ok_or_else(|| self.error(ErrorCode::NumberOverflow, self.position,
                    "array index exceeds u32"))?;
            self.position += 1;
        }
        match self.peek() {
            Some(b']') => {
                self.position += 1;
                Ok(value)
            }
            Some(b',' | b':') => Err(self.unsupported(self.position)),
            None => Err(self.eof()),
            _ => Err(self.error(ErrorCode::InvalidSyntax, self.position,
                "expected closing bracket after index")),
        }
    }

    fn quoted_key(&mut self) -> Result<String> {
        self.position += 1;
        let mut key = String::new();
        loop {
            let source = self.position;
            let character = self.input[self.position..].chars().next().ok_or_else(|| self.eof())?;
            self.position += character.len_utf8();
            match character {
                '"' => return Ok(key),
                '\\' => {
                    let decoded = self.json_escape(source)?;
                    self.append(&mut key, decoded, source)?;
                }
                '\u{0000}'..='\u{001f}' => return Err(self.error(
                    ErrorCode::ControlCharInString, source, "unescaped control in quoted key")),
                _ => self.append(&mut key, character, source)?,
            }
        }
    }

    fn json_escape(&mut self, source: usize) -> Result<char> {
        let escape = self.peek().ok_or_else(|| self.eof())?;
        // Non-ASCII escapes fail at their first byte without advancing into a
        // UTF-8 scalar. All successful cursor movements preserve boundaries.
        let character = match escape {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                self.position += 1;
                return self.unicode_escape(source);
            }
            _ => return Err(self.error(ErrorCode::InvalidEscape, self.position,
                "invalid JSON string escape")),
        };
        self.position += 1;
        Ok(character)
    }

    fn unicode_escape(&mut self, source: usize) -> Result<char> {
        let first = self.hex_quad()?;
        let scalar = match first {
            0xd800..=0xdbff => {
                if self.peek().is_none() {
                    return Err(self.eof());
                }
                if self.peek() != Some(b'\\') {
                    return Err(self.error(ErrorCode::LoneSurrogate, source,
                        "high surrogate requires a low surrogate escape"));
                }
                let second_source = self.position;
                self.position += 1;
                match self.peek() {
                    None => return Err(self.eof()),
                    Some(b'u') => self.position += 1,
                    _ => return Err(self.error(ErrorCode::LoneSurrogate, source,
                        "high surrogate requires a low surrogate escape")),
                }
                let second = self.hex_quad()?;
                if !(0xdc00..=0xdfff).contains(&second) {
                    return Err(self.error(ErrorCode::LoneSurrogate, second_source,
                        "expected low surrogate after high surrogate"));
                }
                0x10000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
            }
            0xdc00..=0xdfff => return Err(self.error(ErrorCode::LoneSurrogate, source,
                "low surrogate without a high surrogate")),
            _ => u32::from(first),
        };
        char::from_u32(scalar).ok_or_else(|| self.error(
            ErrorCode::InvalidEscape, source, "invalid Unicode scalar in quoted key"))
    }

    fn hex_quad(&mut self) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self.peek().ok_or_else(|| self.eof())?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error(ErrorCode::InvalidEscape, self.position,
                    "expected hexadecimal Unicode escape digit")),
            };
            value = (value << 4) | u16::from(digit);
            self.position += 1;
        }
        Ok(value)
    }

    fn append(&self, key: &mut String, character: char, source: usize) -> Result<()> {
        let maximum = self.limits.max_key_bytes as usize;
        let width = character.len_utf8();
        if width > maximum.saturating_sub(key.len()) {
            return Err(self.error(ErrorCode::BudgetExhausted, source, "decoded key byte limit exceeded"));
        }
        let needed = key.len() + width;
        if needed > key.capacity() {
            // Geometric growth avoids an allocation for every scalar, while
            // requested capacities never exceed the decoded key budget.
            let target = key.capacity().saturating_mul(2).max(16).max(needed).min(maximum);
            key.try_reserve_exact(target - key.len()).map_err(|_| self.error(
                ErrorCode::BudgetExhausted, source, "path key allocation failed"))?;
        }
        key.push(character);
        Ok(())
    }
}
