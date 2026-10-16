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
