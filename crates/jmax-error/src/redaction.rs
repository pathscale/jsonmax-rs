//! Policy-controlled, bounded display adapters for diagnostics.

use core::fmt;

use crate::{codes, BoundedPath, Diagnostic, PathSegment, RedactionPolicy, SafeDisplay, SafeError};

/// Creates a display adapter that applies `policy` without exposing raw details by default.
pub fn display(diagnostic: &Diagnostic, policy: RedactionPolicy) -> SafeDisplay<'_> {
    SafeDisplay { diagnostic, policy }
}

/// Creates an error adapter whose source chain never exposes the underlying diagnostic.
pub fn error(diagnostic: &Diagnostic, policy: RedactionPolicy) -> SafeError<'_> {
    SafeError { diagnostic, policy }
}

/// Allocates the policy-controlled representation of a diagnostic.
pub fn redact(diagnostic: &Diagnostic, policy: RedactionPolicy) -> alloc::string::String {
    use alloc::string::ToString;
    display(diagnostic, policy).to_string()
}

impl fmt::Display for SafeDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        render(self.diagnostic, self.policy, f)
    }
}

impl fmt::Debug for SafeDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        render(self.diagnostic, self.policy, f)
    }
}

impl fmt::Display for SafeError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        render(self.diagnostic, self.policy, f)
    }
}

impl fmt::Debug for SafeError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        render(self.diagnostic, self.policy, f)
    }
}

impl core::error::Error for SafeError<'_> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        None
    }
}

fn render(
    diagnostic: &Diagnostic,
    policy: RedactionPolicy,
    formatter: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    let mut counter = Sink::counting();
    write_content(diagnostic, policy, &mut counter);
    let total = counter.count;
    if total <= policy.max_output_bytes {
        let mut emitter = Sink::writing(formatter, policy.max_output_bytes);
        write_content(diagnostic, policy, &mut emitter);
        if emitter.failed { return Err(fmt::Error); }
        return Ok(());
    }

    let marker_len = policy.max_output_bytes.min(3);
    let prefix_limit = policy.max_output_bytes - marker_len;
    let mut emitter = Sink::writing(formatter, prefix_limit);
    write_content(diagnostic, policy, &mut emitter);
    if emitter.failed { return Err(fmt::Error); }
    emitter.write_marker(marker_len)
}

/// A sink either counts the exact logical byte length or writes complete tokens.
/// Every emitted unit is ASCII, and escaped bytes are kept as indivisible tokens.
struct Sink<'a, 'f> {
    formatter: Option<&'a mut fmt::Formatter<'f>>,
    count: usize,
    limit: usize,
    stopped: bool,
    failed: bool,
}

impl<'a, 'f> Sink<'a, 'f> {
    fn counting() -> Sink<'static, 'static> {
        Sink { formatter: None, count: 0, limit: usize::MAX, stopped: false, failed: false }
    }

    fn writing(formatter: &'a mut fmt::Formatter<'f>, limit: usize) -> Self {
        Self { formatter: Some(formatter), count: 0, limit, stopped: false, failed: false }
    }

    fn token(&mut self, text: &str) {
        if self.formatter.is_none() {
            self.count = self.count.saturating_add(text.len());
            return;
        }
        if self.stopped {
            return;
        }
        if text.len() > self.limit.saturating_sub(self.count) {
            self.stopped = true;
            return;
        }
        if let Some(formatter) = self.formatter.as_deref_mut() {
            // `write_str` ignores formatter width, precision and alternate flags.
            if formatter.write_str(text).is_err() {
                self.failed = true;
                return;
            }
        }
        self.count += text.len();
    }

    fn literal(&mut self, text: &str) {
        for character in text.chars() {
            let mut encoded = [0u8; 4];
            self.token(character.encode_utf8(&mut encoded));
        }
    }

    fn quoted_bytes(&mut self, bytes: &[u8]) {
        self.token("\"");
        for &byte in bytes {
            match byte {
                0x20..=0x7e if byte != b'"' && byte != b'\\' => {
                    self.ascii_byte(byte);
                }
                b'"' => self.token("\\\""),
                b'\\' => self.token("\\\\"),
                _ => {
                    let escaped = [b'\\', b'x', hex(byte >> 4), hex(byte & 0x0f)];
                    self.ascii_token(&escaped);
                }
            }
        }
        self.token("\"");
    }

    fn decimal(&mut self, mut value: u64) {
        let mut buffer = [0u8; 20];
        let mut cursor = buffer.len();
        loop {
            cursor -= 1;
            buffer[cursor] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 { break; }
        }
        for &byte in &buffer[cursor..] { self.ascii_byte(byte); }
    }

    fn write_marker(&mut self, count: usize) -> fmt::Result {
        if count == 0 { return Ok(()); }
        let marker = "...";
        if let Some(formatter) = self.formatter.as_deref_mut() {
            formatter.write_str(&marker[..count])?;
        }
        Ok(())
    }

    fn ascii_byte(&mut self, byte: u8) {
        let character = char::from(byte);
        let mut encoded = [0u8; 4];
        self.token(character.encode_utf8(&mut encoded));
    }

    fn ascii_token(&mut self, bytes: &[u8]) {
        // Call sites supply only known ASCII escape tokens.
        match core::str::from_utf8(bytes) {
            Ok(text) => self.token(text),
            Err(_) => self.failed = true,
        }
    }
}

fn hex(nibble: u8) -> u8 {
    if nibble < 10 { b'0' + nibble } else { b'a' + (nibble - 10) }
}

fn write_content(diagnostic: &Diagnostic, policy: RedactionPolicy, sink: &mut Sink<'_, '_>) {
    let error = &diagnostic.error;
    sink.literal(codes::code_name(error.code));
    sink.literal(" [");
    sink.literal(codes::source_name(error.source));
    sink.literal("] at byte ");
    sink.decimal(u64::from(error.byte_offset));

    if let Some(location) = diagnostic.location {
        sink.literal(" (line ");
        sink.decimal(location.line);
        sink.literal(", column ");
        sink.decimal(location.column);
        sink.literal(")");
    }
    if error.schema_id != 0 || error.field_id != 0 {
        sink.literal(" schema=");
        sink.decimal(u64::from(error.schema_id));
        sink.literal(" field=");
        sink.decimal(u64::from(error.field_id));
    }
    if policy.include_detail {
        sink.literal(" detail=");
        sink.quoted_bytes(error.detail.as_bytes());
    }
    if policy.include_paths {
        if let Some(path) = diagnostic.json_path.as_ref() {
            sink.literal(" json_path=");
            write_path(path, sink);
        }
        if let Some(path) = diagnostic.schema_path.as_ref() {
            sink.literal(" schema_path=");
            write_path(path, sink);
        }
    }
    if policy.include_context {
        if let Some(context) = diagnostic.context.as_ref() {
            sink.literal(" context@");
            sink.decimal(u64::from(context.span().offset));
            sink.literal("=");
            sink.quoted_bytes(context.bytes());
        }
    }
}

fn write_path(path: &BoundedPath, sink: &mut Sink<'_, '_>) {
    sink.token("\"");
    for segment in path.segments() {
        sink.token("/");
        match segment {
            PathSegment::Key(key) => {
                for character in key.chars() {
                    match character {
                        '~' => sink.quoted_path_ascii(b"~0"),
                        '/' => sink.quoted_path_ascii(b"~1"),
                        other => {
                            let mut bytes = [0u8; 4];
                            let encoded = other.encode_utf8(&mut bytes);
                            for &byte in encoded.as_bytes() { sink.quoted_byte(byte); }
                        }
                    }
                }
            }
            PathSegment::Index(index) => sink.decimal(u64::from(*index)),
        }
    }
    sink.token("\"");
}

impl Sink<'_, '_> {
    fn quoted_path_ascii(&mut self, bytes: &[u8]) {
        for &byte in bytes { self.quoted_byte(byte); }
    }

    fn quoted_byte(&mut self, byte: u8) {
        match byte {
            0x20..=0x7e if byte != b'"' && byte != b'\\' => {
                self.ascii_byte(byte);
            }
            b'"' => self.token("\\\""),
            b'\\' => self.token("\\\\"),
            _ => {
                let escaped = [b'\\', b'x', hex(byte >> 4), hex(byte & 0x0f)];
                self.ascii_token(&escaped);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use crate::{ByteContext, Error, ErrorCode, ErrorSource, PathKind, PathLimits, Span32};

    fn diagnostic() -> Diagnostic {
        Diagnostic::new(Error::new(ErrorCode::InvalidToken, ErrorSource::Lexer, 7, "bad\n\"token\\"))
            .error_with_schema(0, 0)
    }

    trait DiagnosticTestExt { fn error_with_schema(self, schema: u32, field: u32) -> Diagnostic; }
    impl DiagnosticTestExt for Diagnostic {
        fn error_with_schema(mut self, schema: u32, field: u32) -> Diagnostic {
            self.error = self.error.with_schema(schema, field);
            self
        }
    }

    #[test]
    fn default_policy_is_safe_and_adapters_are_identical() {
        let diagnostic = diagnostic();
        let policy = RedactionPolicy::safe_default();
        let shown = display(&diagnostic, policy);
        let safe_error = error(&diagnostic, policy);
        let expected = "InvalidToken [Lexer] at byte 7";
        assert_eq!(shown.to_string(), expected);
        assert_eq!(format!("{:?}", shown), expected);
        assert_eq!(safe_error.to_string(), expected);
        assert_eq!(format!("{:?}", safe_error), expected);
        assert_eq!(redact(&diagnostic, policy), expected);
        assert!(core::error::Error::source(&safe_error).is_none());
    }

    #[test]
    fn full_policy_orders_fields_and_escapes_raw_bytes() {
        let mut diagnostic = diagnostic().error_with_schema(12, 3);
        diagnostic.location = Some(crate::SourceLocation { byte_offset: 7, line: 2, column: 4 });
        let mut json = BoundedPath::new(PathKind::Json, PathLimits::default());
        json.push_key("a~/é").unwrap();
        json.push_index(u32::MAX).unwrap();
        diagnostic.json_path = Some(json);
        diagnostic.context = Some(ByteContext {
            bytes: alloc::vec![b'A', b'\n', 0xff, b'"'],
            span: Span32::new(5, 4),
            focus: 2,
        });
        let policy = RedactionPolicy { include_detail: true, include_paths: true, include_context: true, max_output_bytes: 1024 };
        assert_eq!(redact(&diagnostic, policy), "InvalidToken [Lexer] at byte 7 (line 2, column 4) schema=12 field=3 detail=\"bad\\x0a\\\"token\\\\\" json_path=\"/a~0~1/\\xc3\\xa9/4294967295\" context@5=\"A\\x0a\\xff\\\"\"");
    }

    #[test]
    fn truncation_respects_byte_budget_and_complete_escape_tokens() {
        let diagnostic = diagnostic();
        for budget in 0..=12 {
            let output = redact(&diagnostic, RedactionPolicy { max_output_bytes: budget, ..RedactionPolicy::safe_default() });
            assert!(output.len() <= budget);
            if budget == 0 { assert_eq!(output, ""); }
            if output.ends_with("...") { assert!(output.is_ascii()); }
        }
        let mut control_detail = diagnostic;
        control_detail.error = Error::new(ErrorCode::InvalidToken, ErrorSource::Lexer, 0, "\n");
        let output = redact(&control_detail, RedactionPolicy { include_detail: true, max_output_bytes: 40, ..RedactionPolicy::safe_default() });
        assert!(!output.ends_with("\\x"));
        assert!(output.len() <= 40);
    }

    #[test]
    fn absent_optional_fields_and_zero_ids_are_omitted() {
        let diagnostic = diagnostic();
        let policy = RedactionPolicy { include_detail: true, include_paths: true, include_context: true, max_output_bytes: 512 };
        assert_eq!(redact(&diagnostic, policy), "InvalidToken [Lexer] at byte 7 detail=\"bad\\x0a\\\"token\\\\\"");
    }

    #[test]
    fn formatter_flags_do_not_expand_or_reformat_output() {
        let diagnostic = diagnostic();
        let shown = display(&diagnostic, RedactionPolicy { max_output_bytes: 5, ..RedactionPolicy::safe_default() });
        assert_eq!(format!("{:>30.2?}", shown), "In...");
    }

    #[test]
    fn boundary_numbers_and_context_start_are_decimal() {
        let mut diagnostic = Diagnostic::new(Error::new(ErrorCode::UnexpectedEof, ErrorSource::Cursor, u32::MAX, ""));
        diagnostic.context = Some(ByteContext { bytes: alloc::vec![], span: Span32::new(u32::MAX, 0), focus: 0 });
        let policy = RedactionPolicy { include_context: true, max_output_bytes: 512, ..RedactionPolicy::safe_default() };
        assert_eq!(redact(&diagnostic, policy), "UnexpectedEof [Cursor] at byte 4294967295 context@4294967295=\"\"");
    }
}
