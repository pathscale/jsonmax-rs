//! Complete-body hosted and webhook boundaries supplied by the host.
//!
//! Authentication sees the original headers and bytes before schema validation.
//! Dispatch transfers ownership synchronously and releases only a fully checked
//! JSON response. Listener, runtime, and signature policy remain with the host.

use super::*;

/// Accepts a complete JSON request, optionally verifying its original bytes.
///
/// Transport metadata and limits are checked before invoking the verifier. A
/// verification error prevents both validation and handler execution. With no
/// verifier, the host explicitly chooses an unverified boundary. The validator
/// must preserve the exact payload, including whitespace and escape spelling.
/// Request metadata is moved into the result without normalization.
pub fn accept_request(
    request: IncomingRequest,
    verifier: Option<&dyn WebhookVerifier>,
    validator: &dyn BodyValidator,
    schema_fingerprint: u64,
    limits: HttpLimits,
) -> Result<HostedRequest> {
    incoming::check_request(&request, limits)?;
    if let Some(verifier) = verifier {
        verifier.verify(&request.headers, request.body.as_slice())?;
    }
    let body = validate_body(request.body, validator, schema_fingerprint, limits)?;
    Ok(HostedRequest {
        method: request.method,
        uri: request.uri,
        headers: request.headers,
        body,
    })
}

/// Accepts a request, invokes the handler once, and checks its complete response.
///
/// The handler chooses its response schema. Statuses 400 through 599 with valid
/// JSON error bodies are returned to the host as response data. Redirects may
/// also carry JSON, but statuses 204, 205, and 304 cannot. Every response must
/// already contain exactly one JSON Content-Type and a canonical Content-Length.
/// Invalid output is a local error; nothing is sent or rewritten here. Verifier,
/// validator, and handler errors retain their original diagnostic metadata.
pub fn dispatch(
    request: IncomingRequest,
    verifier: Option<&dyn WebhookVerifier>,
    validator: &dyn BodyValidator,
    schema_fingerprint: u64,
    handler: &dyn HostedHandler,
    limits: HttpLimits,
) -> Result<OutgoingResponse> {
    let request = accept_request(request, verifier, validator, schema_fingerprint, limits)?;
    let response = handler.handle(request)?;
    check_handler_response(&response, limits)?;
    Ok(response)
}

/// Checks existing validation evidence without selecting a schema or issuing
/// another proof. The immutable ValidatedBody carries its validator's owners.
fn check_handler_response(response: &OutgoingResponse, limits: HttpLimits) -> Result<()> {
    let syntax = limits.effective_syntax()?;
    if !(200..=599).contains(&response.status) {
        return Err(http_error(
            ErrorCode::ConstraintViolation,
            "hosted JSON response status is outside 200 through 599",
        ));
    }
    if matches!(response.status, 204 | 205 | 304) {
        return Err(http_error(
            ErrorCode::TypeMismatch,
            "hosted response status has no JSON body",
        ));
    }

    // This establishes unique representation headers, a JSON media type,
    // length equality, decoded transport framing, and bounded byte storage.
    incoming::check_headers(&response.headers, response.body.payload().len(), limits)?;
    let length = response.headers.iter()
        .find(|header| header.name().eq_ignore_ascii_case("content-length"))
        .ok_or_else(|| http_error(
            ErrorCode::FramingError,
            "hosted JSON response requires Content-Length",
        ))?
        .value()
        .as_bytes();
    if length.is_empty()
        || !length.iter().all(u8::is_ascii_digit)
        || (length.len() > 1 && length[0] == b'0')
    {
        return Err(http_error(
            ErrorCode::FramingError,
            "hosted response Content-Length is not canonical decimal",
        ));
    }

    let body = &response.body;
    check_proof(body.slots(), body.proof().schema_fingerprint(), body.payload())?;
    // After the ownership check, the proof length equals the arena's actual
    // length. Comparing u32 values avoids narrowing either configured limit.
    if body.proof().decoded_len() > syntax.max_decoded_bytes {
        return Err(http_error(
            ErrorCode::DecodedArenaOverflow,
            "hosted response exceeds decoded arena limit",
        ));
    }
    Ok(())
}
