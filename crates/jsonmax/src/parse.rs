//! Owned parsing and typed decoding entry points for the native facade.

use alloc::vec::Vec;

use jmax_abi::syntax::ScanStep;
use jmax_cursor::SkipMode;

use super::{
    facade_error, Decoded, Document, Error, ErrorCode, ErrorSource, JsonDecode,
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
pub fn from_slice<T: JsonDecode + SchemaProvider + 'static>(input: &[u8]) -> Result<Decoded<T>> {
    decode_payload(Payload::from_slice(input), ParseOptions::default())
}

/// Caches and checks `T`'s compiled schema in std builds, validates the payload,
/// and invokes the decoder only after native schema validation succeeds.
pub fn decode_payload<T: JsonDecode + SchemaProvider + 'static>(
    input: Payload,
    options: ParseOptions,
) -> Result<Decoded<T>> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;

    #[cfg(feature = "std")]
    let schema = super::endpoint::cached_schema::<T>()?;
    #[cfg(not(feature = "std"))]
    let schema = super::endpoint::compile_schema(&T::schema_ir(), super::CompileLimits::default_limits())?;
    decode_payload_with_schema_options(&input, &schema, options)
}

/// Decode with a schema compiled by the caller. Compilation is never timed here.
pub fn from_slice_with_schema<T: JsonDecode + SchemaProvider>(
    input: &[u8], schema: &super::CompiledSchema) -> Result<Decoded<T>> {
    decode_payload_with_schema(&Payload::from_slice(input), schema)
}
pub fn decode_payload_with_schema<T: JsonDecode + SchemaProvider>(
    input: &Payload, schema: &super::CompiledSchema) -> Result<Decoded<T>> {
    decode_payload_with_schema_options(input, schema, ParseOptions::default())
}
pub fn decode_payload_with_schema_options<T: JsonDecode + SchemaProvider>(
    input: &Payload, schema: &super::CompiledSchema, options: ParseOptions) -> Result<Decoded<T>> {
    check_type_schema::<T>(schema)?;
    let slots = super::endpoint::validate_payload(input.clone(), schema, options)?;
    let value = T::decode_slots(&slots)?;
    Ok(Decoded { slots, value })
}

pub use jmax_dom::build::ParseScratch;
pub use super::endpoint::ValidationScratch;

/// Returned document borrows scratch until its next reset.
pub fn parse_payload_into<'a>(input: &Payload, options: ParseOptions,
    scratch: &'a mut ParseScratch) -> Result<&'a Document> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    jmax_dom::build::parse_with_numeric_into(input.clone(), options.limits, options.numeric, scratch)
}

/// Decoded values use owner-relative references. Resolve them against
/// scratch.slots(); those references belong only to the current result.
/// Allocations performed by T::decode_slots itself are controlled by T.
pub fn decode_payload_with_schema_into<T: JsonDecode + SchemaProvider>(
    input: &Payload, schema: &super::CompiledSchema,
    scratch: &mut ValidationScratch) -> Result<T> {
    check_type_schema::<T>(schema)?;
    let slots = super::endpoint::validate_payload_into(input, schema, ParseOptions::default(), scratch)?;
    T::decode_slots(slots)
}
pub fn from_slice_with_schema_into<T: JsonDecode + SchemaProvider>(
    input: &[u8], schema: &super::CompiledSchema, scratch: &mut ValidationScratch) -> Result<T> {
    decode_payload_with_schema_into(&Payload::from_slice(input), schema, scratch)
}
pub fn decode_payload_into<T: JsonDecode + SchemaProvider + 'static>(
    input: &Payload, options: ParseOptions, scratch: &mut ValidationScratch) -> Result<T> {
    #[cfg(feature = "std")]
    let schema = super::endpoint::cached_schema::<T>()?;
    #[cfg(not(feature = "std"))]
    let schema = super::endpoint::compile_schema(&T::schema_ir(), super::CompileLimits::default_limits())?;
    check_type_schema::<T>(&schema)?;
    T::decode_slots(super::endpoint::validate_payload_into(input, &schema, options, scratch)?)
}
fn check_type_schema<T: SchemaProvider>(schema: &super::CompiledSchema) -> Result<()> {
    if schema.fingerprint() != T::schema_fingerprint() {
        return Err(facade_error(ErrorCode::SchemaImageInvalid, ErrorSource::SchemaCompile,
            "type schema fingerprint does not match its compiled schema"));
    }
    Ok(())
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


/// Borrowed-input convenience; copying input into Payload still allocates.
/// Use decode_payload_into with a retained Payload for allocation-free input reuse.
pub fn from_slice_into<T: JsonDecode + SchemaProvider + 'static>(
    input: &[u8], scratch: &mut ValidationScratch) -> Result<T> {
    decode_payload_into(&Payload::from_slice(input), ParseOptions::default(), scratch)
}
