//! Native owned/borrowed parsing and typed decoding entry points.

use alloc::{sync::Arc, vec::Vec};

#[cfg(feature = "std")]
use std::cell::RefCell;

use super::{
    BorrowedDocument, Decoded, Document, DomScratch, Error, ErrorCode, ErrorSource, JsonDecode,
    NumericPolicy, ParseOptions, Payload, Result, SchemaProvider, Scratch, ValidatedDocument,
    ValidationProof, facade_error,
};

/// Copies input into an owned payload. Use `parse_borrowed` to retain the slice
/// without copying, or transfer a Payload/Vec/Arc owner to an owned parser.
pub fn parse_slice(input: &[u8], options: ParseOptions) -> Result<Document> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    parse_payload(Payload::from_slice(input), options)
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
}

/// Returns a native tape and retains the original payload owner. The result
/// takes the final tape/decoded buffers; call `Document::recycle` to reuse them.
pub fn parse_payload_with_scratch(
    input: Payload,
    options: ParseOptions,
    scratch: &mut DomScratch,
) -> Result<Document> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    jmax_dom::build::parse_with_numeric_into(input, options.limits, options.numeric, scratch)?;
    scratch.take_document().ok_or_else(|| {
        facade_error(
            ErrorCode::InternalError,
            ErrorSource::Cursor,
            "native DOM result is missing",
        )
    })
}

/// Parses directly over the caller's immutable slice, with no input copy.
/// The result cannot outlive that slice:
///
/// ```compile_fail
/// # fn example() -> jsonmax::Result<()> {
/// let document;
/// {
///     let input = Vec::from(b"{}".as_slice());
///     document = jsonmax::parse::parse_borrowed(&input, jsonmax::ParseOptions::default())?;
/// }
/// let _ = document.raw();
/// # Ok(())
/// # }
/// ```
pub fn parse_borrowed<'a>(input: &'a [u8], options: ParseOptions) -> Result<BorrowedDocument<'a>> {
    parse_borrowed_with_scratch(input, options, &mut DomScratch::new())
}

/// Borrows input and takes the final tape/decoded storage from scratch. Return
/// it with `BorrowedDocument::recycle` before the next parse to reuse capacity.
pub fn parse_borrowed_with_scratch<'a>(
    input: &'a [u8],
    options: ParseOptions,
    scratch: &mut DomScratch,
) -> Result<BorrowedDocument<'a>> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    jmax_dom::build::parse_borrowed_with_numeric_and_scratch(
        input,
        options.limits,
        options.numeric,
        scratch,
    )
}

/// Retains the actual Arc owner, including Arc<Vec<u8>>, without copying bytes.
pub fn parse_arc_owner<T: AsRef<[u8]> + Send + Sync + 'static>(
    owner: Arc<T>,
    options: ParseOptions,
) -> Result<Document> {
    parse_payload(Payload::from_arc_owner(owner)?, options)
}

/// Explicitly allocates legacy parallel tables; native parsing never does so.
pub fn to_legacy_document(document: &Document) -> Result<ValidatedDocument> {
    document.to_legacy()
}

/// Checks mutable legacy layout, then reparses its retained payload into an
/// immutable native tape under the requested syntax and numeric policies.
pub fn from_legacy_document(
    document: ValidatedDocument,
    options: ParseOptions,
) -> Result<Document> {
    document.validate_layout()?;
    parse_payload(document.payload, options)
}

/// Strict syntax/numeric validation over the original slice, without a DOM or
/// slot output. Unselected interiors and trailing bytes are fully checked.
pub fn validate_borrowed(
    input: &[u8],
    options: ParseOptions,
    scratch: &mut Scratch,
) -> Result<ValidationProof> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    jmax_syntax::validate_borrowed(input, options.limits, Some(options.numeric), scratch)
}

pub fn validate_syntax(input: &[u8], options: ParseOptions) -> Result<()> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    #[cfg(feature = "std")]
    {
        return SYNTAX_REUSE.with(|cell| {
            if let Ok(mut reuse) = cell.try_borrow_mut() {
                let SyntaxReuse { validation, skip } = &mut *reuse;
                jmax_syntax::validate_borrowed_reusing(
                    input,
                    options.limits,
                    Some(options.numeric),
                    validation,
                    skip,
                )
                .map(|_| ())
            } else {
                validate_syntax_fresh(input, options)
            }
        });
    }
    #[cfg(not(feature = "std"))]
    validate_syntax_fresh(input, options)
}

fn validate_syntax_fresh(input: &[u8], options: ParseOptions) -> Result<()> {
    let mut validation = Scratch::default();
    let mut skip = jmax_syntax::SkipScratch::default();
    jmax_syntax::validate_borrowed_reusing(
        input,
        options.limits,
        Some(options.numeric),
        &mut validation,
        &mut skip,
    )
    .map(|_| ())
}

#[cfg(feature = "std")]
struct SyntaxReuse {
    validation: Scratch,
    skip: jmax_syntax::SkipScratch,
}

#[cfg(feature = "std")]
std::thread_local! {
    static SYNTAX_REUSE: RefCell<SyntaxReuse> = RefCell::new(SyntaxReuse {
        validation: Scratch::default(),
        skip: jmax_syntax::SkipScratch::default(),
    });
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
    let schema =
        super::endpoint::compile_schema(&T::schema_ir(), super::CompileLimits::default_limits())?;
    decode_payload_with_schema_options(&input, &schema, options)
}

/// Decode with a schema compiled by the caller. Compilation is never timed here.
pub fn from_slice_with_schema<T: JsonDecode + SchemaProvider>(
    input: &[u8],
    schema: &super::CompiledSchema,
) -> Result<Decoded<T>> {
    decode_payload_with_schema(&Payload::from_slice(input), schema)
}
pub fn decode_payload_with_schema<T: JsonDecode + SchemaProvider>(
    input: &Payload,
    schema: &super::CompiledSchema,
) -> Result<Decoded<T>> {
    decode_payload_with_schema_options(input, schema, ParseOptions::default())
}
pub fn decode_payload_with_schema_options<T: JsonDecode + SchemaProvider>(
    input: &Payload,
    schema: &super::CompiledSchema,
    options: ParseOptions,
) -> Result<Decoded<T>> {
    check_type_schema::<T>(schema)?;
    let slots = super::endpoint::decode_slots_payload(input.clone(), schema, options)?;
    let value = T::decode_slots(&slots)?;
    Ok(Decoded { slots, value })
}

/// Strictly validates/extracts and decodes without copying input into a Payload.
/// Derived decoders implement `JsonDecodeBorrowed`; custom decoders can implement
/// it using the checked AOT reader. Allocations for owned output fields belong to T.
pub fn from_borrowed<T: super::JsonDecodeBorrowed + SchemaProvider + 'static>(
    input: &[u8],
    options: ParseOptions,
) -> Result<T> {
    #[cfg(feature = "std")]
    let schema = super::endpoint::cached_schema::<T>()?;
    #[cfg(not(feature = "std"))]
    let schema =
        super::endpoint::compile_schema(&T::schema_ir(), super::CompileLimits::default_limits())?;
    from_borrowed_with_schema(input, &schema, options)
}

/// Caller-compiled decoding over the original immutable input slice.
pub fn from_borrowed_with_schema<T: super::JsonDecodeBorrowed + SchemaProvider>(
    input: &[u8],
    schema: &super::CompiledSchema,
    options: ParseOptions,
) -> Result<T> {
    from_borrowed_with_schema_into(input, schema, options, &mut ValidationScratch::default())
}

/// Reuses validation, decoded-arena and slot storage without copying raw input.
pub fn from_borrowed_with_schema_into<T: super::JsonDecodeBorrowed + SchemaProvider>(
    input: &[u8],
    schema: &super::CompiledSchema,
    options: ParseOptions,
    scratch: &mut ValidationScratch,
) -> Result<T> {
    check_type_schema::<T>(schema)?;
    let slots = super::endpoint::decode_slots_borrowed_into(input, schema, options, scratch)?;
    T::decode_borrowed(jmax_schema_aot::BorrowedSlotReader::new(
        &slots,
        schema.fingerprint(),
    )?)
}

pub use super::endpoint::ValidationScratch;
pub use jmax_dom::build::ParseScratch;

/// Returned document borrows scratch until its next reset.
pub fn parse_payload_into<'a>(
    input: &Payload,
    options: ParseOptions,
    scratch: &'a mut ParseScratch,
) -> Result<&'a Document> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    jmax_dom::build::parse_with_numeric_into(
        input.clone(),
        options.limits,
        options.numeric,
        scratch,
    )
}

/// Decoded values use owner-relative references. Resolve them against
/// scratch.slots(); those references belong only to the current result.
/// Allocations performed by T::decode_slots itself are controlled by T.
pub fn decode_payload_with_schema_into<T: JsonDecode + SchemaProvider>(
    input: &Payload,
    schema: &super::CompiledSchema,
    scratch: &mut ValidationScratch,
) -> Result<T> {
    check_type_schema::<T>(schema)?;
    let slots =
        super::endpoint::validate_payload_into(input, schema, ParseOptions::default(), scratch)?;
    T::decode_slots(slots)
}
pub fn from_slice_with_schema_into<T: JsonDecode + SchemaProvider>(
    input: &[u8],
    schema: &super::CompiledSchema,
    scratch: &mut ValidationScratch,
) -> Result<T> {
    decode_payload_with_schema_into(&Payload::from_slice(input), schema, scratch)
}
pub fn decode_payload_into<T: JsonDecode + SchemaProvider + 'static>(
    input: &Payload,
    options: ParseOptions,
    scratch: &mut ValidationScratch,
) -> Result<T> {
    #[cfg(feature = "std")]
    let schema = super::endpoint::cached_schema::<T>()?;
    #[cfg(not(feature = "std"))]
    let schema =
        super::endpoint::compile_schema(&T::schema_ir(), super::CompileLimits::default_limits())?;
    check_type_schema::<T>(&schema)?;
    T::decode_slots(super::endpoint::validate_payload_into(
        input, &schema, options, scratch,
    )?)
}
fn check_type_schema<T: SchemaProvider>(schema: &super::CompiledSchema) -> Result<()> {
    if schema.fingerprint() != T::schema_fingerprint() {
        return Err(facade_error(
            ErrorCode::SchemaImageInvalid,
            ErrorSource::SchemaCompile,
            "type schema fingerprint does not match its compiled schema",
        ));
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
    input: &[u8],
    scratch: &mut ValidationScratch,
) -> Result<T> {
    decode_payload_into(
        &Payload::from_slice(input),
        ParseOptions::default(),
        scratch,
    )
}
