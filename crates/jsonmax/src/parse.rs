//! Owned parsing and typed decoding entry points for the native facade.

use alloc::vec::Vec;

use jmax_abi::syntax::ScanStep;
use jmax_cursor::SkipMode;

use super::{
    facade_error, CompileLimits, Decoded, Document, Error, ErrorCode, ErrorSource, JsonDecode,
    NumericPolicy, ParseOptions, Payload, Result, SchemaProvider,
};

/// Copies borrowed input into an owner-bearing payload and parses it with the
/// default syntax and numeric policies.
pub fn parse_slice(input: &[u8]) -> Result<Document> {
    parse_payload(Payload::from_slice(input), ParseOptions::default())
}

/// Adopts an owned byte vector and parses it without another input copy.
pub fn parse_owned(input: Vec<u8>, options: ParseOptions) -> Result<Document> {
    parse_payload(Payload::from_vec(input), options)
}

/// Parses a payload while retaining its original owner in the resulting
/// document.
pub fn parse_payload(input: Payload, options: ParseOptions) -> Result<Document> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    jmax_dom::build::parse_with_numeric(input, options.limits, options.numeric)
        .map(Document::into_validated)
}

/// Validates complete JSON syntax and numeric policy without constructing a
/// DOM. Every cursor event is consumed through `Finished`, so trailing content
/// and malformed unselected subtrees are always checked.
pub fn validate_syntax(input: &[u8], options: ParseOptions) -> Result<()> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;

    let payload = Payload::from_slice(input);
    let mut cursor = jmax_cursor::walk::from_payload_with_numeric(
        payload,
        options.limits,
        SkipMode::Strict,
        options.numeric,
    )?;
    loop {
        match jmax_cursor::walk::next(&mut cursor)? {
            ScanStep::Emit(_) => {}
            ScanStep::Finished => return Ok(()),
            ScanStep::NeedMore => {
                return Err(Error::new(
                    ErrorCode::InternalError, ErrorSource::Cursor,
                    cursor.state().byte_offset,
                    "complete-input cursor requested additional bytes",
                ));
            }
        }
    }
}

/// Parses and strictly decodes a typed value using its native schema.
pub fn from_slice<T: JsonDecode + SchemaProvider>(input: &[u8]) -> Result<Decoded<T>> {
    decode_payload(Payload::from_slice(input), ParseOptions::default())
}

/// Compiles and checks `T`'s schema, validates the complete payload against it,
/// and invokes the decoder only after native schema validation succeeds.
pub fn decode_payload<T: JsonDecode + SchemaProvider>(
    input: Payload,
    options: ParseOptions,
) -> Result<Decoded<T>> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;

    let schema_ir = T::schema_ir();
    let schema = super::endpoint::compile_schema(
        &schema_ir,
        CompileLimits::default_limits(),
    )?;
    if schema.fingerprint() != T::schema_fingerprint() {
        return Err(facade_error(
            ErrorCode::SchemaImageInvalid,
            ErrorSource::SchemaCompile,
            "type schema fingerprint does not match its compiled schema",
        ));
    }

    // Endpoint validation owns the proof publication and performs complete
    // syntax, numeric, and schema checks while retaining the payload and arena.
    let slots = super::endpoint::validate_payload(input, &schema, options)?;
    let value = T::decode_slots(&slots)?;
    Ok(Decoded { slots, value })
}

fn ensure_payload_length(length: usize) -> Result<()> {
    if u32::try_from(length).is_err() {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Lexer,
            u32::MAX,
            "payload exceeds the 32-bit ABI length limit",
        ));
    }
    Ok(())
}

fn validate_policy_configuration(policy: NumericPolicy) -> Result<()> {
    if policy.max_exponent_abs < 0 {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            0,
            "numeric exponent limit cannot be negative",
        ));
    }
    Ok(())
}

