//! Allocation-free error metadata shared by every processing stage.

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    UnexpectedEof,
    InvalidSyntax,
    InvalidToken,
    TrailingContent,
    UnclosedString,
    InvalidEscape,
    LoneSurrogate,
    InvalidUtf8,
    ControlCharInString,
    InvalidNumber,
    NumberOverflow,
    DecimalPrecisionOverflow,
    DecimalScaleMismatch,
    TickMultipleViolation,
    DepthLimitExceeded,
    PayloadTooLarge,
    DecodedArenaOverflow,
    DuplicateObjectKey,
    UnknownObjectKey,
    MissingRequiredField,
    TypeMismatch,
    ConstraintViolation,
    PatternMismatch,
    FormatMismatch,
    UnsupportedSchemaFeature,
    SchemaImageInvalid,
    FramingError,
    TransportIncomplete,
    AuthFailed,
    SignatureInvalid,
    BudgetExhausted,
    Cancelled,
    InternalError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorSource {
    Scanner,
    Utf8,
    Lexer,
    Syntax,
    Cursor,
    Dom,
    SchemaCompile,
    SchemaVm,
    Decode,
    Encode,
    Wire,
    Transport,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: ErrorCode,
    pub source: ErrorSource,
    pub byte_offset: u32,
    pub schema_id: u32,
    pub field_id: u32,
    pub detail: &'static str,
}

impl Error {
    /// Creates an error with zero schema and field identifiers until context
    /// is supplied by the caller with `with_schema`.
    pub const fn new(
        code: ErrorCode,
        source: ErrorSource,
        byte_offset: u32,
        detail: &'static str,
    ) -> Self {
        Self { code, source, byte_offset, schema_id: 0, field_id: 0, detail }
    }

    pub const fn with_schema(self, schema_id: u32, field_id: u32) -> Self {
        Self { schema_id, field_id, ..self }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: {:?} at byte {} (schema {}, field {})",
            self.source, self.code, self.byte_offset, self.schema_id, self.field_id,
        )?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl core::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn constructors_are_const_and_preserve_context() {
        const BASE: Error = Error::new(
            ErrorCode::InvalidNumber, ErrorSource::Lexer, 17, "expected digit",
        );
        const CONTEXT: Error = BASE.with_schema(21, 34);
        assert_eq!(BASE.schema_id, 0);
        assert_eq!(BASE.field_id, 0);
        assert_eq!(CONTEXT.code, ErrorCode::InvalidNumber);
        assert_eq!(CONTEXT.source, ErrorSource::Lexer);
        assert_eq!(CONTEXT.byte_offset, 17);
        assert_eq!(CONTEXT.detail, "expected digit");
        assert_eq!((CONTEXT.schema_id, CONTEXT.field_id), (21, 34));
        let replaced = CONTEXT.with_schema(u32::MAX, 0);
        assert_eq!((replaced.schema_id, replaced.field_id), (u32::MAX, 0));
    }

    #[test]
    fn display_contains_code_source_location_and_detail() {
        let error = Error::new(
            ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 9, "expected string",
        ).with_schema(4, 2);
        assert_eq!(
            error.to_string(),
            "SchemaVm: TypeMismatch at byte 9 (schema 4, field 2): expected string",
        );
        assert_eq!(error.clone(), error);
    }

    #[test]
    fn display_handles_empty_detail_and_maximum_offset() {
        let error = Error::new(ErrorCode::UnexpectedEof, ErrorSource::Syntax, u32::MAX, "");
        assert_eq!(
            error.to_string(),
            "Syntax: UnexpectedEof at byte 4294967295 (schema 0, field 0)",
        );
        let error = Error::new(ErrorCode::InvalidToken, ErrorSource::Lexer, 0, "literal {x} %");
        assert!(error.to_string().ends_with(": literal {x} %"));
    }

    #[test]
    fn core_error_trait_and_result_alias_work_without_std() {
        let error = Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 0, "cancelled");
        let object: &dyn core::error::Error = &error;
        assert!(object.source().is_none());
        let result: Result<u8> = Err(error.clone());
        assert_eq!(result.unwrap_err(), error);
        let success: Result<u8> = Ok(42);
        assert_eq!(success.unwrap(), 42);
    }

    #[test]
    fn all_codes_and_sources_have_distinct_diagnostics() {
        let codes = [
            ErrorCode::UnexpectedEof, ErrorCode::InvalidSyntax, ErrorCode::InvalidToken,
            ErrorCode::TrailingContent, ErrorCode::UnclosedString, ErrorCode::InvalidEscape,
            ErrorCode::LoneSurrogate, ErrorCode::InvalidUtf8, ErrorCode::ControlCharInString,
            ErrorCode::InvalidNumber, ErrorCode::NumberOverflow, ErrorCode::DecimalPrecisionOverflow,
            ErrorCode::DecimalScaleMismatch, ErrorCode::TickMultipleViolation,
            ErrorCode::DepthLimitExceeded, ErrorCode::PayloadTooLarge, ErrorCode::DecodedArenaOverflow,
            ErrorCode::DuplicateObjectKey, ErrorCode::UnknownObjectKey, ErrorCode::MissingRequiredField,
            ErrorCode::TypeMismatch, ErrorCode::ConstraintViolation, ErrorCode::PatternMismatch,
            ErrorCode::FormatMismatch, ErrorCode::UnsupportedSchemaFeature, ErrorCode::SchemaImageInvalid,
            ErrorCode::FramingError, ErrorCode::TransportIncomplete, ErrorCode::AuthFailed,
            ErrorCode::SignatureInvalid, ErrorCode::BudgetExhausted, ErrorCode::Cancelled,
            ErrorCode::InternalError,
        ];
        for (index, code) in codes.iter().enumerate() {
            let diagnostic = Error::new(*code, ErrorSource::Scanner, 0, "").to_string();
            assert!(!diagnostic.is_empty());
            for other in &codes[index + 1..] {
                assert_ne!(diagnostic, Error::new(*other, ErrorSource::Scanner, 0, "").to_string());
            }
        }
        let sources = [
            ErrorSource::Scanner, ErrorSource::Utf8, ErrorSource::Lexer, ErrorSource::Syntax,
            ErrorSource::Cursor, ErrorSource::Dom, ErrorSource::SchemaCompile, ErrorSource::SchemaVm,
            ErrorSource::Decode, ErrorSource::Encode, ErrorSource::Wire, ErrorSource::Transport,
            ErrorSource::Auth,
        ];
        for (index, source) in sources.iter().enumerate() {
            for other in &sources[index + 1..] {
                assert_ne!(
                    Error::new(ErrorCode::InternalError, *source, 0, "").to_string(),
                    Error::new(ErrorCode::InternalError, *other, 0, "").to_string(),
                );
            }
        }
        fn assert_hash<T: core::hash::Hash>() {}
        assert_hash::<ErrorCode>();
        assert_hash::<ErrorSource>();
    }
}
