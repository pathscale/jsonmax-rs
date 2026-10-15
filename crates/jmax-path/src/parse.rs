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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    fn key(text: &str) -> Segment {
        Segment::Key(text.to_string())
    }

    fn assert_path(input: &str, expected: &[Segment]) {
        let path = parse(input, PathLimits::default()).unwrap();
        assert_eq!(path.segments(), expected, "input: {input:?}");
    }

    fn assert_error(input: &str, limits: PathLimits, code: ErrorCode, offset: u32) {
        let error = parse(input, limits).unwrap_err();
        assert_eq!(error.code, code, "input: {input:?}, error: {error:?}");
        assert_eq!(error.byte_offset, offset, "input: {input:?}");
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn documented_paths_and_root_forms() {
        assert_path("", &[]);
        assert_path("$", &[]);
        assert_path("book.price", &[key("book"), key("price")]);
        assert_path("$.book.price", &[key("book"), key("price")]);
        assert_path("book[0].price", &[key("book"), Segment::Index(0), key("price")]);
        assert_path("$[0]", &[Segment::Index(0)]);
        assert_path(r#"["a.b"][2]"#, &[key("a.b"), Segment::Index(2)]);
        assert_path(r"a\.b", &[key("a.b")]);
        assert_path(r#"[""]"#, &[key("")]);
    }

    #[test]
    fn selectors_can_repeat_and_follow_indices() {
        assert_path("a[0][1].b", &[key("a"), Segment::Index(0), Segment::Index(1), key("b")]);
        assert_path(r#"$["a"]["b"][3].c["d"]"#,
            &[key("a"), key("b"), Segment::Index(3), key("c"), key("d")]);
        assert_path(r#"[""][""]"#, &[key(""), key("")]);
        assert_path("123", &[key("123")]);
    }

    #[test]
    fn bare_escapes_consume_one_entire_scalar_without_json_decoding() {
        let cases = [
            (r"a\\b", "a\\b"), (r"\*", "*"), (r"\?", "?"), (r"\$", "$"),
            (r"a\[b\]", "a[b]"), (r"a\ b", "a b"), (r"\u0061", "u0061"),
            (r"\é\界\😀", "é界😀"), ("a\\\u{00a0}b", "a\u{00a0}b"),
        ];
        for (input, expected) in cases {
            assert_path(input, &[key(expected)]);
        }
        assert_path(r"a\..b", &[key("a."), key("b")]);
        assert_path(r"a\[0\][1]", &[key("a[0]"), Segment::Index(1)]);
    }

    #[test]
    fn bare_keys_accept_all_other_punctuation_and_unicode() {
        assert_path("é.界.😀", &[key("é"), key("界"), key("😀")]);
        assert_path("a,:@()'\"/+-=b", &[key("a,:@()'\"/+-=b")]);
        assert_path("e\u{0301}", &[key("e\u{0301}")]);
        assert_ne!(parse("é", PathLimits::default()).unwrap(),
            parse("e\u{0301}", PathLimits::default()).unwrap());
    }

    #[test]
    fn quoted_keys_decode_all_simple_json_escapes() {
        assert_path(r#"["\"\\\/\b\f\n\r\t"]"#, &[key("\"\\/\u{0008}\u{000c}\n\r\t")]);
        assert_path(r#"["a.b[0] $*? "]"#, &[key("a.b[0] $*? ")]);
        assert_path(r#"["*"]["?"]"#, &[key("*"), key("?")]);
        assert_path("[\"é界😀\"]", &[key("é界😀")]);
        // JSON only forbids unescaped U+0000 through U+001F.
        assert_path("[\"\u{007f}\u{0085}\u{2028}\"]", &[key("\u{007f}\u{0085}\u{2028}")]);
    }

    #[test]
    fn unicode_escape_boundaries_and_surrogate_pairs() {
        assert_path(r#"["\u0000\u007f\u0080\u07ff\u0800\ud7ff\uE000\uffff"]"#,
            &[key("\0\u{007f}\u{0080}\u{07ff}\u{0800}\u{d7ff}\u{e000}\u{ffff}")]);
        assert_path(r#"["\uD800\uDC00\uDBFF\uDFFF\ud83d\ude00"]"#,
            &[key("\u{10000}\u{10ffff}😀")]);
        assert_eq!(parse(r#"["\u00e9"]"#, PathLimits::default()).unwrap(),
            parse("é", PathLimits::default()).unwrap());
    }

    #[test]
    fn indices_include_u32_boundaries() {
        for value in [0, 1, 9, 10, 255, 65_535, 2_147_483_648, u32::MAX] {
            assert_path(&format!("[{value}]"), &[Segment::Index(value)]);
        }
        assert_path(r#"["0"][0]"#, &[key("0"), Segment::Index(0)]);
        for (input, offset) in [("[4294967296]", 10), ("[9999999999]", 10),
            ("[18446744073709551615]", 11), ("a[42949672960]", 11)] {
            assert_error(input, PathLimits::default(), ErrorCode::NumberOverflow, offset);
        }
    }

    #[test]
    fn malformed_indices_report_the_first_offending_byte() {
        for (input, offset) in [
            ("[]", 1), ("[-1]", 1), ("[+1]", 1), ("[00]", 2), ("[01]", 2),
            ("[0.0]", 2), ("[1e2]", 2), ("[1 ]", 2), ("[ 1]", 1),
            ("[\t1]", 1), ("[0x1]", 2), ("[١]", 1), ("[1a]", 2),
        ] {
            assert_error(input, PathLimits::default(), ErrorCode::InvalidSyntax, offset);
        }
    }

    #[test]
    fn malformed_path_structure_and_no_implicit_whitespace() {
        for (input, offset) in [
            (".", 0), (".a", 0), ("$a", 1), ("$$", 1), ("$ [0]", 1),
            ("a.[0]", 2), ("$.[0]", 2), ("a]", 1), ("a$b", 1), ("a?", 1),
            ("a*", 1), ("*a", 0), ("**", 0), ("[**]", 1),
            (" a", 0), ("a ", 1), ("a\tb", 1), ("a\u{00a0}b", 1),
            ("a\u{2003}b", 1), ("a\u{0000}b", 1), ("a\u{007f}b", 1),
            ("a\u{0085}b", 1), ("a[0]b", 4), ("[0]$", 3),
            ("['a']", 1), ("[\"a\" ]", 4), ("[ \"a\"]", 1),
            ("[\"a\"]b", 5), ("[\"a\",\"b\"]", 4), ("[\"a\":1]", 4),
        ] {
            assert_error(input, PathLimits::default(), ErrorCode::InvalidSyntax, offset);
        }
    }

    #[test]
    fn proper_truncated_prefixes_report_input_end() {
        for input in ["$.", "a.", "[", "a[", "[0", "[4294967295", "[\"", "[\"a",
            "[\"a\"", "[\"\\", "[\"\\u", "[\"\\u0", "[\"\\u00", "[\"\\u000",
            "[\"\\uD800", "[\"\\uD800\\", "[\"\\uD800\\u", "[\"\\uD800\\uDC0",
            "a\\", "é.界\\"] {
            assert_error(input, PathLimits::default(), ErrorCode::UnexpectedEof, input.len() as u32);
        }
    }

    #[test]
    fn extension_introducers_are_explicitly_unsupported() {
        for (input, offset) in [
            ("*", 0), ("*.a", 0), ("*[0]", 0), ("$.*", 2), ("a.*", 2),
            ("[*]", 1), ("a[*]", 2), ("[?", 1), ("[?(@.a==1)]", 1),
            ("..a", 0), ("$..a", 1), ("a..b", 1), ("a...", 1),
            ("[1,2]", 2), ("[0:2]", 2), ("[:2]", 1), ("[1:]", 2), ("[,1]", 1),
        ] {
            assert_error(input, PathLimits::default(), ErrorCode::UnsupportedSchemaFeature, offset);
        }
        assert_path(r"\*\.\?", &[key("*.?")]);
        assert_path(r#"["[*][?]..,:$*"]"#, &[key("[*][?]..,:$*")]);
    }

    #[test]
    fn invalid_json_escapes_and_hex_digits_have_precise_offsets() {
        for (input, offset) in [
            (r#"["\x"]"#, 3), (r#"["\v"]"#, 3), (r#"["\'"]"#, 3),
            (r#"["\uZ000"]"#, 4), (r#"["\u0Z00"]"#, 5),
            (r#"["\u00Z0"]"#, 6), (r#"["\u000Z"]"#, 7),
            (r#"["\u00"]"#, 6), (r#"["\é"]"#, 3), (r#"["\u0界00"]"#, 5),
            (r#"["\uD800\uDC0Z"]"#, 13),
        ] {
            assert_error(input, PathLimits::default(), ErrorCode::InvalidEscape, offset);
        }
    }

    #[test]
    fn lone_surrogates_and_invalid_pairs_are_rejected() {
        for (input, offset) in [
            (r#"["\uD800"]"#, 2), (r#"["\uDBFFx"]"#, 2),
            (r#"["\uDC00"]"#, 2), (r#"["\uDFFF"]"#, 2),
            (r#"["\uD800\n"]"#, 2), (r#"["\uD800\u0041"]"#, 8),
            (r#"["\uD800\uD800"]"#, 8),
        ] {
            assert_error(input, PathLimits::default(), ErrorCode::LoneSurrogate, offset);
        }
    }

    #[test]
    fn all_raw_json_controls_are_rejected_but_unicode_escapes_are_valid() {
        for value in 0u8..=31 {
            let character = char::from(value);
            assert_error(&format!("[\"{character}\"]"), PathLimits::default(),
                ErrorCode::ControlCharInString, 2);
            assert_error(&format!("a{character}"), PathLimits::default(), ErrorCode::InvalidSyntax, 1);
            assert_error(&format!("a\\{character}"), PathLimits::default(), ErrorCode::InvalidSyntax, 2);
            assert_path(&format!("[\"\\u{value:04x}\"]"), &[key(&character.to_string())]);
        }
        assert_error("a\\\u{0085}", PathLimits::default(), ErrorCode::InvalidSyntax, 2);
        assert_error("[\"\\\n\"]", PathLimits::default(), ErrorCode::InvalidEscape, 3);
    }

    #[test]
    fn diagnostics_use_byte_offsets_after_multibyte_keys() {
        assert_error("é.界?", PathLimits::default(), ErrorCode::InvalidSyntax, 6);
        assert_error("😀[01]", PathLimits::default(), ErrorCode::InvalidSyntax, 6);
        assert_error("é..a", PathLimits::default(), ErrorCode::UnsupportedSchemaFeature, 2);
        assert_error("[\"界\\x\"]", PathLimits::default(), ErrorCode::InvalidEscape, 6);
        assert_error("[\"😀\n\"]", PathLimits::default(), ErrorCode::ControlCharInString, 6);
    }

    #[test]
    fn textual_byte_limit_is_inclusive_and_precedes_grammar() {
        let limits = PathLimits { max_path_bytes: 3, ..PathLimits::default() };
        assert!(parse("a.b", limits).is_ok());
        assert_error("a.bc", limits, ErrorCode::BudgetExhausted, 3);
        assert_error("....", limits, ErrorCode::BudgetExhausted, 3);
        let limits = PathLimits { max_path_bytes: 1, ..limits };
        assert_error("é", limits, ErrorCode::BudgetExhausted, 1);
        let limits = PathLimits { max_path_bytes: 0, ..limits };
        assert!(parse("", limits).unwrap().segments().is_empty());
        assert_error("$", limits, ErrorCode::BudgetExhausted, 0);

        let limits = PathLimits { max_path_bytes: 10, max_key_bytes: 1, ..PathLimits::default() };
        assert!(parse(r#"["\u0061"]"#, limits).is_ok());
        assert_error(r#"["\u0061"]"#, PathLimits { max_path_bytes: 9, ..limits },
            ErrorCode::BudgetExhausted, 9);

        let limits = PathLimits { max_key_bytes: u32::MAX, ..PathLimits::default() };
        let text = "a".repeat(limits.max_path_bytes as usize);
        assert!(parse(&text, limits).is_ok());
        assert_error(&format!("{text}a"), limits, ErrorCode::BudgetExhausted, limits.max_path_bytes);
    }

    #[test]
    fn offset_length_guard_accepts_the_u32_boundary() {
        assert!(check_input_length(u32::MAX as usize, u32::MAX).is_ok());
        let error = check_input_length(u32::MAX as usize, u32::MAX - 1).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, u32::MAX - 1);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn unrepresentable_input_length_precedes_the_configured_budget() {
        let error = check_input_length(u32::MAX as usize + 1, 0).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!(error.byte_offset, 0);
    }

    #[test]
    fn segment_limits_are_inclusive_and_roots_need_no_segments() {
        let limits = PathLimits { max_segments: 2, ..PathLimits::default() };
        assert!(parse("a[0]", limits).is_ok());
        assert_error("a[0].b", limits, ErrorCode::BudgetExhausted, 5);
        assert_error("[0][1][2]", limits, ErrorCode::BudgetExhausted, 6);
        let limits = PathLimits { max_segments: 0, ..limits };
        assert!(parse("", limits).is_ok());
        assert!(parse("$", limits).is_ok());
        assert_error("a", limits, ErrorCode::BudgetExhausted, 0);
        assert_error("$[0]", limits, ErrorCode::BudgetExhausted, 1);
        assert_error(r#"[""]"#, limits, ErrorCode::BudgetExhausted, 0);
    }

    #[test]
    fn decoded_key_budget_counts_utf8_bytes_and_resets_per_segment() {
        let limits = PathLimits { max_key_bytes: 2, ..PathLimits::default() };
        assert!(parse("ab.cd", limits).is_ok());
        assert!(parse("é", limits).is_ok());
        assert!(parse(r#"["\u00e9"]"#, limits).is_ok());
        assert_error("abc", limits, ErrorCode::BudgetExhausted, 2);
        assert_error("éx", limits, ErrorCode::BudgetExhausted, 2);
        assert_error("界", limits, ErrorCode::BudgetExhausted, 0);
        assert_error(r"a\界", limits, ErrorCode::BudgetExhausted, 1);
        assert_error(r#"["a\u00e9"]"#, limits, ErrorCode::BudgetExhausted, 3);
        assert_error(r#"["\uD83D\uDE00"]"#, limits, ErrorCode::BudgetExhausted, 2);
        let limits = PathLimits { max_key_bytes: 4, ..limits };
        assert!(parse(r#"["\uD83D\uDE00"]"#, limits).is_ok());
        assert!(parse("😀", limits).is_ok());
    }

    #[test]
    fn zero_key_budget_allows_empty_keys_and_indices() {
        let limits = PathLimits { max_key_bytes: 0, ..PathLimits::default() };
        assert!(parse("$", limits).is_ok());
        assert!(parse("[0]", limits).is_ok());
        assert!(parse(r#"[""][""]"#, limits).is_ok());
        assert_error("a", limits, ErrorCode::BudgetExhausted, 0);
        assert_error(r#"["a"]"#, limits, ErrorCode::BudgetExhausted, 2);
        assert_error(r#"["\u0000"]"#, limits, ErrorCode::BudgetExhausted, 2);
    }

    #[test]
    fn default_and_custom_growth_boundaries_are_exact() {
        let limits = PathLimits::default();
        let maximum_key = "a".repeat(limits.max_key_bytes as usize);
        assert!(parse(&maximum_key, limits).is_ok());
        assert_error(&format!("{maximum_key}b"), limits, ErrorCode::BudgetExhausted,
            limits.max_key_bytes);
        let maximum_path = vec!["[0]"; limits.max_segments as usize].concat();
        assert_eq!(parse(&maximum_path, limits).unwrap().segments().len(), limits.max_segments as usize);
        assert_error(&format!("{maximum_path}[0]"), limits, ErrorCode::BudgetExhausted,
            maximum_path.len() as u32);
        for maximum in [1, 3, 15, 16, 17, 31, 32, 33] {
            let limits = PathLimits { max_key_bytes: maximum, ..limits };
            let text = "x".repeat(maximum as usize);
            assert!(parse(&text, limits).is_ok());
            assert_error(&format!("{text}x"), limits, ErrorCode::BudgetExhausted, maximum);
        }
    }

    #[test]
    fn parse_many_preserves_order_duplicates_and_individual_offsets() {
        let limits = PathLimits::default();
        let paths = parse_many(&["", "a", "$", "a", "[0]"], limits).unwrap();
        assert_eq!(paths.len(), 5);
        assert_eq!(paths[0], Path::root());
        assert_eq!(paths[1].segments(), &[key("a")]);
        assert_eq!(paths[2], Path::root());
        assert_eq!(paths[3], paths[1]);
        assert_eq!(paths[4].segments(), &[Segment::Index(0)]);
        let error = parse_many(&["good", "é?", "[01]"], limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSyntax);
        assert_eq!(error.byte_offset, 2);
        assert_eq!(error.source, ErrorSource::Cursor);
        let error = parse_many(&["[", "[01]"], limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnexpectedEof);
        assert_eq!(error.byte_offset, 1);
    }

    #[test]
    fn path_count_budget_is_inclusive_and_checked_before_parsing() {
        let limits = PathLimits { max_paths: 2, ..PathLimits::default() };
        assert_eq!(parse_many(&["a", "b"], limits).unwrap().len(), 2);
        let error = parse_many(&["[", "a", "b"], limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.source, ErrorSource::Cursor);
        let limits = PathLimits { max_paths: 0, max_path_bytes: 0, ..limits };
        assert!(parse_many(&[], limits).unwrap().is_empty());
        assert_eq!(parse_many(&[""], limits).unwrap_err().code, ErrorCode::BudgetExhausted);
        // A single-path parse does not consume the parse_many path-count budget.
        assert!(parse("", limits).is_ok());

        let limits = PathLimits::default();
        let mut inputs = vec![""; limits.max_paths as usize];
        assert_eq!(parse_many(&inputs, limits).unwrap().len(), inputs.len());
        inputs.push("");
        assert_eq!(parse_many(&inputs, limits).unwrap_err().code, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn compilation_and_query_budgets_do_not_restrict_parsing() {
        let limits = PathLimits {
            max_paths: 0, max_trie_nodes: 0, max_matches: 0, max_visits: 0,
            ..PathLimits::default()
        };
        assert_eq!(parse("a[0]", limits).unwrap().segments(), &[key("a"), Segment::Index(0)]);
    }

    #[test]
    fn owned_paths_outlive_and_do_not_borrow_the_input() {
        let path = {
            let input = String::from(r#"["\uD83D\uDE00"].tail[4]"#);
            parse(&input, PathLimits::default()).unwrap()
        };
        assert_eq!(path.segments(), &[key("😀"), key("tail"), Segment::Index(4)]);
    }
}
