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
        for (index, &byte) in bytes.iter().enumerate() {
            if byte > 0x1f && byte != b'"' && byte != b'\\' {
                continue;
            }
            if start != index {
                self.sink.write_bytes(&bytes[start..index])?;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn render(
        limits: EncodeLimits,
        action: impl FnOnce(&mut JsonWriter<'_>) -> Result<()>,
    ) -> Vec<u8> {
        let mut sink = VecSink::new(usize::MAX);
        let mut writer = JsonWriter::new(&mut sink, limits);
        action(&mut writer).unwrap();
        let len = writer.finish().unwrap();
        assert_eq!(len, sink.len());
        sink.into_vec()
    }

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn assert_poisoned(mut writer: JsonWriter<'_>) {
        assert!(writer.poisoned);
        assert_error(writer.null().unwrap_err(), ErrorCode::InternalError);
        assert_error(writer.finish().unwrap_err(), ErrorCode::InternalError);
    }

    fn external_error() -> Error {
        Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 19, "injected failure")
            .with_schema(23, 29)
    }

    /// Can reject reservation, fail after a byte prefix, or lie about successful
    /// lengths. This exercises writer behavior with arbitrary external sinks.
    struct FaultSink {
        bytes: Vec<u8>,
        reservations: usize,
        writes: usize,
        last_reservation: usize,
        reject_reserve: bool,
        fail_after: Option<usize>,
        dishonest_write: bool,
        dishonest_reserve: bool,
    }

    impl FaultSink {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                reservations: 0,
                writes: 0,
                last_reservation: 0,
                reject_reserve: false,
                fail_after: None,
                dishonest_write: false,
                dishonest_reserve: false,
            }
        }
    }

    impl ByteSink for FaultSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reservations += 1;
            self.last_reservation = additional;
            if self.reject_reserve {
                return Err(external_error());
            }
            self.bytes.try_reserve(additional).map_err(|_| {
                encode_error(ErrorCode::BudgetExhausted, "test allocation failed")
            })?;
            if self.dishonest_reserve {
                self.bytes.push(b'?');
            }
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.writes += 1;
            if self.dishonest_write {
                return Ok(());
            }
            if let Some(remaining) = self.fail_after {
                let accepted = core::cmp::min(remaining, bytes.len());
                ByteSink::write_bytes(&mut self.bytes, &bytes[..accepted])?;
                self.fail_after = Some(remaining - accepted);
                if accepted != bytes.len() {
                    return Err(external_error());
                }
                return Ok(());
            }
            ByteSink::write_bytes(&mut self.bytes, bytes)
        }

        fn len(&self) -> usize {
            self.bytes.len()
        }
    }

    #[test]
    fn constructor_and_drop_do_not_emit_or_reserve() {
        let mut sink = FaultSink::new();
        sink.bytes = b"prefix".to_vec();
        {
            let writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            assert!(!writer.root_written);
            assert!(!writer.poisoned);
            assert!(!writer.sink.is_poisoned());
            assert!(writer.frames.is_empty());
            assert_eq!(writer.sink.len(), 0);
        }
        assert_eq!(sink.bytes, b"prefix");
        assert_eq!((sink.reservations, sink.writes), (0, 0));
    }

    #[test]
    fn nested_objects_arrays_and_duplicate_keys_have_exact_separators() {
        let bytes = render(EncodeLimits::default(), |writer| {
            writer.begin_object()?;
            writer.key("items")?;
            writer.begin_array()?;
            writer.null()?;
            writer.boolean(true)?;
            writer.begin_object()?;
            writer.key("x")?;
            writer.i64(-7)?;
            writer.key("x")?;
            writer.string("again")?;
            writer.end_object()?;
            writer.begin_array()?;
            writer.end_array()?;
            writer.end_array()?;
            writer.key("")?;
            writer.boolean(false)?;
            writer.end_object()
        });
        assert_eq!(bytes, br#"{"items":[null,true,{"x":-7,"x":"again"},[]],"":false}"#);
    }

    #[test]
    fn scalar_roots_and_empty_containers() {
        let limits = EncodeLimits::default();
        assert_eq!(render(limits, |w| w.null()), b"null");
        assert_eq!(render(limits, |w| w.boolean(true)), b"true");
        assert_eq!(render(limits, |w| w.boolean(false)), b"false");
        assert_eq!(render(limits, |w| w.string("")), b"\"\"");
        assert_eq!(render(limits, |w| { w.begin_array()?; w.end_array() }), b"[]");
        assert_eq!(render(limits, |w| { w.begin_object()?; w.end_object() }), b"{}");
    }

    #[test]
    fn integer_extremes_are_exact_minimal_decimal() {
        for (value, expected) in [
            (0, "0"), (1, "1"), (10, "10"),
            (u64::MAX, "18446744073709551615"),
        ] {
            assert_eq!(render(EncodeLimits::default(), |w| w.u64(value)), expected.as_bytes());
        }
        for (value, expected) in [
            (0, "0"), (-1, "-1"), (10, "10"),
            (i64::MIN, "-9223372036854775808"),
            (i64::MAX, "9223372036854775807"),
        ] {
            assert_eq!(render(EncodeLimits::default(), |w| w.i64(value)), expected.as_bytes());
        }
    }

    #[test]
    fn finite_floats_roundtrip_bits_including_subnormals_and_negative_zero() {
        for value in [
            0.0, -0.0, 1.0, -1.0, 0.1, 1.2345678901234567,
            f64::MIN_POSITIVE, f64::from_bits(1), f64::from_bits(0x000f_ffff_ffff_ffff),
            f64::MAX, f64::MIN, 1e20, 1e-20,
        ] {
            let bytes = render(EncodeLimits::default(), |w| w.f64(value));
            let text = core::str::from_utf8(&bytes).unwrap();
            assert_eq!(text.parse::<f64>().unwrap().to_bits(), value.to_bits());
            assert!(!text.contains('E'));
            assert!(!text.starts_with('+'));
        }
        assert_eq!(render(EncodeLimits::default(), |w| w.f64(-0.0)), b"-0");
        assert_eq!(render(EncodeLimits::default(), |w| w.f64(0.0)), b"0");
        assert_eq!(render(EncodeLimits::default(), |w| w.f64(1.0)), b"1");
        assert_eq!(render(EncodeLimits::default(), |w| w.f64(1e20)), b"1e20");
        assert_eq!(render(EncodeLimits::default(), |w| w.f64(f64::from_bits(1))), b"5e-324");
    }

    #[test]
    fn nonfinite_float_errors_precede_all_output_and_poison() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::from_bits(0xfff8_0000_0000_0042)] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            assert_error(writer.f64(value).unwrap_err(), ErrorCode::InvalidNumber);
            assert_eq!(writer.sink.len(), 0);
            assert!(!writer.root_written);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn decimal_vectors_preserve_scale_sign_and_extreme_coefficients() {
        for (coefficient, scale, expected) in [
            (0, 0, "\"0\""), (0, 2, "\"0.00\""),
            (123, 2, "\"1.23\""), (-123, 2, "\"-1.23\""),
            (1, 2, "\"0.01\""), (-1, 2, "\"-0.01\""),
            (100, 2, "\"1.00\""), (10, 1, "\"1.0\""),
            (12, 2, "\"0.12\""), (123, 0, "\"123\""),
            (i128::MIN, 0, "\"-170141183460469231731687303715884105728\""),
            (i128::MAX, 0, "\"170141183460469231731687303715884105727\""),
            (i128::MIN, 38, "\"-1.70141183460469231731687303715884105728\""),
            (1, 38, "\"0.00000000000000000000000000000000000001\""),
        ] {
            let bytes = render(EncodeLimits::default(), |w| {
                w.decimal(Decimal128 { coefficient, scale })
            });
            assert_eq!(bytes, expected.as_bytes());
        }
    }

    #[test]
    fn decimal_scale_rejection_is_atomic_and_poisoning() {
        for scale in [39, u8::MAX] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            assert_error(writer.decimal(Decimal128::new(i128::MIN, scale)).unwrap_err(),
                ErrorCode::DecimalScaleMismatch);
            assert_eq!(writer.sink.len(), 0);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn every_control_character_uses_the_exact_lowercase_escape() {
        let controls: [u8; 32] = core::array::from_fn(|index| index as u8);
        let text = core::str::from_utf8(&controls).unwrap();
        let bytes = render(EncodeLimits::default(), |w| w.string(text));
        let expected = concat!(
            "\"\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007",
            "\\b\\t\\n\\u000b\\f\\r\\u000e\\u000f",
            "\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015\\u0016\\u0017",
            "\\u0018\\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f\"",
        );
        assert_eq!(bytes, expected.as_bytes());
        assert_eq!(bytes.len(), escaped_string_len(text).unwrap());
    }

    #[test]
    fn quotes_backslashes_utf8_and_slashes_escape_identically_in_keys_and_values() {
        let text = "a\"\\/é😀\u{2028}\u{2029}\u{7f}\0z";
        let escaped = "\"a\\\"\\\\/é😀\u{2028}\u{2029}\u{7f}\\u0000z\"";
        assert_eq!(render(EncodeLimits::default(), |w| w.string(text)), escaped.as_bytes());
        let bytes = render(EncodeLimits::default(), |w| {
            w.begin_object()?;
            w.key(text)?;
            w.string(text)?;
            w.end_object()
        });
        let mut expected = Vec::new();
        ByteSink::write_byte(&mut expected, b'{').unwrap();
        ByteSink::write_bytes(&mut expected, escaped.as_bytes()).unwrap();
        ByteSink::write_byte(&mut expected, b':').unwrap();
        ByteSink::write_bytes(&mut expected, escaped.as_bytes()).unwrap();
        ByteSink::write_byte(&mut expected, b'}').unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(escaped.len(), escaped_string_len(text).unwrap());
    }

    #[test]
    fn string_and_key_limits_count_decoded_utf8_bytes() {
        let limits = EncodeLimits { max_string_bytes: 4, ..EncodeLimits::default() };
        assert_eq!(render(limits, |w| w.string("😀")), "\"😀\"".as_bytes());
        assert_eq!(render(limits, |w| w.string("\0\n\t\r")), b"\"\\u0000\\n\\t\\r\"");
        assert_eq!(render(limits, |w| {
            w.begin_object()?; w.key("😀")?; w.null()?; w.end_object()
        }), "{\"😀\":null}".as_bytes());
        for key in [false, true] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            if key { writer.begin_object().unwrap(); }
            let before = writer.sink.len();
            let error = if key { writer.key("😀a") } else { writer.string("😀a") };
            assert_error(error.unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(writer.sink.len(), before);
            assert_poisoned(writer);
        }
        let zero = EncodeLimits { max_string_bytes: 0, ..limits };
        assert_eq!(render(zero, |w| w.string("")), b"\"\"");
        assert_eq!(render(zero, |w| {
            w.begin_object()?; w.key("")?; w.null()?; w.end_object()
        }), b"{\"\":null}");
    }

    #[test]
    fn exact_output_limit_includes_every_escape_and_quote() {
        for text in ["", "plain", "\0", "\n\"\\", "é😀"] {
            let exact = escaped_string_len(text).unwrap();
            let limits = EncodeLimits { max_output_bytes: exact, ..EncodeLimits::default() };
            assert_eq!(render(limits, |w| w.string(text)).len(), exact);
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits { max_output_bytes: exact - 1, ..limits });
            assert_error(writer.string(text).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_eq!(writer.sink.len(), 0);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn output_budget_and_finish_length_exclude_existing_prefix() {
        let mut sink = VecSink::new(32);
        sink.write_bytes(b"existing:").unwrap();
        let limits = EncodeLimits { max_output_bytes: 4, ..EncodeLimits::default() };
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.null().unwrap();
        assert_eq!(writer.finish().unwrap(), 4);
        assert_eq!(sink.as_slice(), b"existing:null");

        let mut writer = JsonWriter::new(&mut sink, EncodeLimits { max_output_bytes: 0, ..limits });
        assert_error(writer.null().unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(writer.sink.len(), 0);
        assert_poisoned(writer);
        assert_eq!(sink.as_slice(), b"existing:null");
    }

    #[test]
    fn commas_colons_and_closing_delimiters_are_budgeted_before_writing() {
        let mut sink = VecSink::new(64);
        let limits = EncodeLimits { max_output_bytes: 3, ..EncodeLimits::default() };
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        writer.u64(1).unwrap();
        assert_error(writer.u64(2).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(writer.sink.len(), 2);
        assert_eq!(writer.frames[0].count, 1);
        assert_poisoned(writer);
        assert_eq!(sink.as_slice(), b"[1");

        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_object().unwrap();
        assert_error(writer.key("").unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(writer.frames[0].count, 0);
        assert!(writer.frames[0].expecting_key);
        assert_poisoned(writer);
        assert_eq!(sink.as_slice(), b"{");

        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits { max_output_bytes: 1, ..limits });
        writer.begin_array().unwrap();
        assert_error(writer.end_array().unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(writer.frames.len(), 1);
        assert_poisoned(writer);
        assert_eq!(sink.as_slice(), b"[");
    }

    #[test]
    fn number_and_decimal_content_limits_are_inclusive() {
        let limits = EncodeLimits { max_number_bytes: 2, ..EncodeLimits::default() };
        assert_eq!(render(limits, |w| w.u64(99)), b"99");
        assert_eq!(render(limits, |w| w.i64(-9)), b"-9");
        assert_eq!(render(limits, |w| w.f64(-0.0)), b"-0");
        assert_eq!(render(limits, |w| w.decimal(Decimal128::new(-1, 0))), b"\"-1\"");
        for operation in 0..4 {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            let error = match operation {
                0 => writer.u64(100),
                1 => writer.i64(-10),
                2 => writer.f64(0.1),
                _ => writer.decimal(Decimal128::new(1, 1)),
            };
            assert_error(error.unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(writer.sink.len(), 0);
            assert_poisoned(writer);
        }
        let limits = EncodeLimits { max_number_bytes: 0, ..limits };
        assert_eq!(render(limits, |w| w.null()), b"null");
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, limits);
        assert_error(writer.u64(0).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_poisoned(writer);
    }

    #[test]
    fn decimal_string_and_output_limits_include_the_correct_bytes() {
        let limits = EncodeLimits {
            max_string_bytes: 4, max_number_bytes: 4, max_output_bytes: 6,
            ..EncodeLimits::default()
        };
        assert_eq!(render(limits, |w| w.decimal(Decimal128::new(123, 2))), b"\"1.23\"");
        for limits in [
            EncodeLimits { max_string_bytes: 3, ..limits },
            EncodeLimits { max_output_bytes: 5, ..limits },
        ] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            let code = if limits.max_string_bytes == 3 { ErrorCode::BudgetExhausted }
                else { ErrorCode::PayloadTooLarge };
            assert_error(writer.decimal(Decimal128::new(123, 2)).unwrap_err(), code);
            assert_eq!(writer.sink.len(), 0);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn depth_zero_allows_scalars_and_rejects_root_containers() {
        let limits = EncodeLimits { max_depth: 0, ..EncodeLimits::default() };
        assert_eq!(render(limits, |w| w.null()), b"null");
        assert_eq!(render(limits, |w| w.string("x")), b"\"x\"");
        for object in [false, true] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            let error = if object { writer.begin_object() } else { writer.begin_array() };
            assert_error(error.unwrap_err(), ErrorCode::DepthLimitExceeded);
            assert_eq!(writer.sink.len(), 0);
            assert!(writer.frames.is_empty());
            assert!(!writer.root_written);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn inclusive_depth_and_frame_counts_do_not_double_count_children() {
        let limits = EncodeLimits { max_depth: 2, max_container_items: 1, ..EncodeLimits::default() };
        assert_eq!(render(limits, |w| {
            w.begin_array()?;
            w.begin_object()?;
            assert_eq!(w.frames[0].count, 1);
            w.key("x")?;
            w.null()?;
            assert_eq!(w.frames[1].count, 1);
            w.end_object()?;
            assert_eq!(w.frames[0].count, 1);
            w.end_array()
        }), b"[{\"x\":null}]");
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        writer.begin_array().unwrap();
        let before = writer.sink.len();
        assert_error(writer.begin_object().unwrap_err(), ErrorCode::DepthLimitExceeded);
        assert_eq!(writer.sink.len(), before);
        assert_eq!(writer.frames[1].count, 0);
        assert_poisoned(writer);
    }

    #[test]
    fn zero_container_budget_accepts_empty_containers_only() {
        let limits = EncodeLimits { max_container_items: 0, ..EncodeLimits::default() };
        assert_eq!(render(limits, |w| { w.begin_array()?; w.end_array() }), b"[]");
        assert_eq!(render(limits, |w| { w.begin_object()?; w.end_object() }), b"{}");
        for object in [false, true] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            let error = if object {
                writer.begin_object().unwrap();
                writer.key("a")
            } else {
                writer.begin_array().unwrap();
                writer.null()
            };
            assert_error(error.unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(writer.sink.len(), 1);
            assert_eq!(writer.frames[0].count, 0);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn second_array_item_and_object_property_fail_before_the_comma() {
        let limits = EncodeLimits { max_container_items: 1, ..EncodeLimits::default() };
        for object in [false, true] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            if object {
                writer.begin_object().unwrap();
                writer.key("a").unwrap();
            } else {
                writer.begin_array().unwrap();
            }
            writer.null().unwrap();
            let before = writer.sink.len();
            let error = if object { writer.key("b") } else { writer.begin_object() };
            assert_error(error.unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(writer.sink.len(), before);
            assert_eq!(writer.frames[0].count, 1);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn maximum_item_counter_does_not_wrap() {
        // Seed only the private counter so the arithmetic boundary needs no
        // multi-gigabyte fixture. Public operations still perform every check.
        let limits = EncodeLimits { max_container_items: u32::MAX, ..EncodeLimits::default() };
        for object in [false, true] {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, limits);
            if object { writer.begin_object().unwrap(); } else { writer.begin_array().unwrap(); }
            writer.frames[0].count = u32::MAX - 1;
            if object { writer.key("last").unwrap(); }
            writer.null().unwrap();
            assert_eq!(writer.frames[0].count, u32::MAX);
            let before = writer.sink.len();
            let error = if object { writer.key("overflow") } else { writer.null() };
            assert_error(error.unwrap_err(), ErrorCode::BudgetExhausted);
            assert_eq!(writer.sink.len(), before);
            assert_eq!(writer.frames[0].count, u32::MAX);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn invalid_operation_orders_poison_without_extra_output() {
        for case in 0..10 {
            let mut sink = VecSink::new(128);
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            match case {
                0..=2 => (),
                3 | 4 => writer.begin_array().unwrap(),
                5..=8 => writer.begin_object().unwrap(),
                _ => writer.null().unwrap(),
            }
            if case == 7 || case == 8 { writer.key("a").unwrap(); }
            let before = writer.sink.len();
            let error = match case {
                0 => writer.key("a"),
                1 => writer.end_object(),
                2 => writer.end_array(),
                3 => writer.key("a"),
                4 => writer.end_object(),
                5 => writer.null(),
                6 => writer.end_array(),
                7 => writer.key("b"),
                8 => writer.end_object(),
                _ => writer.boolean(true),
            };
            assert_error(error.unwrap_err(), ErrorCode::InvalidSyntax);
            assert_eq!(writer.sink.len(), before);
            assert_poisoned(writer);
        }
    }

    #[test]
    fn unfinished_messages_report_unexpected_eof_and_drop_does_not_close() {
        for case in 0..4 {
            let mut sink = VecSink::new(64);
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            match case {
                1 => writer.begin_array().unwrap(),
                2 | 3 => writer.begin_object().unwrap(),
                _ => (),
            }
            if case == 3 { writer.key("a").unwrap(); }
            let before = writer.sink.len();
            assert_error(writer.finish().unwrap_err(), ErrorCode::UnexpectedEof);
            assert_eq!(sink.len(), before);
        }
        let mut sink = VecSink::new(64);
        {
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            writer.begin_array().unwrap();
            writer.null().unwrap();
        }
        assert_eq!(sink.as_slice(), b"[null");
    }

    #[test]
    fn every_operation_and_finish_reject_a_poisoned_writer() {
        let mut sink = VecSink::new(64);
        let fragment = super::super::raw::validate(b"null", EncodeLimits::default()).unwrap();
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        assert_error(writer.key("bad").unwrap_err(), ErrorCode::InvalidSyntax);
        let errors = [
            writer.begin_object(), writer.end_object(), writer.begin_array(), writer.end_array(),
            writer.key("a"), writer.null(), writer.boolean(true), writer.string("x"),
            writer.u64(0), writer.i64(0), writer.f64(f64::NAN),
            writer.decimal(Decimal128::new(0, u8::MAX)), writer.raw(fragment),
        ];
        for error in errors { assert_error(error.unwrap_err(), ErrorCode::InternalError); }
        assert_eq!(writer.sink.len(), 0);
        assert_error(writer.finish().unwrap_err(), ErrorCode::InternalError);
    }

    #[test]
    fn raw_preserves_spelling_whitespace_and_parent_item_accounting() {
        let fragment = super::super::raw::validate(
            b" \n{\"x\" : [1e+02, \"\\u0061\"], \"x\":null}\t", EncodeLimits::default(),
        ).unwrap();
        let bytes = render(EncodeLimits::default(), |w| {
            w.begin_array()?;
            w.raw(fragment)?;
            assert_eq!(w.frames[0].count, 1);
            w.boolean(false)?;
            w.end_array()
        });
        assert_eq!(bytes, b"[ \n{\"x\" : [1e+02, \"\\u0061\"], \"x\":null}\t,false]");
        let bytes = render(EncodeLimits::default(), |w| {
            w.begin_object()?; w.key("raw")?; w.raw(fragment)?; w.end_object()
        });
        assert_eq!(bytes, b"{\"raw\": \n{\"x\" : [1e+02, \"\\u0061\"], \"x\":null}\t}");
    }

    #[test]
    fn raw_depth_is_relative_to_current_open_containers() {
        let nested = super::super::raw::validate(b"[{}]", EncodeLimits::default()).unwrap();
        let scalar = super::super::raw::validate(b"null", EncodeLimits::default()).unwrap();
        let limits = EncodeLimits { max_depth: 2, ..EncodeLimits::default() };
        assert_eq!(render(limits, |w| w.raw(nested)), b"[{}]");
        assert_eq!(render(limits, |w| {
            w.begin_array()?; w.begin_array()?; w.raw(scalar)?; w.end_array()?; w.end_array()
        }), b"[[null]]");
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        let error = writer.raw(nested).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(writer.sink.len(), 1);
        assert_eq!(writer.frames[0].count, 0);
        assert_poisoned(writer);
        assert_eq!(render(EncodeLimits { max_depth: 0, ..limits }, |w| w.raw(scalar)), b"null");
    }

    #[test]
    fn raw_revalidates_nested_counts_decoded_strings_and_number_lengths() {
        let loose = EncodeLimits::default();
        let cases: &[(&[u8], EncodeLimits, ErrorCode)] = &[
            (b"{\"x\":[0,1]}", EncodeLimits { max_container_items: 1, ..loose }, ErrorCode::BudgetExhausted),
            (b"[{\"a\":0,\"b\":1}]", EncodeLimits { max_container_items: 1, ..loose }, ErrorCode::BudgetExhausted),
            (br#"["\ud83d\ude00"]"#, EncodeLimits { max_string_bytes: 3, ..loose }, ErrorCode::BudgetExhausted),
            (br#"{"\u0061\u0062":0}"#, EncodeLimits { max_string_bytes: 1, ..loose }, ErrorCode::BudgetExhausted),
            (b"[123]", EncodeLimits { max_number_bytes: 2, ..loose }, ErrorCode::BudgetExhausted),
            (b" null ", EncodeLimits { max_output_bytes: 5, ..loose }, ErrorCode::PayloadTooLarge),
        ];
        for &(bytes, limits, code) in cases {
            let fragment = super::super::raw::validate(bytes, loose).unwrap();
            let mut sink = VecSink::new(128);
            let mut writer = JsonWriter::new(&mut sink, limits);
            let error = writer.raw(fragment).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.source, ErrorSource::Encode);
            assert_eq!(writer.sink.len(), 0);
            assert!(!writer.root_written);
            assert_poisoned(writer);
        }
        let fragment = super::super::raw::validate(br#"["\ud83d\ude00"]"#, loose).unwrap();
        assert_eq!(render(EncodeLimits { max_string_bytes: 4, ..loose }, |w| w.raw(fragment)),
            br#"["\ud83d\ude00"]"#);
    }

    #[test]
    fn raw_whole_token_reservation_includes_parent_comma() {
        let fragment = super::super::raw::validate(b" null ", EncodeLimits::default()).unwrap();
        let limits = EncodeLimits { max_output_bytes: 9, ..EncodeLimits::default() };
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, limits);
        writer.begin_array().unwrap();
        writer.u64(1).unwrap();
        writer.raw(fragment).unwrap();
        assert_eq!(writer.sink.len(), 9);
        assert_error(writer.end_array().unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_poisoned(writer);
        assert_eq!(sink.as_slice(), b"[1, null ");

        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits { max_output_bytes: 8, ..limits });
        writer.begin_array().unwrap();
        writer.u64(1).unwrap();
        assert_error(writer.raw(fragment).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(writer.sink.len(), 2);
        assert_poisoned(writer);
        assert_eq!(sink.as_slice(), b"[1");
    }

    #[test]
    fn raw_revalidation_preserves_parser_error_offsets() {
        // Only crate-internal tests can construct this malformed record. The
        // public API admits fragments exclusively through raw::validate.
        let fragment = RawFragment { bytes: b"[0,]", kind: FragmentKind::Array, depth: 0 };
        let expected = super::super::raw::validate(fragment.bytes, EncodeLimits::default()).unwrap_err();
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        assert_eq!(writer.raw(fragment).unwrap_err(), expected);
        assert_eq!(writer.sink.len(), 0);
        assert_poisoned(writer);
    }

    #[test]
    fn external_reservation_failure_retains_metadata_and_never_starts_value() {
        let mut sink = FaultSink::new();
        sink.reject_reserve = true;
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        assert_eq!(writer.begin_object().unwrap_err(), external_error());
        assert!(!writer.root_written);
        assert!(writer.frames.is_empty());
        assert_poisoned(writer);
        assert_eq!(sink.reservations, 1);
        assert_eq!(sink.writes, 0);
        assert!(sink.bytes.is_empty());
    }

    #[test]
    fn partial_external_writes_are_visible_and_do_not_commit_syntax_state() {
        let mut sink = FaultSink::new();
        sink.fail_after = Some(2);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        assert_eq!(writer.string("abc\n").unwrap_err(), external_error());
        assert!(!writer.root_written);
        assert_eq!(writer.sink.len(), 2);
        assert_poisoned(writer);
        assert_eq!(sink.bytes, b"\"a");
        assert_eq!(sink.reservations, 1);
        assert_eq!(sink.last_reservation, 7);
    }

    #[test]
    fn external_failure_after_comma_never_increments_parent_count() {
        let mut sink = FaultSink::new();
        sink.fail_after = Some(3);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        writer.begin_array().unwrap();
        writer.u64(0).unwrap();
        assert_eq!(writer.null().unwrap_err(), external_error());
        assert_eq!(writer.frames[0].count, 1);
        assert_poisoned(writer);
        assert_eq!(sink.bytes, b"[0,");
    }

    #[test]
    fn incorrect_external_success_lengths_poison_writer() {
        for reservation in [false, true] {
            let mut sink = FaultSink::new();
            sink.dishonest_reserve = reservation;
            sink.dishonest_write = !reservation;
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            assert_error(writer.null().unwrap_err(), ErrorCode::InternalError);
            assert!(!writer.root_written);
            assert_poisoned(writer);
            assert_eq!(sink.reservations, 1);
            assert_eq!(sink.writes, usize::from(!reservation));
        }
    }

    #[test]
    fn complete_string_key_and_number_tokens_are_reserved_once_before_emission() {
        let mut sink = FaultSink::new();
        {
            let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
            writer.begin_object().unwrap();
            writer.key("a\0").unwrap();
            writer.string("é\n").unwrap();
            writer.key("b").unwrap();
            writer.u64(u64::MAX).unwrap();
            writer.end_object().unwrap();
            writer.finish().unwrap();
        }
        assert_eq!(sink.reservations, 6);
        assert_eq!(sink.last_reservation, 1);
        assert_eq!(sink.bytes, "{\"a\\u0000\":\"é\\n\",\"b\":18446744073709551615}".as_bytes());
    }
}
