//! Retained native utilities with checked borrows and canonical ABI types.
//!
//! Resolving a reference checks it against the caller's selected storage. It
//! does not establish provenance, interpret JSON escapes, or issue a validation
//! proof. Endpoint lookup borrows the model without changing its ordering.

use alloc::string::String;
use jmax_abi::{
    buffer::Payload,
    error::{Error, ErrorCode, ErrorSource, Result},
    span::SpanSource,
};

pub use jmax_abi::endpoint::{
    EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec,
};
pub use jmax_abi::span::{Span32, StrRef};
pub use jmax_abi::traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider};

/// Borrow the selected string after checking its range, UTF-8, and scalar count.
///
/// Scalar counts measure Unicode scalar values, including combining characters,
/// rather than UTF-8 bytes or graphemes. Only the referenced bytes are examined;
/// unrelated bytes in either buffer need not contain valid UTF-8 or JSON.
/// Invalid ranges and counts use `ConstraintViolation`; invalid UTF-8 uses
/// `InvalidUtf8`. All errors have `Decode` source. UTF-8 offsets are relative to
/// the selected raw payload or decoded arena, respectively.
pub fn resolve_str<'a>(
    payload: &'a Payload,
    decoded: &'a [u8],
    reference: StrRef,
) -> Result<&'a str> {
    let owner = match reference.source {
        SpanSource::RawPayload => payload.as_slice(),
        SpanSource::DecodedArena => decoded,
    };
    let bytes = checked_bytes(owner, reference.span)?;
    let text = core::str::from_utf8(bytes).map_err(|error| {
        // The checked span bounds valid_up_to and its absolute offset by u32.
        // Saturating diagnostics also avoid truncation on wider host targets.
        let relative = u32::try_from(error.valid_up_to()).unwrap_or(u32::MAX);
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Decode,
            reference.span.offset.saturating_add(relative),
            "compatibility string is not valid UTF-8",
        )
    })?;
    if u32::try_from(text.chars().count()).ok() != Some(reference.scalar_len) {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Decode,
            reference.span.offset,
            "compatibility string scalar count does not match reference",
        ));
    }
    Ok(text)
}

/// Borrow raw bytes after checking 32-bit end arithmetic and payload bounds.
///
/// Empty spans at the payload end are valid. Raw bytes need not be UTF-8, align
/// with a string boundary, or constitute JSON. An invalid span returns
/// `ConstraintViolation` with `Decode` source and the span's starting offset.
pub fn checked_raw_span(payload: &Payload, span: Span32) -> Result<&[u8]> {
    checked_bytes(payload.as_slice(), span)
}

fn checked_bytes(bytes: &[u8], span: Span32) -> Result<&[u8]> {
    span.slice(bytes).ok_or_else(|| {
        Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Decode,
            span.offset,
            "compatibility span is out of bounds or its end overflows",
        )
    })
}

/// Find one endpoint using exact, case-sensitive service and endpoint names.
///
/// Empty or absent names return `MissingRequiredField`. Duplicate matching
/// service names are ambiguous even if only one service contains the endpoint;
/// duplicate matching endpoint names within the selected service are likewise
/// rejected with `DuplicateObjectKey`. Unrelated names are not validated by
/// this lookup. All errors have `Decode` source.
pub fn find_endpoint<'a>(
    services: &'a [ServiceSpec],
    service: &str,
    endpoint: &str,
) -> Result<&'a EndpointSchemaSpec> {
    if service.is_empty() || endpoint.is_empty() {
        return Err(Error::new(
            ErrorCode::MissingRequiredField,
            ErrorSource::Decode,
            0,
            "service and endpoint lookup names must be nonempty",
        ));
    }
    let mut matches = services.iter().filter(|item| item.name == service);
    let selected = matches.next().ok_or_else(|| {
        Error::new(
            ErrorCode::MissingRequiredField,
            ErrorSource::Decode,
            0,
            "service lookup name was not found",
        )
    })?;
    if matches.next().is_some() {
        return Err(Error::new(
            ErrorCode::DuplicateObjectKey,
            ErrorSource::Decode,
            0,
            "service lookup name is ambiguous",
        ));
    }
    let mut matches = selected.endpoints.iter().filter(|item| item.name == endpoint);
    let selected = matches.next().ok_or_else(|| {
        Error::new(
            ErrorCode::MissingRequiredField,
            ErrorSource::Decode,
            0,
            "endpoint lookup name was not found",
        )
    })?;
    if matches.next().is_some() {
        return Err(Error::new(
            ErrorCode::DuplicateObjectKey,
            ErrorSource::Decode,
            0,
            "endpoint lookup name is ambiguous",
        ));
    }
    Ok(selected)
}

/// Validate an ASCII header value and return an exact, owned copy.
///
/// Horizontal tab and printable ASCII are permitted, including empty values
/// and leading or trailing spaces. All other controls, DEL, and non-ASCII
/// bytes return `InvalidToken` with `Encode` source at the first invalid byte.
/// This validates a value only; it performs no escaping or authentication.
/// Allocation failure returns `BudgetExhausted` with `Encode` source.
pub fn encode_header_value(value: &str) -> Result<String> {
    if let Some(offset) = value.bytes().position(|byte| {
        byte != b'\t' && !(b' '..=b'~').contains(&byte)
    }) {
        return Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Encode,
            u32::try_from(offset).unwrap_or(u32::MAX),
            "header value contains a control or non-ASCII byte",
        ));
    }
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Encode,
            0,
            "header value allocation failed",
        )
    })?;
    output.push_str(value);
    Ok(output)
}
