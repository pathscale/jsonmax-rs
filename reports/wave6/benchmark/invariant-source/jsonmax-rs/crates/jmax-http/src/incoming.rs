//! Strict boundaries for complete, decoded HTTP JSON bodies.
//!
//! Transport framing and content codings must already have been removed by the
//! adapter. These checks preserve the request target and body bytes exactly;
//! schema validation remains the responsibility of the supplied validator.

use super::*;
use alloc::sync::Arc;

/// Checks decoded body size, bounded header storage, and JSON framing metadata.
///
/// Missing Content-Length is permitted because callers separately establish
/// completion. Repeated framing or representation fields are always rejected,
/// even when their values agree. Unknown fields may be repeated.
pub fn check_headers(headers: &[Header], body_len: usize, limits: HttpLimits) -> Result<()> {
    let syntax = limits.effective_syntax()?;
    let body_length = u32::try_from(body_len)
        .map_err(|_| http_error(ErrorCode::PayloadTooLarge, "HTTP body exceeds ABI length range"))?;
    if body_length > syntax.max_payload_bytes {
        return Err(http_error(ErrorCode::PayloadTooLarge, "HTTP body exceeds payload limit"));
    }
    if headers.len() > usize::from(limits.max_headers) {
        return Err(http_error(ErrorCode::BudgetExhausted, "HTTP header count exceeds limit"));
    }

    let mut storage = 2usize; // The final CRLF, including for an empty header set.
    let mut content_length = None;
    let mut content_type = None;
    let mut content_encoding = None;
    for header in headers {
        storage = header_storage(storage, header.name().len(), header.value().len())?;
        let bytes = u32::try_from(storage)
            .map_err(|_| http_error(ErrorCode::PayloadTooLarge, "HTTP header storage exceeds limit"))?;
        if bytes > limits.max_header_bytes {
            return Err(http_error(ErrorCode::PayloadTooLarge, "HTTP header storage exceeds limit"));
        }
        let name = header.name();
        let field = if name.eq_ignore_ascii_case("content-length") {
            &mut content_length
        } else if name.eq_ignore_ascii_case("content-type") {
            &mut content_type
        } else if name.eq_ignore_ascii_case("content-encoding") {
            &mut content_encoding
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(http_error(ErrorCode::FramingError, "transfer framing metadata must be removed"));
        } else {
            continue;
        };
        if field.replace(header.value()).is_some() {
            return Err(http_error(ErrorCode::FramingError, "duplicate HTTP representation header"));
        }
    }

    if let Some(value) = content_length {
        let declared = parse_content_length(value)?;
        if declared > body_len {
            return Err(http_error(ErrorCode::TransportIncomplete, "HTTP body is shorter than Content-Length"));
        }
        if declared < body_len {
            return Err(http_error(ErrorCode::FramingError, "HTTP body exceeds Content-Length"));
        }
    }
    if let Some(value) = content_encoding {
        if !trim_ows(value).eq_ignore_ascii_case("identity") {
            return Err(http_error(ErrorCode::FramingError, "content coding must be decoded by the adapter"));
        }
    }
    let media = content_type
        .ok_or_else(|| http_error(ErrorCode::TypeMismatch, "JSON Content-Type is required"))?;
    if !json_media_type(media) {
        return Err(http_error(ErrorCode::TypeMismatch, "unsupported JSON Content-Type"));
    }
    Ok(())
}

fn header_storage(current: usize, name: usize, value: usize) -> Result<usize> {
    current.checked_add(name).and_then(|sum| sum.checked_add(value))
        .and_then(|sum| sum.checked_add(4))
        .ok_or_else(|| http_error(ErrorCode::PayloadTooLarge, "HTTP header storage size overflow"))
}

fn trim_ows(value: &str) -> &str {
    value.trim_matches(|ch| ch == ' ' || ch == '\t')
}

fn parse_content_length(value: &str) -> Result<usize> {
    let value = trim_ows(value);
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
    usize::try_from(length)
        .map_err(|_| http_error(ErrorCode::FramingError, "Content-Length exceeds addressable length"))
}

fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
            | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
}

fn json_media_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let media = trim_ows(parts.next().unwrap_or(""));
    let Some((kind, subtype)) = media.split_once('/') else { return false; };
    if !kind.eq_ignore_ascii_case("application") || !subtype.bytes().all(token) {
        return false;
    }
    let json_subtype = subtype.eq_ignore_ascii_case("json")
        || subtype.len().checked_sub(5).is_some_and(|prefix| {
            prefix > 0 && subtype.get(prefix..).is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
        });
    if !json_subtype {
        return false;
    }
    if let Some(parameter) = parts.next() {
        let Some((name, charset)) = trim_ows(parameter).split_once('=') else { return false; };
        if !trim_ows(name).eq_ignore_ascii_case("charset") {
            return false;
        }
        let charset = trim_ows(charset);
        if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("\"utf-8\"") {
            return false;
        }
    }
    parts.next().is_none()
}

/// Validates a target without DNS, allocation, normalization, or network access.
///
/// Absolute targets require a lowercase HTTP(S) scheme and a DNS-style ASCII
/// host or a bracketed IPv6 address. When `absolute` is false, origin-form
/// targets are accepted too; authority-form and fragment identifiers are not.
pub fn validate_uri(uri: &str, absolute: bool) -> Result<()> {
    if !uri.is_ascii() || uri.bytes().any(|byte| byte <= b' ' || matches!(byte, 0x7f | b'\\' | b'#')) {
        return Err(invalid_uri());
    }
    if !absolute && uri.starts_with('/') {
        if uri.starts_with("//") {
            return Err(invalid_uri());
        }
        return validate_target(uri);
    }
    let remainder = uri.strip_prefix("http://").or_else(|| uri.strip_prefix("https://"))
        .ok_or_else(invalid_uri)?;
    let end = remainder.find(['/', '?']).unwrap_or(remainder.len());
    let authority = &remainder[..end];
    validate_authority(authority)?;
    validate_target(&remainder[end..])
}

fn invalid_uri() -> Error {
    http_error(ErrorCode::ConstraintViolation, "invalid HTTP request URI")
}

fn validate_authority(authority: &str) -> Result<()> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']').ok_or_else(invalid_uri)?;
        if host.parse::<core::net::Ipv6Addr>().is_err() {
            return Err(invalid_uri());
        }
        if !suffix.is_empty() {
            validate_port(suffix.strip_prefix(':').ok_or_else(invalid_uri)?)?;
        }
        return Ok(());
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if host.is_empty() || host.len() > 253 {
        return Err(invalid_uri());
    }
    let labels = host.strip_suffix('.').unwrap_or(host);
    for label in labels.split('.') {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-')
            || !label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid_uri());
        }
    }
    if let Some(port) = port {
        validate_port(port)?;
    }
    Ok(())
}

fn validate_port(port: &str) -> Result<()> {
    if port.is_empty() {
        return Err(invalid_uri());
    }
    let mut number = 0u16;
    for byte in port.bytes() {
        if !byte.is_ascii_digit() {
            return Err(invalid_uri());
        }
        number = number.checked_mul(10)
            .and_then(|number| number.checked_add(u16::from(byte - b'0')))
            .ok_or_else(invalid_uri)?;
    }
    if number == 0 { Err(invalid_uri()) } else { Ok(()) }
}

fn unreserved_or_subdelimiter(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\''
            | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=')
}

fn validate_target(target: &str) -> Result<()> {
    if !target.is_empty() && !target.starts_with('/') && !target.starts_with('?') {
        return Err(invalid_uri());
    }
    let bytes = target.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            // Subtraction establishes that advancing over this triplet cannot
            // overflow and that both subsequent bytes are in bounds.
            if bytes.len() - index < 3 || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(invalid_uri());
            }
            index += 3;
            continue;
        }
        if !unreserved_or_subdelimiter(byte) && !matches!(byte, b':' | b'@' | b'/' | b'?') {
            return Err(invalid_uri());
        }
        // The first question mark separates the query; later ones are legal
        // query data. Both regions otherwise share this allowed character set.
        index += 1;
    }
    Ok(())
}

/// Checks completion, JSON response status, headers, and decoded body size.
/// Incomplete exchanges fail before status or representation checks.
pub fn check_response(response: &IncomingResponse, limits: HttpLimits) -> Result<()> {
    limits.effective_syntax()?;
    if response.completion != BodyCompletion::Complete {
        return Err(http_error(ErrorCode::TransportIncomplete, "HTTP response body is incomplete"));
    }
    if !(100..=599).contains(&response.status) {
        return Err(http_error(ErrorCode::ConstraintViolation, "invalid HTTP response status"));
    }
    if !(200..=299).contains(&response.status) {
        return Err(http_error(ErrorCode::ConstraintViolation, "HTTP response status is not successful"));
    }
    if matches!(response.status, 204 | 205) {
        return Err(http_error(ErrorCode::TypeMismatch, "HTTP response status has no JSON body"));
    }
    check_headers(&response.headers, response.body.len(), limits)
}

/// Checks an incoming complete request, allowing origin-form or absolute URIs.
pub fn check_request(request: &IncomingRequest, limits: HttpLimits) -> Result<()> {
    limits.effective_syntax()?;
    if request.completion != BodyCompletion::Complete {
        return Err(http_error(ErrorCode::TransportIncomplete, "HTTP request body is incomplete"));
    }
    if request.method == HttpMethod::Head {
        return Err(http_error(ErrorCode::UnsupportedSchemaFeature, "HEAD has no JSON body boundary"));
    }
    validate_uri(&request.uri, false)?;
    check_headers(&request.headers, request.body.len(), limits)
}

/// Checks transport metadata, then validates the exact response bytes and schema.
pub fn validate_response(
    response: IncomingResponse,
    validator: &dyn BodyValidator,
    schema_fingerprint: u64,
    limits: HttpLimits,
) -> Result<ValidatedBody> {
    check_response(&response, limits)?;
    validate_body(response.body, validator, schema_fingerprint, limits)
}

/// Decodes under `T`'s schema and retains the original proven backing owners.
pub fn decode_response<T: JsonDecode + SchemaProvider>(
    response: IncomingResponse,
    validator: &dyn BodyValidator,
    limits: HttpLimits,
) -> Result<ParsedMessage<T>> {
    limits.effective_syntax()?;
    let validated = validate_response(response, validator, T::schema_fingerprint(), limits)?;
    let value = T::decode_slots(validated.slots())?;
    let ValidatedSlots { payload, decoded, slots, proof } = validated.into_slots();
    drop(slots);
    // SAFETY: validate_body checked the exact original bytes, proof lengths,
    // schema fingerprint, and decoded arena cap. The trusted JsonDecode trait
    // produced values from those validated slots under T's exact schema and
    // preserves their reference invariants. These owners and proof are moved
    // unchanged, so all references continue to resolve in their proven buffers.
    Ok(unsafe { ParsedMessage::new_proven(payload, decoded.into_boxed_slice(), value, proof) })
}

/// Preflights transport metadata before submitting owned validation and decode.
///
/// Nagoya supplies admission and cooperative cancellation. Its result wrapper
/// holds the reservation through result retention. A preflight failure drops
/// the supplied scratch lease without contacting the executor.
pub fn decode_on<E, V, T>(
    executor: &E,
    validator: Arc<V>,
    response: IncomingResponse,
    limits: HttpLimits,
    lease: ScratchLease,
    cancellation: CancellationToken,
) -> Result<E::Task<Admitted<ParsedMessage<T>>>>
where
    E: CpuExecutor,
    V: ParseValidator,
    T: JsonDecode + SchemaProvider + Send + 'static,
{
    let syntax = limits.effective_syntax()?;
    check_response(&response, limits)?;
    let job = ParseJob { payload: response.body, limits: syntax, cancellation };
    jmax_nagoya::submit::submit_typed::<E, V, T>(executor, validator, job, lease)
}
