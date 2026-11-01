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
