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

#[cfg(test)]
mod tests {
    use super::*;

    const ERROR_CODES: [(ErrorCode, ErrorCategory, &str); 33] = [
        (ErrorCode::UnexpectedEof, ErrorCategory::Syntax, "UnexpectedEof"),
        (ErrorCode::InvalidSyntax, ErrorCategory::Syntax, "InvalidSyntax"),
        (ErrorCode::InvalidToken, ErrorCategory::Syntax, "InvalidToken"),
        (ErrorCode::TrailingContent, ErrorCategory::Syntax, "TrailingContent"),
        (ErrorCode::UnclosedString, ErrorCategory::Syntax, "UnclosedString"),
        (ErrorCode::InvalidEscape, ErrorCategory::Encoding, "InvalidEscape"),
        (ErrorCode::LoneSurrogate, ErrorCategory::Encoding, "LoneSurrogate"),
        (ErrorCode::InvalidUtf8, ErrorCategory::Encoding, "InvalidUtf8"),
        (ErrorCode::ControlCharInString, ErrorCategory::Encoding, "ControlCharInString"),
        (ErrorCode::InvalidNumber, ErrorCategory::Number, "InvalidNumber"),
        (ErrorCode::NumberOverflow, ErrorCategory::Number, "NumberOverflow"),
        (ErrorCode::DecimalPrecisionOverflow, ErrorCategory::Number, "DecimalPrecisionOverflow"),
        (ErrorCode::DecimalScaleMismatch, ErrorCategory::Number, "DecimalScaleMismatch"),
        (ErrorCode::TickMultipleViolation, ErrorCategory::Number, "TickMultipleViolation"),
        (ErrorCode::DepthLimitExceeded, ErrorCategory::Limit, "DepthLimitExceeded"),
        (ErrorCode::PayloadTooLarge, ErrorCategory::Limit, "PayloadTooLarge"),
        (ErrorCode::DecodedArenaOverflow, ErrorCategory::Limit, "DecodedArenaOverflow"),
        (ErrorCode::DuplicateObjectKey, ErrorCategory::Validation, "DuplicateObjectKey"),
        (ErrorCode::UnknownObjectKey, ErrorCategory::Validation, "UnknownObjectKey"),
        (ErrorCode::MissingRequiredField, ErrorCategory::Validation, "MissingRequiredField"),
        (ErrorCode::TypeMismatch, ErrorCategory::Validation, "TypeMismatch"),
        (ErrorCode::ConstraintViolation, ErrorCategory::Validation, "ConstraintViolation"),
        (ErrorCode::PatternMismatch, ErrorCategory::Validation, "PatternMismatch"),
        (ErrorCode::FormatMismatch, ErrorCategory::Validation, "FormatMismatch"),
        (ErrorCode::UnsupportedSchemaFeature, ErrorCategory::Schema, "UnsupportedSchemaFeature"),
        (ErrorCode::SchemaImageInvalid, ErrorCategory::Schema, "SchemaImageInvalid"),
        (ErrorCode::FramingError, ErrorCategory::Framing, "FramingError"),
        (ErrorCode::TransportIncomplete, ErrorCategory::Transport, "TransportIncomplete"),
        (ErrorCode::AuthFailed, ErrorCategory::Authentication, "AuthFailed"),
        (ErrorCode::SignatureInvalid, ErrorCategory::Authentication, "SignatureInvalid"),
        (ErrorCode::BudgetExhausted, ErrorCategory::Limit, "BudgetExhausted"),
        (ErrorCode::Cancelled, ErrorCategory::Cancelled, "Cancelled"),
        (ErrorCode::InternalError, ErrorCategory::Internal, "InternalError"),
    ];

    #[test]
    fn classifies_and_names_every_canonical_code() {
        for &(code, expected_category, expected_name) in &ERROR_CODES {
            assert_eq!(category(code), expected_category, "category for {expected_name}");
            assert_eq!(code_name(code), expected_name, "name for {expected_name}");
            assert!(!code_name(code).is_empty());
        }
        assert_eq!(ERROR_CODES.len(), 33);
    }

    #[test]
    fn code_names_are_unique_and_classification_is_stable() {
        for (index, &(code, expected_category, name)) in ERROR_CODES.iter().enumerate() {
            assert_eq!(category(code), expected_category);
            for &(other_code, _, other_name) in &ERROR_CODES[index + 1..] {
                assert_ne!(name, other_name);
                assert_ne!(code_name(code), code_name(other_code));
            }
        }
    }

    #[test]
    fn names_every_canonical_source_and_category() {
        let sources = [
            (ErrorSource::Scanner, "Scanner"),
            (ErrorSource::Utf8, "Utf8"),
            (ErrorSource::Lexer, "Lexer"),
            (ErrorSource::Syntax, "Syntax"),
            (ErrorSource::Cursor, "Cursor"),
            (ErrorSource::Dom, "Dom"),
            (ErrorSource::SchemaCompile, "SchemaCompile"),
            (ErrorSource::SchemaVm, "SchemaVm"),
            (ErrorSource::Decode, "Decode"),
            (ErrorSource::Encode, "Encode"),
            (ErrorSource::Wire, "Wire"),
            (ErrorSource::Transport, "Transport"),
            (ErrorSource::Auth, "Auth"),
        ];
        for &(source, expected) in &sources {
            assert_eq!(source_name(source), expected);
            assert!(!source_name(source).is_empty());
        }
        for (index, &(_, name)) in sources.iter().enumerate() {
            for &(_, other_name) in &sources[index + 1..] {
                assert_ne!(name, other_name);
            }
        }

        let categories = [
            (ErrorCategory::Syntax, "Syntax"),
            (ErrorCategory::Encoding, "Encoding"),
            (ErrorCategory::Number, "Number"),
            (ErrorCategory::Limit, "Limit"),
            (ErrorCategory::Schema, "Schema"),
            (ErrorCategory::Validation, "Validation"),
            (ErrorCategory::Framing, "Framing"),
            (ErrorCategory::Transport, "Transport"),
            (ErrorCategory::Authentication, "Authentication"),
            (ErrorCategory::Cancelled, "Cancelled"),
            (ErrorCategory::Internal, "Internal"),
        ];
        for &(category, expected) in &categories {
            assert_eq!(category_name(category), expected);
            assert!(!category_name(category).is_empty());
        }
        for (index, &(_, name)) in categories.iter().enumerate() {
            for &(_, other_name) in &categories[index + 1..] {
                assert_ne!(name, other_name);
            }
        }
    }

    #[test]
    fn incomplete_and_retryable_classifications_match_contract() {
        for &(code, _, _) in &ERROR_CODES {
            assert_eq!(
                is_incomplete(code),
                matches!(code, ErrorCode::UnexpectedEof | ErrorCode::TransportIncomplete),
                "incomplete classification for {}",
                code_name(code),
            );
            assert_eq!(
                is_retryable(code),
                matches!(code, ErrorCode::TransportIncomplete),
                "retryable classification for {}",
                code_name(code),
            );
        }
        assert!(is_incomplete(ErrorCode::UnexpectedEof));
        assert!(!is_retryable(ErrorCode::UnexpectedEof));
        assert!(is_incomplete(ErrorCode::TransportIncomplete));
        assert!(is_retryable(ErrorCode::TransportIncomplete));
        assert!(!is_incomplete(ErrorCode::InvalidSyntax));
        assert!(!is_retryable(ErrorCode::InvalidSyntax));
    }

    #[test]
    fn const_classification_vectors_cover_boundary_categories() {
        const FIRST: ErrorCategory = category(ErrorCode::UnexpectedEof);
        const LAST: ErrorCategory = category(ErrorCode::InternalError);
        const FIRST_NAME: &str = code_name(ErrorCode::UnexpectedEof);
        const LAST_NAME: &str = code_name(ErrorCode::InternalError);
        const INCOMPLETE: bool = is_incomplete(ErrorCode::TransportIncomplete);
        const RETRYABLE: bool = is_retryable(ErrorCode::TransportIncomplete);
        assert_eq!(FIRST, ErrorCategory::Syntax);
        assert_eq!(LAST, ErrorCategory::Internal);
        assert_eq!(FIRST_NAME, "UnexpectedEof");
        assert_eq!(LAST_NAME, "InternalError");
        assert!(INCOMPLETE && RETRYABLE);
        assert_eq!(category_name(category(ErrorCode::BudgetExhausted)), "Limit");
        assert_eq!(category_name(category(ErrorCode::SignatureInvalid)), "Authentication");
    }
}
