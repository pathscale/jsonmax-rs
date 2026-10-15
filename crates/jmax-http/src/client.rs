//! Typed, complete-body calls through a host-provided nago-http adapter.
//!
//! Request preflight runs before the adapter is invoked. The adapter's future
//! is awaited directly, preserving its cancellation and deadline behavior.
//! Response validation and decoding run on the polling task; callers needing
//! CPU admission should use `incoming::decode_on` explicitly.

use super::*;

/// Sends an already validated JSON request exactly once and decodes its reply.
///
/// Publicly assembled requests must satisfy the outgoing framing contract,
/// including an absolute HTTP(S) URI and one canonical Content-Length. The
/// request's proven bytes are transferred unchanged to the transport. They are
/// neither encoded again nor validated against the response schema.
///
/// Transport, validator, and decoder errors retain their original metadata.
/// Redirects and unsuccessful or bodyless response statuses are errors under
/// the incoming JSON response policy. No retry or redirect is performed here.
pub async fn call<T, R>(
    transport: &T,
    validator: &dyn BodyValidator,
    request: OutgoingRequest,
    limits: HttpLimits,
) -> Result<ParsedMessage<R>>
where
    T: HttpTransport,
    R: JsonDecode + SchemaProvider,
{
    preflight(&request, limits)?;
    let response = transport.send(request).await?;
    incoming::decode_response::<R>(response, validator, limits)
}

/// Encodes and validates `value` under `Q`'s schema, then calls the transport.
///
/// The outgoing builder owns header normalization and bounded encoding. Its
/// resulting body is sent without modification; `R`'s schema is used only for
/// the response. Work begins when the returned future is polled.
pub async fn call_json<T, Q, R>(
    transport: &T,
    validator: &dyn BodyValidator,
    method: HttpMethod,
    uri: &str,
    headers: Vec<Header>,
    value: &Q,
    limits: HttpLimits,
) -> Result<ParsedMessage<R>>
where
    T: HttpTransport,
    Q: JsonEncode + SchemaProvider,
    R: JsonDecode + SchemaProvider,
{
    let request = outgoing::request(method, uri, headers, value, validator, limits)?;
    call::<T, R>(transport, validator, request, limits).await
}

/// Checks existing evidence without issuing a proof or changing the message.
fn preflight(request: &OutgoingRequest, limits: HttpLimits) -> Result<()> {
    let syntax = limits.effective_syntax()?;
    if request.method == HttpMethod::Head {
        return Err(http_error(
            ErrorCode::UnsupportedSchemaFeature,
            "HEAD is not supported by the JSON body client",
        ));
    }
    incoming::validate_uri(&request.uri, true)?;
    incoming::check_headers(&request.headers, request.body.payload().len(), limits)?;

    // Incoming checks establish uniqueness, JSON media type, decimal range,
    // and equality to the body length. Outgoing messages additionally require
    // an explicit length in its shortest decimal form, without whitespace.
    let length = request.headers.iter()
        .find(|header| header.name().eq_ignore_ascii_case("content-length"))
        .ok_or_else(|| http_error(
            ErrorCode::FramingError,
            "outgoing JSON request requires Content-Length",
        ))?
        .value()
        .as_bytes();
    if length.is_empty()
        || !length.iter().all(u8::is_ascii_digit)
        || (length.len() > 1 && length[0] == b'0')
    {
        return Err(http_error(
            ErrorCode::FramingError,
            "outgoing Content-Length is not canonical decimal",
        ));
    }

    let slots = request.body.slots();
    check_proof(slots, request.body.proof().schema_fingerprint(), request.body.payload().as_slice())?;
    if slots.decoded.len() > syntax.max_decoded_bytes as usize {
        return Err(http_error(
            ErrorCode::DecodedArenaOverflow,
            "outgoing body exceeds decoded arena limit",
        ));
    }
    Ok(())
}
