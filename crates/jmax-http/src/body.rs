//! Zero-copy adoption of de-framed HTTP request/response bodies. These helpers
//! establish ownership and bounds; incoming metadata checks establish completion.

use super::*;
use alloc::sync::Arc;
use bytes::Bytes;
use jmax_abi::span::Span32;

/// Adopt a transport Bytes allocation without copying body bytes.
pub fn receive_bytes(bytes: Bytes) -> Payload {
    Payload::Bytes(bytes)
}

/// Adopt a generic immutable transport allocation without a wrapper allocation.
pub fn receive_arc_owner<T: AsRef<[u8]> + Send + Sync + 'static>(owner: Arc<T>) -> Result<Payload> {
    Payload::from_arc_owner(owner)
}

/// Retain a checked body-only view into a transport allocation. Headers and any
/// remaining pipelined bytes stay outside the visible JSON payload. Arc-backed
/// views pin their original allocation, which admission accounts in full.
pub fn subview(payload: &Payload, span: Span32) -> Result<Payload> {
    payload.try_slice(span)
}

/// Adopt and slice a generic Arc-backed transport allocation in one operation.
pub fn receive_arc_subview<T: AsRef<[u8]> + Send + Sync + 'static>(
    owner: Arc<T>,
    span: Span32,
) -> Result<Payload> {
    subview(&receive_arc_owner(owner)?, span)
}

/// Adopt a Bytes transport allocation and expose only its body range.
pub fn receive_bytes_subview(bytes: Bytes, span: Span32) -> Result<Payload> {
    subview(&receive_bytes(bytes), span)
}

/// Check response completion and representation before moving the exact body
/// owner into a CPU job. No references to the response or transport escape.
pub fn response_job(
    response: IncomingResponse,
    limits: HttpLimits,
    cancellation: CancellationToken,
) -> Result<ParseJob> {
    incoming::check_response(&response, limits)?;
    Ok(ParseJob {
        payload: response.body,
        limits: limits.effective_syntax()?,
        cancellation,
    })
}

/// Check request metadata and authenticate the exact bytes before CPU handoff.
/// The host must retain routing metadata separately; only the body enters the
/// CPU job, and the consumed request's other fields are dropped here.
pub fn request_job(
    request: IncomingRequest,
    limits: HttpLimits,
    verifier: Option<&dyn WebhookVerifier>,
    cancellation: CancellationToken,
) -> Result<ParseJob> {
    incoming::check_request(&request, limits)?;
    if let Some(verifier) = verifier {
        verifier.verify(&request.headers, request.body.as_slice())?;
    }
    Ok(ParseJob {
        payload: request.body,
        limits: limits.effective_syntax()?,
        cancellation,
    })
}
