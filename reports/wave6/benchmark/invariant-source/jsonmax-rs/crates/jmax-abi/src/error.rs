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
