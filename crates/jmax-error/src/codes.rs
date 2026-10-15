//! Stable names and classifications for canonical ABI error values.

use crate::{ErrorCategory, ErrorCode, ErrorSource};

/// Returns the frozen category for a canonical error code.
pub const fn category(code: ErrorCode) -> ErrorCategory {
    match code {
        ErrorCode::UnexpectedEof
        | ErrorCode::InvalidSyntax
        | ErrorCode::InvalidToken
        | ErrorCode::TrailingContent
        | ErrorCode::UnclosedString => ErrorCategory::Syntax,

        ErrorCode::InvalidEscape
        | ErrorCode::LoneSurrogate
        | ErrorCode::InvalidUtf8
        | ErrorCode::ControlCharInString => ErrorCategory::Encoding,

        ErrorCode::InvalidNumber
        | ErrorCode::NumberOverflow
        | ErrorCode::DecimalPrecisionOverflow
        | ErrorCode::DecimalScaleMismatch
        | ErrorCode::TickMultipleViolation => ErrorCategory::Number,

        ErrorCode::DepthLimitExceeded
        | ErrorCode::PayloadTooLarge
        | ErrorCode::DecodedArenaOverflow
        | ErrorCode::BudgetExhausted => ErrorCategory::Limit,

        ErrorCode::UnsupportedSchemaFeature | ErrorCode::SchemaImageInvalid => {
            ErrorCategory::Schema
        }

        ErrorCode::DuplicateObjectKey
        | ErrorCode::UnknownObjectKey
        | ErrorCode::MissingRequiredField
        | ErrorCode::TypeMismatch
        | ErrorCode::ConstraintViolation
        | ErrorCode::PatternMismatch
        | ErrorCode::FormatMismatch => ErrorCategory::Validation,

        ErrorCode::FramingError => ErrorCategory::Framing,
        ErrorCode::TransportIncomplete => ErrorCategory::Transport,
        ErrorCode::AuthFailed | ErrorCode::SignatureInvalid => ErrorCategory::Authentication,
        ErrorCode::Cancelled => ErrorCategory::Cancelled,
        ErrorCode::InternalError => ErrorCategory::Internal,
    }
}

/// Returns the exact Rust variant identifier for an error code.
pub const fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::UnexpectedEof => "UnexpectedEof",
        ErrorCode::InvalidSyntax => "InvalidSyntax",
        ErrorCode::InvalidToken => "InvalidToken",
        ErrorCode::TrailingContent => "TrailingContent",
        ErrorCode::UnclosedString => "UnclosedString",
        ErrorCode::InvalidEscape => "InvalidEscape",
        ErrorCode::LoneSurrogate => "LoneSurrogate",
        ErrorCode::InvalidUtf8 => "InvalidUtf8",
        ErrorCode::ControlCharInString => "ControlCharInString",
        ErrorCode::InvalidNumber => "InvalidNumber",
        ErrorCode::NumberOverflow => "NumberOverflow",
        ErrorCode::DecimalPrecisionOverflow => "DecimalPrecisionOverflow",
        ErrorCode::DecimalScaleMismatch => "DecimalScaleMismatch",
        ErrorCode::TickMultipleViolation => "TickMultipleViolation",
        ErrorCode::DepthLimitExceeded => "DepthLimitExceeded",
        ErrorCode::PayloadTooLarge => "PayloadTooLarge",
        ErrorCode::DecodedArenaOverflow => "DecodedArenaOverflow",
        ErrorCode::DuplicateObjectKey => "DuplicateObjectKey",
        ErrorCode::UnknownObjectKey => "UnknownObjectKey",
        ErrorCode::MissingRequiredField => "MissingRequiredField",
        ErrorCode::TypeMismatch => "TypeMismatch",
        ErrorCode::ConstraintViolation => "ConstraintViolation",
        ErrorCode::PatternMismatch => "PatternMismatch",
        ErrorCode::FormatMismatch => "FormatMismatch",
        ErrorCode::UnsupportedSchemaFeature => "UnsupportedSchemaFeature",
        ErrorCode::SchemaImageInvalid => "SchemaImageInvalid",
        ErrorCode::FramingError => "FramingError",
        ErrorCode::TransportIncomplete => "TransportIncomplete",
        ErrorCode::AuthFailed => "AuthFailed",
        ErrorCode::SignatureInvalid => "SignatureInvalid",
        ErrorCode::BudgetExhausted => "BudgetExhausted",
        ErrorCode::Cancelled => "Cancelled",
        ErrorCode::InternalError => "InternalError",
    }
}

/// Returns the exact Rust variant identifier for an error source.
pub const fn source_name(source: ErrorSource) -> &'static str {
    match source {
        ErrorSource::Scanner => "Scanner",
        ErrorSource::Utf8 => "Utf8",
        ErrorSource::Lexer => "Lexer",
        ErrorSource::Syntax => "Syntax",
        ErrorSource::Cursor => "Cursor",
        ErrorSource::Dom => "Dom",
        ErrorSource::SchemaCompile => "SchemaCompile",
        ErrorSource::SchemaVm => "SchemaVm",
        ErrorSource::Decode => "Decode",
        ErrorSource::Encode => "Encode",
        ErrorSource::Wire => "Wire",
        ErrorSource::Transport => "Transport",
        ErrorSource::Auth => "Auth",
    }
}

/// Returns the exact Rust variant identifier for an error category.
pub const fn category_name(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Syntax => "Syntax",
        ErrorCategory::Encoding => "Encoding",
        ErrorCategory::Number => "Number",
        ErrorCategory::Limit => "Limit",
        ErrorCategory::Schema => "Schema",
        ErrorCategory::Validation => "Validation",
        ErrorCategory::Framing => "Framing",
        ErrorCategory::Transport => "Transport",
        ErrorCategory::Authentication => "Authentication",
        ErrorCategory::Cancelled => "Cancelled",
        ErrorCategory::Internal => "Internal",
    }
}

/// Reports whether the error means more input may be needed to finish parsing.
pub const fn is_incomplete(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::UnexpectedEof | ErrorCode::TransportIncomplete)
}

/// Reports whether more transport bytes may allow progress.
///
/// This classification does not authorize replaying a request.
pub const fn is_retryable(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::TransportIncomplete)
}
