//! Endpoint-friendly diagnostics that retain the exact selected JSON path.

use alloc::string::String;
use jmax_path::{Path, Segment};

use crate::{Error, ErrorCode, ErrorSource, Result};

/// The two fields used by endpoint `BAD_REQUEST` responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathErrorReport {
    /// Stable machine-readable error kind.
    pub kind: ErrorCode,
    /// The canonical path followed by the original diagnostic detail.
    pub message: String,
}

/// Formats a parsed path and an existing error for an endpoint error response.
///
/// Keys are emitted in canonical native-path syntax: simple keys use dot
/// notation, other keys use a JSON-escaped bracket string, and indexes use
/// unpadded decimal bracket notation. The root is `$`. The original error
/// code is returned as `kind`; its detail is preserved after the path.
///
/// The sole allocation is explicitly reserved before writing. Allocation or
/// size failure is reported as `BudgetExhausted`; the input error is otherwise
/// left untouched.
pub fn format_path_error(path: &Path, error: &Error) -> Result<PathErrorReport> {
    let path_len = canonical_path_len(path)?;
    let separator_len = if error.detail.is_empty() { 0 } else { 2 };
    let capacity = path_len
        .checked_add(separator_len)
        .and_then(|length| length.checked_add(error.detail.len()))
        .ok_or_else(|| format_error(ErrorCode::PayloadTooLarge, "formatted path error is too large"))?;

    let mut message = String::new();
    message.try_reserve_exact(capacity).map_err(|_| {
        format_error(ErrorCode::BudgetExhausted, "path error message allocation failed")
    })?;
    append_path(&mut message, path);
    if !error.detail.is_empty() {
        message.push_str(": ");
        message.push_str(error.detail);
    }
    debug_assert_eq!(message.len(), capacity);
    Ok(PathErrorReport { kind: error.code, message })
}

fn canonical_path_len(path: &Path) -> Result<usize> {
    let mut length = 1usize; // root marker
    for segment in path.segments() {
        let segment_len = match segment {
            Segment::Index(index) => 2usize
                .checked_add(decimal_len(*index))
                .ok_or_else(|| too_large())?,
            Segment::Key(key) if is_bare_key(key) => 1usize
                .checked_add(key.len())
                .ok_or_else(|| too_large())?,
            Segment::Key(key) => {
                let escaped = key.chars().try_fold(4usize, |length, character| {
                    length.checked_add(json_char_len(character))
                }).ok_or_else(|| too_large())?;
                escaped
            }
        };
        length = length.checked_add(segment_len).ok_or_else(|| too_large())?;
    }
    Ok(length)
}

fn append_path(output: &mut String, path: &Path) {
    output.push('$');
    for segment in path.segments() {
        match segment {
            Segment::Index(index) => {
                output.push('[');
                append_decimal(output, *index);
                output.push(']');
            }
            Segment::Key(key) if is_bare_key(key) => {
                output.push('.');
                output.push_str(key);
            }
            Segment::Key(key) => {
                output.push_str("[\"");
                for character in key.chars() {
                    append_json_char(output, character);
                }
                output.push_str("\"]");
            }
        }
    }
}

fn is_bare_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn json_char_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{0008}' | '\u{000c}' => 2,
        '\u{0000}'..='\u{001f}' => 6,
        _ => character.len_utf8(),
    }
}

fn append_json_char(output: &mut String, character: char) {
    match character {
        '"' => output.push_str("\\\""),
        '\\' => output.push_str("\\\\"),
        '\n' => output.push_str("\\n"),
        '\r' => output.push_str("\\r"),
        '\t' => output.push_str("\\t"),
        '\u{0008}' => output.push_str("\\b"),
        '\u{000c}' => output.push_str("\\f"),
        '\u{0000}'..='\u{001f}' => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let value = character as u8;
            output.push_str("\\u00");
            output.push(char::from(HEX[usize::from(value >> 4)]));
            output.push(char::from(HEX[usize::from(value & 0x0f)]));
        }
        _ => output.push(character),
    }
}

fn decimal_len(value: u32) -> usize {
    let mut digits = 1;
    let mut remaining = value;
    while remaining >= 10 {
        digits += 1;
        remaining /= 10;
    }
    digits
}

fn append_decimal(output: &mut String, value: u32) {
    let mut digits = [0u8; 10];
    let mut position = digits.len();
    let mut remaining = value;
    loop {
        position -= 1;
        digits[position] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    for byte in &digits[position..] {
        output.push(char::from(*byte));
    }
}

fn too_large() -> Error {
    format_error(ErrorCode::PayloadTooLarge, "formatted path error is too large")
}

fn format_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(path: &str) -> Path {
        jmax_path::parse::parse(path, jmax_path::PathLimits::default()).unwrap()
    }

    #[test]
    fn formats_indexes_and_json_escaped_keys() {
        let path = parsed("$.items[12][\"a.b/\\\\\\\"\\n\"]");
        let error = Error::new(ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 4, "expected string");
        let report = format_path_error(&path, &error).unwrap();

        assert_eq!(report.kind, ErrorCode::TypeMismatch);
        assert_eq!(report.message, "$.items[12][\"a.b/\\\\\\\"\\n\"]: expected string");
    }

    #[test]
    fn root_path_and_empty_detail_are_supported() {
        let error = Error::new(ErrorCode::InvalidSyntax, ErrorSource::Syntax, 0, "");
        let report = format_path_error(&Path::root(), &error).unwrap();
        assert_eq!(report.message, "$");
    }
}
