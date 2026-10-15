//! Bounded byte locations and owned source context for ABI errors.

use core::convert::TryFrom;

use alloc::vec::Vec;

use crate::{
    ByteContext, ContextLimits, Diagnostic, Error, ErrorCode, ErrorSource, Result, SourceLocation,
    Span32,
};

/// Locates a byte boundary using one-based line and byte-column counters.
///
/// LF bytes strictly before `byte_offset` advance the line. CR and all other
/// bytes count as ordinary column bytes, so the input need not be valid UTF-8.
pub fn locate(input: &[u8], byte_offset: u32) -> Result<SourceLocation> {
    if input.len() > u32::MAX as usize {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Cursor,
            u32::MAX,
            "diagnostic input exceeds 32-bit span capacity",
        ));
    }

    let input_len = u32::try_from(input.len()).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Cursor,
            u32::MAX,
            "diagnostic input exceeds 32-bit span capacity",
        )
    })?;
    if byte_offset > input_len {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic byte offset is outside the input",
        ));
    }

    let prefix_len = usize::try_from(byte_offset).map_err(|_| {
        Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic byte offset is not addressable",
        )
    })?;
    let prefix = &input[..prefix_len];
    let mut line = 1u64;
    let mut column = 1u64;
    for byte in prefix {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    Ok(SourceLocation { byte_offset, line, column })
}

/// Copies the bounded byte range around a byte boundary into owned storage.
///
/// The returned span uses coordinates in the original input. `focus` is the
/// boundary offset relative to the copied range and may equal its length.
pub fn context(input: &[u8], byte_offset: u32, limits: ContextLimits) -> Result<ByteContext> {
    if input.len() > u32::MAX as usize {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Cursor,
            u32::MAX,
            "diagnostic input exceeds 32-bit span capacity",
        ));
    }

    let input_len = u32::try_from(input.len()).map_err(|_| {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Cursor,
            u32::MAX,
            "diagnostic input exceeds 32-bit span capacity",
        )
    })?;
    if byte_offset > input_len {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic byte offset is outside the input",
        ));
    }

    let offset = usize::try_from(byte_offset).map_err(|_| {
        Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic byte offset is not addressable",
        )
    })?;
    let input_len = input.len();
    let before = usize::from(limits.before);
    let start = offset.saturating_sub(before);

    // Widen before addition so even a maximal offset and after limit cannot
    // wrap at the 32-bit coordinate boundary.
    let end_wide = u64::from(byte_offset) + u64::from(limits.after);
    let end = usize::try_from(end_wide.min(input_len as u64)).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic context range is not addressable",
        )
    })?;
    let copied_len = end - start;
    let span_offset = u32::try_from(start).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic context start exceeds 32-bit span capacity",
        )
    })?;
    let span_len = u32::try_from(copied_len).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic context length exceeds 32-bit span capacity",
        )
    })?;
    let focus = u32::try_from(offset - start).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic context focus exceeds 32-bit span capacity",
        )
    })?;

    let mut bytes = Vec::new();
    bytes.try_reserve(copied_len).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Cursor,
            byte_offset,
            "diagnostic context allocation failed",
        )
    })?;
    bytes.extend_from_slice(&input[start..end]);

    Ok(ByteContext { bytes, span: Span32::new(span_offset, span_len), focus })
}

/// Builds an owned diagnostic while preserving every field of the supplied error.
pub fn build(error: Error, input: &[u8], limits: ContextLimits) -> Result<Diagnostic> {
    let location = locate(input, error.byte_offset)?;
    let byte_context = context(input, error.byte_offset, limits)?;
    Ok(Diagnostic {
        error,
        json_path: None,
        schema_path: None,
        location: Some(location),
        context: Some(byte_context),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode, offset: u32) {
        let error = result.expect_err("operation should fail");
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Cursor);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn locate_counts_lf_and_treats_cr_as_an_ordinary_byte() {
        let input = b"ab\r\ncd\nef";
        assert_eq!(locate(input, 0).unwrap(), SourceLocation { byte_offset: 0, line: 1, column: 1 });
        assert_eq!(locate(input, 3).unwrap(), SourceLocation { byte_offset: 3, line: 1, column: 4 });
        // A position on LF remains on the preceding line.
        assert_eq!(locate(input, 3).unwrap().line, 1);
        assert_eq!(locate(input, 4).unwrap(), SourceLocation { byte_offset: 4, line: 2, column: 1 });
        assert_eq!(locate(input, 7).unwrap(), SourceLocation { byte_offset: 7, line: 3, column: 1 });
        assert_eq!(locate(input, 8).unwrap(), SourceLocation { byte_offset: 8, line: 3, column: 2 });
        assert_eq!(locate(input, 9).unwrap(), SourceLocation { byte_offset: 9, line: 3, column: 3 });
    }

    #[test]
    fn locate_uses_byte_columns_for_invalid_utf8_and_multibyte_text() {
        let input = [b'a', 0xff, 0xe9, 0x9b, 0xaa, b'\n', b'z'];
        assert_eq!(locate(&input, 4).unwrap(), SourceLocation { byte_offset: 4, line: 1, column: 5 });
        assert_eq!(locate(&input, 6).unwrap(), SourceLocation { byte_offset: 6, line: 2, column: 1 });
    }

    #[test]
    fn eof_and_empty_input_are_valid_boundaries() {
        assert_eq!(locate(&[], 0).unwrap(), SourceLocation { byte_offset: 0, line: 1, column: 1 });
        assert_eq!(locate(b"x\n\n", 3).unwrap(), SourceLocation { byte_offset: 3, line: 3, column: 1 });
    }

    #[test]
    fn locate_rejects_offsets_beyond_eof_with_cursor_error() {
        assert_error(locate(b"abc", 4), ErrorCode::InvalidToken, 4);
        assert_error(locate(&[], 1), ErrorCode::InvalidToken, 1);
    }

    #[test]
    fn context_selects_inclusive_before_and_exclusive_after_range() {
        let input = b"0123456789";
        let selected = context(input, 5, ContextLimits { before: 2, after: 3 }).unwrap();
        assert_eq!(selected.bytes(), b"34567");
        assert_eq!(selected.span(), Span32::new(3, 5));
        assert_eq!(selected.focus(), 2);
    }

    #[test]
    fn context_obeys_start_eof_and_zero_limit_boundaries() {
        let input = b"abc";
        let start = context(input, 0, ContextLimits { before: 2, after: 1 }).unwrap();
        assert_eq!(start.bytes(), b"a");
        assert_eq!(start.span(), Span32::new(0, 1));
        assert_eq!(start.focus(), 0);

        let eof = context(input, 3, ContextLimits { before: 1, after: 1 }).unwrap();
        assert_eq!(eof.bytes(), b"c");
        assert_eq!(eof.span(), Span32::new(2, 1));
        assert_eq!(eof.focus(), 1);

        let empty = context(input, 2, ContextLimits { before: 0, after: 0 }).unwrap();
        assert!(empty.bytes().is_empty());
        assert_eq!(empty.span(), Span32::new(2, 0));
        assert_eq!(empty.focus(), 0);

        let eof_empty = context(input, 3, ContextLimits { before: 0, after: 0 }).unwrap();
        assert!(eof_empty.bytes().is_empty());
        assert_eq!(eof_empty.span(), Span32::new(3, 0));
        assert_eq!(eof_empty.focus(), 0);
    }

    #[test]
    fn context_preserves_raw_invalid_utf8_bytes() {
        let input = [0xff, b'\n', 0xc3, 0x28];
        let selected = context(&input, 3, ContextLimits { before: 3, after: 1 }).unwrap();
        assert_eq!(selected.bytes(), &[0xff, b'\n', 0xc3, 0x28]);
        assert_eq!(selected.span(), Span32::new(0, 4));
        assert_eq!(selected.focus(), 3);
    }

    #[test]
    fn context_rejects_offsets_beyond_eof_with_cursor_error() {
        assert_error(context(b"abc", 4, ContextLimits::default()), ErrorCode::InvalidToken, 4);
    }

    #[test]
    fn build_attaches_location_and_context_without_rewriting_error() {
        let original = Error::new(ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 4, "expected value")
            .with_schema(17, 29);
        let diagnostic = build(original.clone(), b"a\nbcde", ContextLimits { before: 2, after: 2 }).unwrap();
        assert_eq!(diagnostic.error, original);
        assert_eq!(diagnostic.location, Some(SourceLocation { byte_offset: 4, line: 2, column: 3 }));
        let selected = diagnostic.context.unwrap();
        assert_eq!(selected.bytes(), b"bcde");
        assert_eq!(selected.span(), Span32::new(2, 4));
        assert_eq!(selected.focus(), 2);
        assert_eq!(diagnostic.json_path, None);
        assert_eq!(diagnostic.schema_path, None);
    }

    #[test]
    fn build_propagates_location_errors_and_keeps_input_unreferenced() {
        let error = Error::new(ErrorCode::InvalidToken, ErrorSource::Lexer, 5, "bad token");
        assert_error(build(error, b"tiny", ContextLimits::default()), ErrorCode::InvalidToken, 5);

        let owned = build(
            Error::new(ErrorCode::UnexpectedEof, ErrorSource::Syntax, 2, "eof"),
            b"large source",
            ContextLimits { before: 1, after: 0 },
        ).unwrap();
        assert_eq!(owned.context.as_ref().unwrap().bytes(), b"o");
        assert_eq!(owned.context.as_ref().unwrap().span(), Span32::new(1, 1));
    }
}
