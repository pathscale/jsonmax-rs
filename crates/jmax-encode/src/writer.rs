//! Syntax-aware, bounded JSON emission. A failed operation permanently poisons
//! the writer; callers must discard any output already published to the sink.

use super::*;
use core::fmt::Write as _;

impl<'a> JsonWriter<'a> {
    /// Starts a message whose output budget excludes any existing sink prefix.
    pub fn new(sink: &'a mut dyn ByteSink, limits: EncodeLimits) -> Self {
        Self {
            sink: LimitedSink::new(sink, limits.max_output_bytes),
            limits,
            frames: Vec::new(),
            root_written: false,
            poisoned: false,
        }
    }

    pub fn begin_object(&mut self) -> Result<()> {
        self.operation(|writer| writer.begin(WriteContainer::Object))
    }

    pub fn end_object(&mut self) -> Result<()> {
        self.operation(|writer| writer.end(WriteContainer::Object))
    }

    pub fn begin_array(&mut self) -> Result<()> {
        self.operation(|writer| writer.begin(WriteContainer::Array))
    }

    pub fn end_array(&mut self) -> Result<()> {
        self.operation(|writer| writer.end(WriteContainer::Array))
    }

    /// Emits one escaped property name and its colon. Duplicate names are legal.
    pub fn key(&mut self, key: &str) -> Result<()> {
        self.operation(|writer| {
            let frame = writer.frames.last().ok_or_else(|| {
                encode_error(ErrorCode::InvalidSyntax, "key outside an object")
            })?;
            if frame.kind != WriteContainer::Object || !frame.expecting_key {
                return Err(encode_error(ErrorCode::InvalidSyntax, "object is not expecting a key"));
            }
            if frame.count >= writer.limits.max_container_items {
                return Err(encode_error(ErrorCode::BudgetExhausted, "object property limit exceeded"));
            }
            let comma = frame.count != 0;
            writer.check_string(key)?;
            let size = checked_output_len(escaped_string_len(key)?, 1, usize::MAX)?;
            let size = checked_output_len(size, usize::from(comma), usize::MAX)?;
            writer.sink.reserve(size)?;
            writer.write_comma(comma)?;
            writer.write_string(key)?;
            writer.sink.write_byte(b':')?;
            // The count was checked before any output, so this cannot overflow.
            let frame = writer.frames.last_mut().ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "object frame disappeared during key emission")
            })?;
            frame.count += 1;
            frame.expecting_key = false;
            Ok(())
        })
    }

    pub fn null(&mut self) -> Result<()> {
        self.operation(|writer| writer.token(b"null"))
    }

    pub fn boolean(&mut self, value: bool) -> Result<()> {
        self.operation(|writer| writer.token(if value { b"true" } else { b"false" }))
    }

    pub fn string(&mut self, value: &str) -> Result<()> {
        self.operation(|writer| {
            writer.check_string(value)?;
            let comma = writer.reserve_value(escaped_string_len(value)?)?;
            writer.write_comma(comma)?;
            writer.write_string(value)?;
            writer.accept_value();
            Ok(())
        })
    }

    pub fn u64(&mut self, value: u64) -> Result<()> {
        self.operation(|writer| {
            let mut digits = [0; 39];
            let start = unsigned_digits(u128::from(value), &mut digits);
            writer.number_token(&digits[start..])
        })
    }

    pub fn i64(&mut self, value: i64) -> Result<()> {
        self.operation(|writer| {
            let mut digits = [0; 39];
            let start = unsigned_digits(u128::from(value.unsigned_abs()), &mut digits);
            let mut text = [0; 20];
            let sign = usize::from(value < 0);
            let len = digits.len() - start + sign;
            if sign != 0 {
                text[0] = b'-';
            }
            text[sign..len].copy_from_slice(&digits[start..]);
            writer.number_token(&text[..len])
        })
    }

    /// Validates and emits one exact JSON number lexeme without conversion or
    /// allocation. Whitespace, quoted numbers and multiple tokens are rejected.
    pub fn number_lexeme(&mut self, bytes: &[u8]) -> Result<()> {
        self.operation(|writer| {
            writer.value_comma()?;
            writer.check_number_len(bytes.len())?;
            if !matches!(bytes.first(), Some(b'-' | b'0'..=b'9'))
                || !bytes.last().is_some_and(u8::is_ascii_digit)
            {
                return Err(encode_error(ErrorCode::InvalidNumber, "expected a JSON number lexeme"));
            }
            // Starting with a number keeps fragment validation on its scalar
            // path, which uses no heap storage and checks the complete grammar.
            let validated = super::raw::validate(bytes, writer.limits)?;
            if validated.kind() != FragmentKind::Number {
                return Err(encode_error(ErrorCode::InvalidNumber, "expected a JSON number lexeme"));
            }
            writer.number_token(validated.as_bytes())
        })
    }

    /// Uses core's shortest-roundtrip formatting, choosing the shorter notation.
    pub fn f64(&mut self, value: f64) -> Result<()> {
        self.operation(|writer| {
            if !value.is_finite() {
                return Err(encode_error(ErrorCode::InvalidNumber, "nonfinite JSON number"));
            }
            if value == 0.0 {
                return writer.number_token(if value.is_sign_negative() { b"-0" } else { b"0" });
            }
            let mut ordinary = jmax_number::NumberText::new();
            let mut scientific = jmax_number::NumberText::new();
            write!(&mut ordinary, "{}", value).map_err(|_| {
                encode_error(ErrorCode::InternalError, "float formatting exceeded scratch storage")
            })?;
            write!(&mut scientific, "{:e}", value).map_err(|_| {
                encode_error(ErrorCode::InternalError, "float formatting exceeded scratch storage")
            })?;
            let text = if scientific.len() < ordinary.len() {
                scientific.as_bytes()
            } else {
                ordinary.as_bytes()
            };
            writer.number_token(text)
        })
    }

    /// Emits exact fixed-scale decimal text inside JSON quotes, without rounding.
    pub fn decimal(&mut self, value: Decimal128) -> Result<()> {
        self.operation(|writer| {
            let mut text = [0; 43];
            let len = decimal_text(value, &mut text)?;
            let contents_len = len - 2;
            writer.check_number_len(contents_len)?;
            if contents_len > writer.limits.max_string_bytes as usize {
                return Err(encode_error(ErrorCode::BudgetExhausted, "decimal string limit exceeded"));
            }
            writer.token(&text[..len])
        })
    }

    /// Revalidates the exact borrowed bytes under this message's resource limits.
    pub fn raw(&mut self, value: RawFragment<'_>) -> Result<()> {
        self.operation(|writer| {
            // Reject an invalid surrounding syntax state before parsing input.
            writer.value_comma()?;
            let mut limits = writer.limits;
            let depth = u16::try_from(writer.frames.len()).map_err(|_| {
                encode_error(ErrorCode::DepthLimitExceeded, "writer depth limit exceeded")
            })?;
            limits.max_depth = limits.max_depth.checked_sub(depth).ok_or_else(|| {
                encode_error(ErrorCode::DepthLimitExceeded, "writer depth limit exceeded")
            })?;
            // The parser checks nested counts, decoded string lengths, number
            // lengths and actual depth. A previously validated fragment may have
            // been accepted with much looser limits than these.
            let validated = super::raw::validate(value.as_bytes(), limits)?;
            writer.token(validated.as_bytes())
        })
    }

    /// Completes a single root and reports only bytes appended by this writer.
    pub fn finish(self) -> Result<usize> {
        if self.poisoned || self.sink.is_poisoned() {
            return Err(encode_error(ErrorCode::InternalError, "JSON writer is poisoned"));
        }
        if !self.root_written || !self.frames.is_empty() {
            return Err(encode_error(ErrorCode::UnexpectedEof, "incomplete JSON message"));
        }
        Ok(self.sink.appended_len())
    }

    /// Centralizes poisoning, including errors which occurred before any write.
    fn operation(&mut self, action: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if self.poisoned || self.sink.is_poisoned() {
            self.poisoned = true;
            return Err(encode_error(ErrorCode::InternalError, "JSON writer is poisoned"));
        }
        let result = action(self);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn begin(&mut self, kind: WriteContainer) -> Result<()> {
        self.value_comma()?;
        if self.frames.len() >= usize::from(self.limits.max_depth) {
            return Err(encode_error(ErrorCode::DepthLimitExceeded, "writer depth limit exceeded"));
        }
        self.frames.try_reserve(1).map_err(|_| {
            encode_error(ErrorCode::BudgetExhausted, "writer frame allocation failed")
        })?;
        self.token(match kind {
            WriteContainer::Object => b"{",
            WriteContainer::Array => b"[",
        })?;
        self.frames.push(WriteFrame {
            kind,
            count: 0,
            expecting_key: kind == WriteContainer::Object,
        });
        Ok(())
    }

    fn end(&mut self, kind: WriteContainer) -> Result<()> {
        let frame = self.frames.last().ok_or_else(|| {
            encode_error(ErrorCode::InvalidSyntax, "closing delimiter without an open container")
        })?;
        if frame.kind != kind {
            return Err(encode_error(ErrorCode::InvalidSyntax, "mismatched closing delimiter"));
        }
        if kind == WriteContainer::Object && !frame.expecting_key {
            return Err(encode_error(ErrorCode::InvalidSyntax, "object key has no value"));
        }
        self.sink.reserve(1)?;
        self.sink.write_byte(match kind {
            WriteContainer::Object => b'}',
            WriteContainer::Array => b']',
        })?;
        self.frames.pop();
        Ok(())
    }

    fn check_string(&self, text: &str) -> Result<()> {
        if text.len() > self.limits.max_string_bytes as usize {
            return Err(encode_error(ErrorCode::BudgetExhausted, "decoded string limit exceeded"));
        }
        Ok(())
    }

    fn check_number_len(&self, len: usize) -> Result<()> {
        if len > self.limits.max_number_bytes as usize {
            return Err(encode_error(ErrorCode::BudgetExhausted, "number text limit exceeded"));
        }
        Ok(())
    }

    fn number_token(&mut self, bytes: &[u8]) -> Result<()> {
        self.check_number_len(bytes.len())?;
        self.token(bytes)
    }

    /// Examines state without committing a root, array item, or object value.
    fn value_comma(&self) -> Result<bool> {
        match self.frames.last() {
            Some(frame) if frame.kind == WriteContainer::Object => {
                if frame.expecting_key {
                    return Err(encode_error(ErrorCode::InvalidSyntax, "object value requires a key"));
                }
                Ok(false)
            }
            Some(frame) => {
                if frame.count >= self.limits.max_container_items {
                    return Err(encode_error(ErrorCode::BudgetExhausted, "array item limit exceeded"));
                }
                Ok(frame.count != 0)
            }
            None if self.root_written => {
                Err(encode_error(ErrorCode::InvalidSyntax, "second JSON root"))
            }
            None => Ok(false),
        }
    }

    fn reserve_value(&mut self, token_len: usize) -> Result<bool> {
        let comma = self.value_comma()?;
        let size = checked_output_len(token_len, usize::from(comma), usize::MAX)?;
        self.sink.reserve(size)?;
        Ok(comma)
    }

    fn write_comma(&mut self, comma: bool) -> Result<()> {
        if comma {
            self.sink.write_byte(b',')?;
        }
        Ok(())
    }

    fn accept_value(&mut self) {
        match self.frames.last_mut() {
            Some(frame) if frame.kind == WriteContainer::Object => frame.expecting_key = true,
            Some(frame) => frame.count += 1,
            None => self.root_written = true,
        }
    }

    fn token(&mut self, bytes: &[u8]) -> Result<()> {
        let comma = self.reserve_value(bytes.len())?;
        self.write_comma(comma)?;
        self.sink.write_bytes(bytes)?;
        self.accept_value();
        Ok(())
    }

    /// The complete quoted token was reserved by the caller. Emit unchanged
    /// UTF-8 runs between escapes, never interpreting a partial UTF-8 sequence.
    fn write_string(&mut self, text: &str) -> Result<()> {
        self.sink.write_byte(b'"')?;
        let bytes = text.as_bytes();
        let mut start = 0;
        let policy = jmax_string::EscapePolicy::default();
        while let Some(relative) = jmax_string::escape::find_escape(&bytes[start..], policy) {
            let index = start + relative;
            if start != index {
                self.sink.write_bytes(&bytes[start..index])?;
            }
            // The default policy matches only ASCII bytes, so the safe run
            // always ends on a UTF-8 boundary even when it contains Unicode.
            let byte = bytes[index];
            let escape: &[u8] = match byte {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                b'\x08' => b"\\b",
                b'\x0c' => b"\\f",
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                _ => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    self.sink.write_bytes(&[
                        b'\\', b'u', b'0', b'0',
                        HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)],
                    ])?;
                    start = index + 1;
                    continue;
                }
            };
            self.sink.write_bytes(escape)?;
            start = index + 1;
        }
        if start != bytes.len() {
            self.sink.write_bytes(&bytes[start..])?;
        }
        self.sink.write_byte(b'"')
    }
}

/// Every u128 fits in 39 decimal digits, including zero as one digit.
fn unsigned_digits(mut value: u128, digits: &mut [u8; 39]) -> usize {
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return start;
        }
    }
}

/// Scale <= 38 requires at most 41 content bytes plus two quotes. Magnitudes
/// use unsigned_abs so the full i128 range is supported without signed overflow.
fn decimal_text(value: Decimal128, text: &mut [u8; 43]) -> Result<usize> {
    if value.scale > 38 {
        return Err(encode_error(ErrorCode::DecimalScaleMismatch, "decimal scale exceeds 38"));
    }
    let mut scratch = [0; 39];
    let start = unsigned_digits(value.coefficient.unsigned_abs(), &mut scratch);
    let digits = &scratch[start..];
    let scale = usize::from(value.scale);
    text[0] = b'"';
    let mut len = 1;
    if value.coefficient < 0 {
        text[len] = b'-';
        len += 1;
    }
    if scale == 0 {
        text[len..len + digits.len()].copy_from_slice(digits);
        len += digits.len();
    } else if scale >= digits.len() {
        text[len] = b'0';
        text[len + 1] = b'.';
        len += 2;
        let zeros = scale - digits.len();
        text[len..len + zeros].fill(b'0');
        len += zeros;
        text[len..len + digits.len()].copy_from_slice(digits);
        len += digits.len();
    } else {
        let split = digits.len() - scale;
        text[len..len + split].copy_from_slice(&digits[..split]);
        len += split;
        text[len] = b'.';
        len += 1;
        text[len..len + scale].copy_from_slice(&digits[split..]);
        len += scale;
    }
    text[len] = b'"';
    Ok(len + 1)
}
