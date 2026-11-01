//! Bounded JSON encoding and validated, canonically framed HTTP messages.
//!
//! The encoder runs once. The schema validator receives the resulting owner,
//! and its exact validated bytes are moved into the outgoing message unchanged.

use super::*;
use core::fmt::Write;

/// Encodes once into the effective body budget, then validates the actual bytes.
///
/// Encoder and validator failures retain their original metadata. Encoding is
/// not evidence of schema validity: only `validate_body` can bind the returned
/// immutable owner to the validator's proof and the exact schema fingerprint.
pub fn encode_body<T: JsonEncode + SchemaProvider>(
    value: &T,
    validator: &dyn BodyValidator,
    limits: HttpLimits,
) -> Result<ValidatedBody> {
    let syntax = limits.effective_syntax()?;
    let mut sink = BodySink::new(syntax.max_payload_bytes);
    value.encode_json(&mut sink)?;
    validate_body(sink.into_payload(), validator, T::schema_fingerprint(), limits)
}

/// Builds a validated JSON request for an absolute HTTP(S) target.
///
/// HEAD is unsupported by this JSON body API. Supplied representation metadata
/// must describe the encoded bytes faithfully; a supplied length is verified
/// before being replaced with one canonical decimal Content-Length at the end.
pub fn request<T: JsonEncode + SchemaProvider>(
    method: HttpMethod,
    uri: &str,
    headers: Vec<Header>,
    value: &T,
    validator: &dyn BodyValidator,
    limits: HttpLimits,
) -> Result<OutgoingRequest> {
    limits.effective_syntax()?;
    if method == HttpMethod::Head {
        return Err(http_error(
            ErrorCode::UnsupportedSchemaFeature,
            "HEAD has no JSON body boundary",
        ));
    }
    incoming::validate_uri(uri, true)?;
    let body = encode_body(value, validator, limits)?;
    let headers = canonical_headers(headers, body.payload().len(), limits)?;
    let mut owned_uri = String::new();
    owned_uri.try_reserve(uri.len())
        .map_err(|_| http_error(ErrorCode::BudgetExhausted, "HTTP URI allocation failed"))?;
    owned_uri.push_str(uri);
    Ok(OutgoingRequest { method, uri: owned_uri, headers, body })
}

/// Builds a validated JSON success response with canonical framing headers.
///
/// Only statuses 200 through 299 with JSON bodies are accepted. Statuses 204
/// and 205 cannot carry a JSON body. Hosted handlers building typed error
/// responses use their separately validated error-schema body instead.
pub fn response<T: JsonEncode + SchemaProvider>(
    status: u16,
    headers: Vec<Header>,
    value: &T,
    validator: &dyn BodyValidator,
    limits: HttpLimits,
) -> Result<OutgoingResponse> {
    limits.effective_syntax()?;
    if !(200..=299).contains(&status) {
        return Err(http_error(
            ErrorCode::ConstraintViolation,
            "outgoing JSON response status is not successful",
        ));
    }
    if matches!(status, 204 | 205) {
        return Err(http_error(ErrorCode::TypeMismatch, "HTTP response status has no JSON body"));
    }
    let body = encode_body(value, validator, limits)?;
    let headers = canonical_headers(headers, body.payload().len(), limits)?;
    Ok(OutgoingResponse { status, headers, body })
}

/// Preserves all caller fields in order except the verified Content-Length.
fn canonical_headers(
    mut headers: Vec<Header>,
    body_len: usize,
    limits: HttpLimits,
) -> Result<Vec<Header>> {
    let mut supplied_length = None;
    let mut has_type = false;
    for header in &headers {
        if header.name().eq_ignore_ascii_case("content-length") {
            if supplied_length.replace(header.value()).is_some() {
                return Err(http_error(ErrorCode::FramingError, "duplicate Content-Length"));
            }
        } else if header.name().eq_ignore_ascii_case("content-type") {
            has_type = true;
        }
    }
    if let Some(length) = supplied_length {
        verify_supplied_length(length, body_len)?;
    }
    headers.retain(|header| !header.name().eq_ignore_ascii_case("content-length"));

    let additional = if has_type { 1usize } else { 2usize };
    let final_count = headers.len().checked_add(additional)
        .ok_or_else(|| http_error(ErrorCode::BudgetExhausted, "HTTP header count overflow"))?;
    if final_count > usize::from(limits.max_headers) {
        return Err(http_error(ErrorCode::BudgetExhausted, "HTTP header count exceeds limit"));
    }
    headers.try_reserve(additional)
        .map_err(|_| http_error(ErrorCode::BudgetExhausted, "HTTP header allocation failed"))?;
    if !has_type {
        headers.push(Header::new("content-type", JSON_MEDIA_TYPE)?);
    }
    headers.push(length_header(body_len)?);

    // Check the final storage size, including generated fields and CRLF. All
    // supplied type/encoding fields remain present, so the incoming policy
    // also rejects duplicates, unsupported media types, and transfer framing.
    incoming::check_headers(&headers, body_len, limits)?;
    Ok(headers)
}

/// Matches the incoming decimal/OWS policy without changing caller metadata
/// before it is verified. This check deliberately precedes normalization, so
/// malformed or mismatched supplied lengths can never be silently repaired.
fn verify_supplied_length(value: &str, body_len: usize) -> Result<()> {
    let value = value.trim_matches(|ch| ch == ' ' || ch == '\t');
    if value.is_empty() {
        return Err(http_error(ErrorCode::FramingError, "empty Content-Length"));
    }
    let mut length = 0u64;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(http_error(ErrorCode::FramingError, "Content-Length must be unsigned decimal"));
        }
        length = length.checked_mul(10)
            .and_then(|length| length.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| http_error(ErrorCode::FramingError, "Content-Length arithmetic overflow"))?;
    }
    let length = usize::try_from(length)
        .map_err(|_| http_error(ErrorCode::FramingError, "Content-Length exceeds addressable length"))?;
    if length > body_len {
        return Err(http_error(ErrorCode::TransportIncomplete, "HTTP body is shorter than Content-Length"));
    }
    if length < body_len {
        return Err(http_error(ErrorCode::FramingError, "HTTP body exceeds Content-Length"));
    }
    Ok(())
}

fn length_header(body_len: usize) -> Result<Header> {
    let length = u32::try_from(body_len)
        .map_err(|_| http_error(ErrorCode::PayloadTooLarge, "HTTP body exceeds ABI length range"))?;
    // A u32 has at most ten decimal digits. Reserving before formatting keeps
    // this allocation fallible, and formatting never needs to grow the string.
    let mut decimal = String::new();
    decimal.try_reserve(10)
        .map_err(|_| http_error(ErrorCode::BudgetExhausted, "HTTP length allocation failed"))?;
    write!(&mut decimal, "{length}")
        .map_err(|_| http_error(ErrorCode::InternalError, "HTTP length formatting failed"))?;
    Header::new("content-length", &decimal)
}
