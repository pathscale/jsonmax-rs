//! Strict validation of borrowed JSON fragments and syntax-aware emission.
//!
//! Parsing uses one fallibly allocated frame per open container. Strings and
//! numbers are scanned without allocating or converting their values, so large
//! valid numbers retain their exact spelling and escaped strings need no arena.

use super::*;

/// Validates exactly one JSON value, retaining all leading and trailing JSON
/// whitespace in the returned immutable borrow.
///
/// Every limit is inclusive. String limits count decoded UTF-8 bytes, container
/// limits apply independently to each array or object, and scalar depth is zero.
/// No fragment is constructed until the complete input has been accepted.
pub fn validate(bytes: &[u8], limits: EncodeLimits) -> Result<RawFragment<'_>> {
    checked_output_len(0, bytes.len(), limits.max_output_bytes)?;
    // Validate even bytes outside strings, including a malformed trailing UTF-8
    // sequence. Subsequent scanning can count literal string bytes directly.
    core::str::from_utf8(bytes).map_err(|error| {
        raw_error(ErrorCode::InvalidUtf8, error.valid_up_to(), "invalid fragment UTF-8")
    })?;
    let mut parser = Parser {
        bytes,
        limits,
        offset: 0,
        frames: Vec::new(),
        depth: 0,
    };
    parser.whitespace();
    let kind = parser.value()?;
    parser.containers()?;
    parser.whitespace();
    if parser.offset != bytes.len() {
        return Err(parser.error(ErrorCode::TrailingContent, "content after the JSON value"));
    }
    Ok(RawFragment { bytes, kind, depth: parser.depth })
}

/// Emits a fragment through the writer's normal state, limits and poisoning.
pub fn emit(writer: &mut JsonWriter<'_>, fragment: RawFragment<'_>) -> Result<()> {
    writer.raw(fragment)
}

fn raw_error(code: ErrorCode, offset: usize, detail: &'static str) -> Error {
    // The ABI carries a u32 diagnostic offset. Keep wider host offsets from
    // wrapping if a caller explicitly permits a fragment larger than 4 GiB.
    Error::new(code, ErrorSource::Encode, u32::try_from(offset).unwrap_or(u32::MAX), detail)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    ArrayFirst,
    ArrayValue,
    ArrayAfter,
    ObjectFirst,
    ObjectKey,
    ObjectColon,
    ObjectValue,
    ObjectAfter,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    phase: Phase,
    count: u32,
}

struct Parser<'a> {
    bytes: &'a [u8],
    limits: EncodeLimits,
    offset: usize,
    frames: Vec<Frame>,
    depth: u16,
}

impl Parser<'_> {
    fn error(&self, code: ErrorCode, detail: &'static str) -> Error {
        raw_error(code, self.offset, detail)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn required(&self) -> Result<u8> {
        self.peek().ok_or_else(|| self.error(ErrorCode::UnexpectedEof, "incomplete JSON value"))
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.offset += 1;
        }
    }

    /// Parses a scalar completely or starts a container. It never calls itself.
    fn value(&mut self) -> Result<FragmentKind> {
        match self.required()? {
            b'n' => {
                self.literal(b"null")?;
                Ok(FragmentKind::Null)
            }
            b't' => {
                self.literal(b"true")?;
                Ok(FragmentKind::Boolean)
            }
            b'f' => {
                self.literal(b"false")?;
                Ok(FragmentKind::Boolean)
            }
            b'"' => {
                self.string()?;
                Ok(FragmentKind::String)
            }
            b'-' | b'0'..=b'9' => {
                self.number()?;
                Ok(FragmentKind::Number)
            }
            b'[' => {
                self.open(Phase::ArrayFirst)?;
                Ok(FragmentKind::Array)
            }
            b'{' => {
                self.open(Phase::ObjectFirst)?;
                Ok(FragmentKind::Object)
            }
            b'+' | b'.' | b'N' | b'I' => {
                Err(self.error(ErrorCode::InvalidNumber, "invalid JSON number"))
            }
            _ => Err(self.error(ErrorCode::InvalidSyntax, "expected a JSON value")),
        }
    }

    fn open(&mut self, phase: Phase) -> Result<()> {
        if self.frames.len() >= usize::from(self.limits.max_depth) {
            return Err(self.error(ErrorCode::DepthLimitExceeded, "fragment depth limit exceeded"));
        }
        self.frames.try_reserve(1).map_err(|_| {
            self.error(ErrorCode::BudgetExhausted, "fragment frame allocation failed")
        })?;
        self.frames.push(Frame { phase, count: 0 });
        // The depth check above ensures this conversion is exact.
        self.depth = self.depth.max(self.frames.len() as u16);
        self.offset += 1;
        Ok(())
    }

    fn close(&mut self) {
        self.frames.pop();
        self.offset += 1;
    }

    /// Counts a property at its key, or an array item at its value. Children
    /// never increment their parent again when they close.
    fn count(&mut self, index: usize) -> Result<()> {
        if self.frames[index].count >= self.limits.max_container_items {
            return Err(self.error(ErrorCode::BudgetExhausted, "fragment container item limit exceeded"));
        }
        self.frames[index].count += 1;
        Ok(())
    }

    fn containers(&mut self) -> Result<()> {
        while let Some(frame) = self.frames.last().copied() {
            // This index remains valid across scanning a key or scalar. When a
            // child is opened, the parent's next phase has already been saved.
            let index = self.frames.len() - 1;
            self.whitespace();
            let byte = self.required()?;
            match frame.phase {
                Phase::ArrayFirst if byte == b']' => self.close(),
                Phase::ArrayFirst | Phase::ArrayValue => {
                    self.count(index)?;
                    self.frames[index].phase = Phase::ArrayAfter;
                    self.value()?;
                }
                Phase::ArrayAfter => match byte {
                    b']' => self.close(),
                    b',' => {
                        self.frames[index].phase = Phase::ArrayValue;
                        self.offset += 1;
                    }
                    _ => return Err(self.error(ErrorCode::InvalidSyntax, "expected array comma or close")),
                },
                Phase::ObjectFirst if byte == b'}' => self.close(),
                Phase::ObjectFirst | Phase::ObjectKey => {
                    if byte != b'"' {
                        return Err(self.error(ErrorCode::InvalidSyntax, "expected a quoted object key"));
                    }
                    self.count(index)?;
                    self.string()?;
                    self.frames[index].phase = Phase::ObjectColon;
                }
                Phase::ObjectColon => {
                    if byte != b':' {
                        return Err(self.error(ErrorCode::InvalidSyntax, "expected object colon"));
                    }
                    self.offset += 1;
                    self.frames[index].phase = Phase::ObjectValue;
                }
                Phase::ObjectValue => {
                    self.frames[index].phase = Phase::ObjectAfter;
                    self.value()?;
                }
                Phase::ObjectAfter => match byte {
                    b'}' => self.close(),
                    b',' => {
                        self.frames[index].phase = Phase::ObjectKey;
                        self.offset += 1;
                    }
                    _ => return Err(self.error(ErrorCode::InvalidSyntax, "expected object comma or close")),
                },
            }
        }
        Ok(())
    }

    fn literal(&mut self, spelling: &[u8]) -> Result<()> {
        for &expected in spelling {
            if self.required()? != expected {
                return Err(self.error(ErrorCode::InvalidSyntax, "invalid JSON literal"));
            }
            self.offset += 1;
        }
        Ok(())
    }

    fn number(&mut self) -> Result<()> {
        let start = self.offset;
        if self.peek() == Some(b'-') {
            self.number_byte(start)?;
        }
        match self.required()? {
            b'0' => {
                self.number_byte(start)?;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error(ErrorCode::InvalidNumber, "leading zero in JSON number"));
                }
            }
            b'1'..=b'9' => self.digits(start)?,
            _ => return Err(self.error(ErrorCode::InvalidNumber, "expected integer digits")),
        }
        if self.peek() == Some(b'.') {
            self.number_byte(start)?;
            self.required_digits(start)?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.number_byte(start)?;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.number_byte(start)?;
            }
            self.required_digits(start)?;
        }
        match self.peek() {
            None | Some(b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}') => Ok(()),
            _ => Err(self.error(ErrorCode::InvalidNumber, "invalid character after JSON number")),
        }
    }

    fn required_digits(&mut self, start: usize) -> Result<()> {
        if !self.required()?.is_ascii_digit() {
            return Err(self.error(ErrorCode::InvalidNumber, "expected number digits"));
        }
        self.digits(start)
    }

    fn digits(&mut self, start: usize) -> Result<()> {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.number_byte(start)?;
        }
        Ok(())
    }

    fn number_byte(&mut self, start: usize) -> Result<()> {
        // All callers have established that a byte exists. Check before the
        // increment, making a zero budget and the first excess byte precise.
        if self.offset - start >= self.limits.max_number_bytes as usize {
            return Err(self.error(ErrorCode::BudgetExhausted, "fragment number byte limit exceeded"));
        }
        self.offset += 1;
        Ok(())
    }

    fn string(&mut self) -> Result<()> {
        self.offset += 1; // Opening quote was checked by the caller.
        let mut decoded = 0u32;
        loop {
            let at = self.offset;
            let width = match self.required()? {
                b'"' => {
                    self.offset += 1;
                    return Ok(());
                }
                0..=0x1f => {
                    return Err(self.error(ErrorCode::ControlCharInString, "unescaped control in string"));
                }
                b'\\' => {
                    self.offset += 1;
                    match self.required()? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                            self.offset += 1;
                            1
                        }
                        b'u' => {
                            self.offset += 1;
                            let unit = self.hex_unit()?;
                            match unit {
                                0xd800..=0xdbff => {
                                    let low_at = self.offset;
                                    if self.required()? != b'\\' {
                                        return Err(raw_error(ErrorCode::LoneSurrogate, at, "high surrogate requires a low surrogate"));
                                    }
                                    self.offset += 1;
                                    if self.required()? != b'u' {
                                        return Err(raw_error(ErrorCode::LoneSurrogate, at, "high surrogate requires a Unicode escape"));
                                    }
                                    self.offset += 1;
                                    let low = self.hex_unit()?;
                                    if !(0xdc00..=0xdfff).contains(&low) {
                                        return Err(raw_error(ErrorCode::LoneSurrogate, low_at, "expected a low surrogate"));
                                    }
                                    4
                                }
                                0xdc00..=0xdfff => {
                                    return Err(raw_error(ErrorCode::LoneSurrogate, at, "low surrogate without a high surrogate"));
                                }
                                0..=0x7f => 1,
                                0x80..=0x7ff => 2,
                                _ => 3,
                            }
                        }
                        _ => return Err(self.error(ErrorCode::InvalidEscape, "invalid JSON string escape")),
                    }
                }
                _ => {
                    // UTF-8 was checked for the entire slice. Counting each
                    // literal byte yields its exact decoded UTF-8 size.
                    self.offset += 1;
                    1
                }
            };
            decoded = self.string_bytes(decoded, width, at)?;
        }
    }

    fn string_bytes(&self, decoded: u32, width: u32, at: usize) -> Result<u32> {
        let next = decoded.checked_add(width).ok_or_else(|| {
            raw_error(ErrorCode::BudgetExhausted, at, "decoded string byte count overflow")
        })?;
        if next > self.limits.max_string_bytes {
            return Err(raw_error(ErrorCode::BudgetExhausted, at, "fragment decoded string limit exceeded"));
        }
        Ok(next)
    }

    fn hex_unit(&mut self) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.required()? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error(ErrorCode::InvalidEscape, "expected four hexadecimal escape digits")),
            };
            value = (value << 4) | u16::from(digit);
            self.offset += 1;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error(bytes: &[u8], limits: EncodeLimits, code: ErrorCode, offset: usize) {
        let error = validate(bytes, limits).unwrap_err();
        assert_eq!(error.code, code, "input: {:?}", bytes);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.byte_offset, u32::try_from(offset).unwrap());
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn parser(bytes: &[u8], limits: EncodeLimits) -> Parser<'_> {
        Parser { bytes, limits, offset: 0, frames: Vec::new(), depth: 0 }
    }

    #[test]
    fn roots_record_category_depth_and_the_exact_original_borrow() {
        let cases: &[(&[u8], FragmentKind, u16)] = &[
            (b"null", FragmentKind::Null, 0),
            (b" true ", FragmentKind::Boolean, 0),
            (b"\r\nfalse\t", FragmentKind::Boolean, 0),
            (b" -0.0100E+002 ", FragmentKind::Number, 0),
            (br#""\u0061\/""#, FragmentKind::String, 0),
            (b"{}", FragmentKind::Object, 1),
            (b"[]", FragmentKind::Array, 1),
            (br#" { "x": [null, {"x":true}], "x":false } "#, FragmentKind::Object, 3),
            (br#"[{}, [], [1, [2]], "[]{}"]"#, FragmentKind::Array, 3),
        ];
        for &(bytes, kind, depth) in cases {
            let fragment = validate(bytes, EncodeLimits::default()).unwrap();
            assert_eq!(fragment.kind(), kind);
            assert_eq!(fragment.depth(), depth);
            assert_eq!(fragment.as_bytes(), bytes);
            assert_eq!(fragment.as_bytes().as_ptr(), bytes.as_ptr());
        }
    }

    #[test]
    fn only_the_four_json_whitespace_bytes_are_ignored() {
        validate(b" \t\n\r{\t\"\"\r:\n[ \rtrue\t,\nfalse\r ]\t}\r\n\t ", EncodeLimits::default()).unwrap();
        for &byte in &[0, 0x0b, 0x0c, 0x1f, 0x7f] {
            assert_error(&[byte, b'0'], EncodeLimits::default(), ErrorCode::InvalidSyntax, 0);
            assert_error(&[b'n', b'u', b'l', b'l', byte], EncodeLimits::default(), ErrorCode::TrailingContent, 4);
        }
        for text in ["\u{feff}null", "\u{a0}null", "\u{2028}null"] {
            assert_error(text.as_bytes(), EncodeLimits::default(), ErrorCode::InvalidSyntax, 0);
        }
        assert_error("null\u{a0}".as_bytes(), EncodeLimits::default(), ErrorCode::TrailingContent, 4);
        // These codepoints are legal string contents, even though they are not
        // JSON token whitespace. No Unicode normalization is performed.
        validate("\"\u{feff}\u{a0}\u{2028}\u{2029}\"".as_bytes(), EncodeLimits::default()).unwrap();
    }

    #[test]
    fn incomplete_values_report_eof_at_the_input_end() {
        let cases: &[&[u8]] = &[
            b"", b" \t\r\n", b"n", b"nu", b"nul", b"t", b"tru", b"fals",
            b"-", b"1.", b"1e", b"1e+", b"1e-", b"-0.1E-",
            b"\"", b"\"abc", b"\"\\", b"\"\\u", b"\"\\u0", b"\"\\u00", b"\"\\u000",
            b"\"\\uD800", b"\"\\uD800\\", b"\"\\uD800\\u", b"\"\\uD800\\uDC0",
            b"[", b"[ ", b"[0", b"[0,", b"[[]", b"{", b"{\"a\"", b"{\"a\":",
            b"{\"a\":0", b"{\"a\":0,", b"{\"a\":[{}]",
        ];
        for &bytes in cases {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::UnexpectedEof, bytes.len());
        }
    }

    #[test]
    fn malformed_container_grammar_has_exact_offsets() {
        let cases: &[(&[u8], usize)] = &[
            (b"]", 0), (b"}", 0), (b"[}", 1), (b"{]", 1),
            (b"[,0]", 1), (b"[0,]", 3), (b"[0,,1]", 3), (b"[0 1]", 3),
            (b"[true false]", 6), (b"[true:1]", 5), (b"[[]}", 3),
            (b"{a:0}", 1), (b"{0:0}", 1), (b"{'a':0}", 1), (b"{,}", 1),
            (br#"{"a" 0}"#, 5), (br#"{"a",0}"#, 4), (br#"{"a"::0}"#, 5),
            (br#"{"a":}"#, 5), (br#"{"a":0,}"#, 7), (br#"{"a":0 "b":1}"#, 7),
            (br#"{"a":[]]"#, 7), (br#"{"a":0,,"b":1}"#, 7),
            (b"// comment\nnull", 0), (b"/* comment */null", 0),
        ];
        for &(bytes, offset) in cases {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::InvalidSyntax, offset);
        }
    }

    #[test]
    fn complete_roots_reject_second_values_and_trailing_garbage() {
        let cases: &[(&[u8], usize)] = &[
            (b"null true", 5), (b"truefalse", 4), (b"nullx", 4), (b"0 1", 2),
            (b"{}[]", 2), (b"[]{}", 2), (b"\"a\"\"b\"", 3),
            (b"null,", 4), (b"null]", 4), (b"null/*x*/", 4), (b" \n[] \t x", 7),
        ];
        for &(bytes, offset) in cases {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::TrailingContent, offset);
        }
    }

    #[test]
    fn literal_spelling_is_case_sensitive_and_exact() {
        for &(bytes, offset) in &[
            (&b"Null"[..], 0), (&b"TRUE"[..], 0), (&b"False"[..], 0),
            (&b"nulL"[..], 3), (&b"trxe"[..], 2), (&b"falsE"[..], 4),
            (&b"[truX]"[..], 4),
        ] {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::InvalidSyntax, offset);
        }
    }

    #[test]
    fn number_vectors_preserve_arbitrary_precision_and_exponents() {
        for &bytes in &[
            &b"0"[..], &b"-0"[..], &b"12"[..], &b"-123"[..], &b"0.0"[..],
            &b"-0.00"[..], &b"1e0"[..], &b"1E+0"[..], &b"1e-0009"[..],
            &b"-12.3400E+123456789012345678901234567890"[..],
            &b"18446744073709551616000000000000000000000000000"[..],
            &b"-170141183460469231731687303715884105729"[..],
            &b"1e9999999999999999999999999999999999999999999999"[..],
        ] {
            let fragment = validate(bytes, EncodeLimits::default()).unwrap();
            assert_eq!(fragment.kind(), FragmentKind::Number);
            assert_eq!(fragment.as_bytes(), bytes);
            assert_eq!(fragment.depth(), 0);
        }
        // Exercise independent optional sign, fraction and exponent choices.
        for sign in ["", "-"] {
            for integer in ["0", "1", "9876543210"] {
                for fraction in ["", ".0", ".00012"] {
                    for exponent in ["", "e0", "E+0", "e-012", "E999"] {
                        let text = alloc::format!("{}{}{}{}", sign, integer, fraction, exponent);
                        validate(text.as_bytes(), EncodeLimits::default()).unwrap();
                        let array = alloc::format!("[{},{}]", text, text);
                        validate(array.as_bytes(), EncodeLimits::default()).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_number_grammar_identifies_the_first_bad_byte() {
        let cases: &[(&[u8], usize)] = &[
            (b"+1", 0), (b".1", 0), (b"NaN", 0), (b"Infinity", 0),
            (b"-Infinity", 1), (b"--1", 1), (b"-+1", 1), (b"-x", 1),
            (b"00", 1), (b"01", 1), (b"-01", 2), (b"0x10", 1),
            (b"1..0", 2), (b"1.e0", 2), (b"1e.0", 2), (b"1e+-2", 3),
            (b"1e--2", 3), (b"1e2e3", 3), (b"1_000", 1), (b"12abc", 2),
            (b"[1.]", 3), (b"[1e+]", 4), (b"[+0]", 1), (b"[01]", 2),
            (b"[1:2]", 2), (b"1\"x\"", 1), (b"1\x0b", 1),
        ];
        for &(bytes, offset) in cases {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::InvalidNumber, offset);
        }
    }

    #[test]
    fn string_escapes_are_legal_and_unescaped_controls_are_rejected() {
        validate(br#""\"\\\/\b\f\n\r\t\u0000\u001f""#, EncodeLimits::default()).unwrap();
        for byte in 0u8..=0x7f {
            let bytes = [b'"', byte, b'"'];
            match byte {
                0..=0x1f => assert_error(&bytes, EncodeLimits::default(), ErrorCode::ControlCharInString, 1),
                b'"' => assert_error(&bytes, EncodeLimits::default(), ErrorCode::TrailingContent, 2),
                b'\\' => assert_error(&bytes, EncodeLimits::default(), ErrorCode::UnexpectedEof, 3),
                _ => { validate(&bytes, EncodeLimits::default()).unwrap(); }
            }
        }
        for escape in [b'x', b'v', b'0', b'\'', b'U', b' ', b'\n'] {
            assert_error(&[b'"', b'\\', escape, b'"'], EncodeLimits::default(), ErrorCode::InvalidEscape, 2);
        }
        for &(bytes, offset) in &[
            (&b"\"\\uZ000\""[..], 3), (&b"\"\\u0x00\""[..], 4),
            (&b"\"\\u00G0\""[..], 5), (&b"\"\\u000g\""[..], 6),
            (&b"\"\\u00\""[..], 5),
        ] {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::InvalidEscape, offset);
        }
    }

    #[test]
    fn utf8_validation_rejects_invalid_scalars_and_truncated_sequences() {
        let cases: &[(&[u8], usize)] = &[
            (&[0xff], 0), (&[b'"', 0x80, b'"'], 1),
            (&[b'"', 0xc0, 0xaf, b'"'], 1),
            (&[b'"', 0xe0, 0x80, 0x80, b'"'], 1),
            (&[b'"', 0xed, 0xa0, 0x80, b'"'], 1),
            (&[b'"', 0xf4, 0x90, 0x80, 0x80, b'"'], 1),
            (&[b'"', 0xf5, 0x80, 0x80, 0x80, b'"'], 1),
            (&[b'"', 0xc2], 1), (&[b'"', 0xe2, 0x82], 1),
            (&[b'"', 0xf0, 0x9f, 0x98], 1), (&[b'n', b'u', b'l', b'l', 0xff], 4),
        ];
        for &(bytes, offset) in cases {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::InvalidUtf8, offset);
        }
        for byte in 0x80u8..=0xff {
            assert_error(&[b'"', byte, b'"'], EncodeLimits::default(), ErrorCode::InvalidUtf8, 1);
        }
        for scalar in ['\u{7f}', '\u{80}', '\u{7ff}', '\u{800}', '\u{d7ff}', '\u{e000}', '\u{ffff}', '\u{10000}', '\u{10ffff}'] {
            let text = alloc::format!("\"{}\"", scalar);
            let limits = EncodeLimits { max_string_bytes: scalar.len_utf8() as u32, ..EncodeLimits::default() };
            validate(text.as_bytes(), limits).unwrap();
        }
    }

    #[test]
    fn surrogate_pairs_cover_both_extremes_and_reject_unpaired_units() {
        for &bytes in &[
            &br#""\uD800\uDC00""#[..], &br#""\ud83d\ude00""#[..],
            &br#""\uDBFF\uDFFF""#[..], &br#""\uD800\udfff\uDBFF\uDC00""#[..],
        ] {
            validate(bytes, EncodeLimits::default()).unwrap();
        }
        let cases: &[(&[u8], usize)] = &[
            (br#""\uD800""#, 1), (br#""\uDBFFx""#, 1),
            (br#""\uDC00""#, 1), (br#""\uDFFF""#, 1),
            (br#""\uD800\n""#, 1), (br#""\uD800\u0000""#, 7),
            (br#""\uD800\uD800""#, 7), (br#""\uD800\uE000""#, 7),
            (br#""\uD800\uDC00\uDC00""#, 13),
            (br#"{"\uDC00":0}"#, 2),
        ];
        for &(bytes, offset) in cases {
            assert_error(bytes, EncodeLimits::default(), ErrorCode::LoneSurrogate, offset);
        }
        assert_error(br#""\uD800\uDC0X""#, EncodeLimits::default(), ErrorCode::InvalidEscape, 12);
    }

    #[test]
    fn decoded_byte_limits_use_codepoint_width_instead_of_escape_spelling() {
        let cases: &[(&[u8], u32)] = &[
            (br#""""#, 0), (br#""\u0000""#, 1), (br#""\u007f""#, 1),
            (br#""\u0080""#, 2), (br#""\u07FF""#, 2), (br#""\u0800""#, 3),
            (br#""\uD7FF""#, 3), (br#""\uE000""#, 3), (br#""\uFFFF""#, 3),
            (br#""\uD800\uDC00""#, 4), (br#""\uDBFF\uDFFF""#, 4),
            (br#""\"\\\/\b\f\n\r\t""#, 8),
            (br#""a\u00E9\u6C34\uD83D\uDE00""#, 10),
            ("\"aé水😀\"".as_bytes(), 10),
        ];
        for &(bytes, width) in cases {
            let limits = EncodeLimits { max_string_bytes: width, ..EncodeLimits::default() };
            validate(bytes, limits).unwrap();
            if width != 0 {
                let error = validate(bytes, EncodeLimits { max_string_bytes: width - 1, ..limits }).unwrap_err();
                assert_eq!(error.code, ErrorCode::BudgetExhausted);
                assert_eq!(error.source, ErrorSource::Encode);
                assert!((error.byte_offset as usize) < bytes.len());
            }
        }
        for limit in 0..10 {
            for &bytes in &["\"aé水😀\"".as_bytes(), &br#""a\u00E9\u6C34\uD83D\uDE00""#[..]] {
                let limits = EncodeLimits { max_string_bytes: limit, ..EncodeLimits::default() };
                assert_eq!(validate(bytes, limits).unwrap_err().code, ErrorCode::BudgetExhausted);
            }
        }
    }

    #[test]
    fn escaped_controls_each_decode_to_one_byte_and_keys_share_string_limits() {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let one = EncodeLimits { max_string_bytes: 1, ..EncodeLimits::default() };
        for byte in 0u8..=0x1f {
            let bytes = [b'"', b'\\', b'u', b'0', b'0', HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)], b'"'];
            validate(&bytes, one).unwrap();
            assert_error(&bytes, EncodeLimits { max_string_bytes: 0, ..one }, ErrorCode::BudgetExhausted, 1);
        }
        validate(br#"{"\u0061":0}"#, one).unwrap();
        assert_error(br#"{"\u0061\u0062":0}"#, one, ErrorCode::BudgetExhausted, 8);
        validate(br#"{"\uD83D\uDE00":0}"#, EncodeLimits { max_string_bytes: 4, ..one }).unwrap();
        assert_error(br#"{"\uD83D\uDE00":0}"#, EncodeLimits { max_string_bytes: 3, ..one }, ErrorCode::BudgetExhausted, 2);
        let zero = EncodeLimits { max_string_bytes: 0, ..one };
        validate(br#"{"":""}"#, zero).unwrap();
        assert_error(br#"{"a":0}"#, zero, ErrorCode::BudgetExhausted, 2);
    }

    #[test]
    fn whole_input_budget_includes_all_whitespace_and_is_inclusive() {
        for &bytes in &[&b"0"[..], &b" null \r\n"[..], &br#""\u0000""#[..], &b"[{},[]]"[..]] {
            let limits = EncodeLimits { max_output_bytes: bytes.len(), ..EncodeLimits::default() };
            validate(bytes, limits).unwrap();
            assert_error(bytes, EncodeLimits { max_output_bytes: bytes.len() - 1, ..limits }, ErrorCode::PayloadTooLarge, 0);
        }
        let limits = EncodeLimits { max_output_bytes: 0, ..EncodeLimits::default() };
        assert_error(b"", limits, ErrorCode::UnexpectedEof, 0);
        assert_error(b"0", limits, ErrorCode::PayloadTooLarge, 0);
        // Size is checked before UTF-8 or grammar work.
        assert_error(&[0xff], limits, ErrorCode::PayloadTooLarge, 0);
    }

    #[test]
    fn number_length_counts_sign_fraction_and_exponent_but_not_whitespace() {
        for &bytes in &[&b"0"[..], &b"-0"[..], &b"12.30E-04"[..]] {
            let limits = EncodeLimits { max_number_bytes: bytes.len() as u32, ..EncodeLimits::default() };
            validate(bytes, limits).unwrap();
            assert_error(bytes, EncodeLimits { max_number_bytes: limits.max_number_bytes - 1, ..limits }, ErrorCode::BudgetExhausted, bytes.len() - 1);
        }
        let limits = EncodeLimits { max_number_bytes: 1, ..EncodeLimits::default() };
        validate(b" \n0\r\t ", limits).unwrap();
        assert_error(b"[12]", limits, ErrorCode::BudgetExhausted, 2);
        let zero = EncodeLimits { max_number_bytes: 0, ..limits };
        assert_error(b"0", zero, ErrorCode::BudgetExhausted, 0);
        validate(b"null", zero).unwrap();
        validate(br#""123""#, zero).unwrap();
        let long = alloc::vec![b'9'; 1025];
        assert_error(&long, EncodeLimits::default(), ErrorCode::BudgetExhausted, 1024);
        validate(&long, EncodeLimits { max_number_bytes: 1025, ..limits }).unwrap();
    }

    #[test]
    fn depth_limits_are_relative_inclusive_and_independent_of_sibling_count() {
        let zero = EncodeLimits { max_depth: 0, ..EncodeLimits::default() };
        for &bytes in &[&b"null"[..], &b"false"[..], &b"0"[..], &br#""x""#[..]] {
            assert_eq!(validate(bytes, zero).unwrap().depth(), 0);
        }
        assert_error(b"[]", zero, ErrorCode::DepthLimitExceeded, 0);
        assert_error(b"{}", zero, ErrorCode::DepthLimitExceeded, 0);
        let two = EncodeLimits { max_depth: 2, ..zero };
        assert_eq!(validate(b"[[],{},[]]", two).unwrap().depth(), 2);
        assert_eq!(validate(br#"{"a":[0],"b":{}}"#, two).unwrap().depth(), 2);
        assert_error(b"[[[]]]", two, ErrorCode::DepthLimitExceeded, 2);
        assert_error(br#"{"a":{"b":[]}}"#, two, ErrorCode::DepthLimitExceeded, 10);
    }

    #[test]
    fn full_u16_depth_uses_heap_frames_without_host_stack_recursion() {
        let depth = usize::from(u16::MAX);
        let mut bytes = alloc::vec![b'['; depth];
        bytes.resize(depth * 2, b']');
        let limits = EncodeLimits { max_depth: u16::MAX, max_container_items: 1, ..EncodeLimits::default() };
        assert_eq!(validate(&bytes, limits).unwrap().depth(), u16::MAX);
        // One more opening delimiter must fail before allocating another frame.
        bytes.insert(depth, b'[');
        bytes.push(b']');
        assert_error(&bytes, limits, ErrorCode::DepthLimitExceeded, depth);
    }

    #[test]
    fn container_counts_are_per_container_and_duplicate_keys_are_legal() {
        let limits = EncodeLimits { max_container_items: 1, ..EncodeLimits::default() };
        for &bytes in &[&b"[]"[..], &b"{}"[..], &b"[[[]]]"[..], &br#"{"a":[{"b":0}]}"#[..]] {
            validate(bytes, limits).unwrap();
        }
        assert_error(b"[0,1]", limits, ErrorCode::BudgetExhausted, 3);
        assert_error(br#"{"a":0,"a":1}"#, limits, ErrorCode::BudgetExhausted, 7);
        assert_error(br#"[{"a":0,"b":1}]"#, limits, ErrorCode::BudgetExhausted, 8);
        assert_error(br#"{"a":[0,1]}"#, limits, ErrorCode::BudgetExhausted, 8);
        let two = EncodeLimits { max_container_items: 2, ..limits };
        validate(br#"{"a":0,"a":1}"#, two).unwrap();
        validate(b"[[0,1],[2,3]]", two).unwrap();
        let zero = EncodeLimits { max_container_items: 0, ..limits };
        validate(b"[]", zero).unwrap();
        validate(b"{}", zero).unwrap();
        assert_error(b"[null]", zero, ErrorCode::BudgetExhausted, 1);
        assert_error(br#"{"":null}"#, zero, ErrorCode::BudgetExhausted, 1);
    }

    #[test]
    fn counter_overflow_boundaries_fail_without_wrapping_or_advancing() {
        // Seed private counters so u32 boundaries need no multi-gigabyte input.
        let limits = EncodeLimits { max_container_items: u32::MAX, max_string_bytes: u32::MAX, ..EncodeLimits::default() };
        let mut scan = parser(b"[", limits);
        scan.open(Phase::ArrayFirst).unwrap();
        scan.frames[0].count = u32::MAX - 1;
        scan.count(0).unwrap();
        assert_eq!(scan.frames[0].count, u32::MAX);
        let error = scan.count(0).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 1);
        assert_eq!(scan.frames[0].count, u32::MAX);
        assert_eq!(scan.string_bytes(u32::MAX - 4, 4, 0).unwrap(), u32::MAX);
        for width in [1, 4] {
            let error = scan.string_bytes(u32::MAX, width, 1).unwrap_err();
            assert_eq!(error.code, ErrorCode::BudgetExhausted);
            assert_eq!(error.source, ErrorSource::Encode);
            assert_eq!(error.byte_offset, 1);
        }
    }

    #[test]
    fn every_proper_prefix_of_a_container_document_is_rejected() {
        let bytes = br#"{"a":[true,false,null,-1.25e+02,"\uD83D\uDE00"],"b":{}}"#;
        for end in 0..bytes.len() {
            let error = validate(&bytes[..end], EncodeLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnexpectedEof, "prefix length: {}", end);
            assert_eq!(error.byte_offset as usize, end);
        }
        validate(bytes, EncodeLimits::default()).unwrap();
    }

    #[test]
    fn emit_preserves_root_bytes_and_excludes_a_preexisting_sink_prefix() {
        let bytes = b" \n{\"x\" : [1e+02, \"\\u0061\"], \"x\":null}\t";
        let fragment = validate(bytes, EncodeLimits::default()).unwrap();
        let mut sink = VecSink::new(1024);
        sink.write_bytes(b"prefix:").unwrap();
        let limits = EncodeLimits { max_output_bytes: bytes.len(), ..EncodeLimits::default() };
        let mut writer = JsonWriter::new(&mut sink, limits);
        emit(&mut writer, fragment).unwrap();
        assert_eq!(writer.finish().unwrap(), bytes.len());
        assert_eq!(&sink.as_slice()[..7], b"prefix:");
        assert_eq!(&sink.as_slice()[7..], bytes);
    }

    #[test]
    fn emit_participates_in_parent_syntax_and_counts_one_surrounding_value() {
        let fragment = validate(b" [0,1] ", EncodeLimits::default()).unwrap();
        let mut sink = VecSink::new(128);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        writer.begin_array().unwrap();
        emit(&mut writer, fragment).unwrap();
        emit(&mut writer, fragment).unwrap();
        writer.begin_object().unwrap();
        writer.key("raw").unwrap();
        emit(&mut writer, fragment).unwrap();
        writer.end_object().unwrap();
        writer.end_array().unwrap();
        writer.finish().unwrap();
        assert_eq!(sink.as_slice(), b"[ [0,1] , [0,1] ,{\"raw\": [0,1] }]");
        let one = validate(b"null", EncodeLimits::default()).unwrap();
        let limits = EncodeLimits { max_container_items: 1, ..EncodeLimits::default() };
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        emit(&mut writer, one).unwrap();
        assert_eq!(emit(&mut writer, one).unwrap_err().code, ErrorCode::BudgetExhausted);
        assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(sink.as_slice(), b"[null");
    }

    #[test]
    fn emit_output_budget_includes_the_parent_comma_and_closing_delimiter() {
        let fragment = validate(b" null ", EncodeLimits::default()).unwrap();
        for limit in [8, 9, 10] {
            let mut sink = VecSink::new(64);
            let limits = EncodeLimits { max_output_bytes: limit, ..EncodeLimits::default() };
            let mut writer = JsonWriter::new(&mut sink, limits);
            writer.begin_array().unwrap();
            writer.u64(0).unwrap();
            if limit == 8 {
                assert_eq!(emit(&mut writer, fragment).unwrap_err().code, ErrorCode::PayloadTooLarge);
                assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
                assert_eq!(sink.as_slice(), b"[0");
            } else {
                emit(&mut writer, fragment).unwrap();
                if limit == 9 {
                    assert_eq!(writer.end_array().unwrap_err().code, ErrorCode::PayloadTooLarge);
                    assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
                    assert_eq!(sink.as_slice(), b"[0, null ");
                } else {
                    writer.end_array().unwrap();
                    assert_eq!(writer.finish().unwrap(), 10);
                    assert_eq!(sink.as_slice(), b"[0, null ]");
                }
            }
        }
    }

    #[test]
    fn emit_revalidates_every_resource_under_the_writers_limits() {
        let loose = EncodeLimits::default();
        let cases: &[(&[u8], EncodeLimits, ErrorCode)] = &[
            (b"[[]]", EncodeLimits { max_depth: 1, ..loose }, ErrorCode::DepthLimitExceeded),
            (b"[0,1]", EncodeLimits { max_container_items: 1, ..loose }, ErrorCode::BudgetExhausted),
            (br#"{"a":0,"b":1}"#, EncodeLimits { max_container_items: 1, ..loose }, ErrorCode::BudgetExhausted),
            (br#"["\uD83D\uDE00"]"#, EncodeLimits { max_string_bytes: 3, ..loose }, ErrorCode::BudgetExhausted),
            (br#"{"\u0061":0}"#, EncodeLimits { max_string_bytes: 0, ..loose }, ErrorCode::BudgetExhausted),
            (b"[123]", EncodeLimits { max_number_bytes: 2, ..loose }, ErrorCode::BudgetExhausted),
            (b" null ", EncodeLimits { max_output_bytes: 5, ..loose }, ErrorCode::PayloadTooLarge),
        ];
        for &(bytes, limits, code) in cases {
            let fragment = validate(bytes, loose).unwrap();
            let expected = validate(bytes, limits).unwrap_err();
            assert_eq!(expected.code, code);
            let mut sink = VecSink::new(128);
            let mut writer = JsonWriter::new(&mut sink, limits);
            assert_eq!(emit(&mut writer, fragment).unwrap_err(), expected);
            assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
            assert!(sink.is_empty());
        }
    }

    #[test]
    fn emit_rebases_fragment_depth_and_poisoning_cannot_be_bypassed() {
        let fragment = validate(b"[{}]", EncodeLimits::default()).unwrap();
        let scalar = validate(b"null", EncodeLimits::default()).unwrap();
        let limits = EncodeLimits { max_depth: 2, ..EncodeLimits::default() };
        let mut sink = VecSink::new(128);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        assert_eq!(emit(&mut writer, fragment).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        assert_eq!(emit(&mut writer, scalar).unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(sink.as_slice(), b"[");
        let mut sink = VecSink::new(128);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        writer.begin_array().unwrap();
        emit(&mut writer, scalar).unwrap();
        writer.end_array().unwrap();
        writer.end_array().unwrap();
        writer.finish().unwrap();
        assert_eq!(sink.as_slice(), b"[[null]]");
    }

    #[test]
    fn emit_rejects_missing_object_keys_and_a_second_root() {
        let fragment = validate(b"null", EncodeLimits::default()).unwrap();
        for object in [false, true] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            if object { writer.begin_object().unwrap(); } else { emit(&mut writer, fragment).unwrap(); }
            assert_eq!(emit(&mut writer, fragment).unwrap_err().code, ErrorCode::InvalidSyntax);
            assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
            assert_eq!(sink.as_slice(), if object { &b"{"[..] } else { &b"null"[..] });
        }
    }

    #[test]
    fn emit_revalidates_bytes_instead_of_trusting_internal_fragment_metadata() {
        // Public users cannot make this record. Internal callers still receive
        // the same strict byte validation and exact parser diagnostics.
        let malformed = RawFragment { bytes: b"[0,]", kind: FragmentKind::Null, depth: 0 };
        let expected = validate(malformed.as_bytes(), EncodeLimits::default()).unwrap_err();
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        assert_eq!(emit(&mut writer, malformed).unwrap_err(), expected);
        assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
        assert!(sink.is_empty());
    }

    struct FailingSink {
        bytes: Vec<u8>,
        fail_reserve: bool,
    }

    fn sink_error() -> Error {
        Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 17, "injected sink error")
            .with_schema(23, 29)
    }

    impl ByteSink for FailingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            if self.fail_reserve {
                return Err(sink_error());
            }
            ByteSink::reserve(&mut self.bytes, additional)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            ByteSink::write_bytes(&mut self.bytes, &bytes[..core::cmp::min(2, bytes.len())])?;
            Err(sink_error())
        }

        fn len(&self) -> usize {
            self.bytes.len()
        }
    }

    #[test]
    fn emit_preserves_external_error_metadata_and_visible_partial_output() {
        let fragment = validate(b"null", EncodeLimits::default()).unwrap();
        for fail_reserve in [false, true] {
            let mut sink = FailingSink { bytes: Vec::new(), fail_reserve };
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            assert_eq!(emit(&mut writer, fragment).unwrap_err(), sink_error());
            assert_eq!(emit(&mut writer, fragment).unwrap_err().code, ErrorCode::InternalError);
            assert_eq!(writer.finish().unwrap_err().code, ErrorCode::InternalError);
            assert_eq!(sink.bytes.as_slice(), if fail_reserve { &b""[..] } else { &b"nu"[..] });
        }
    }
}
