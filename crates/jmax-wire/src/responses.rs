//! Streaming encoders for the six endpoint response variants.
//!
//! Application results remain typed `JsonEncode` values. Their implementations
//! must emit exactly one complete JSON value on success. Encoding invokes them
//! once, directly on the destination, without buffering or reparsing their data.

#![forbid(unsafe_code)]

use super::*;

/// Writes one response in canonical field order, without a trailing newline.
///
/// Invalid log levels and empty forwarding targets are rejected before touching
/// the sink. Sink and nested encoder errors propagate unchanged. Either may
/// leave a partial response in the destination, which the caller must discard.
pub fn encode_response(response: &Response<'_>, sink: &mut dyn ByteSink) -> Result<()> {
    encode_response_dialect(response, ResponseDialect::Native, sink)
}

/// Encodes a response using either the native `kind` envelope or the tagged
/// endpoint-libs `type` dialect.
pub fn encode_response_dialect(
    response: &Response<'_>, dialect: ResponseDialect, sink: &mut dyn ByteSink,
) -> Result<()> {
    match response {
        Response::Log { level, .. }
            if !matches!(*level, "trace" | "debug" | "info" | "warn" | "error") =>
        {
            return Err(wire_error(ErrorCode::ConstraintViolation, "unknown log level"));
        }
        Response::Forwarded { target, .. } if target.is_empty() => {
            return Err(wire_error(ErrorCode::ConstraintViolation,
                "forward target must not be empty"));
        }
        _ => {}
    }
    if dialect == ResponseDialect::EndpointLibs {
        return encode_endpoint_libs_response(response, sink);
    }

    match response {
        Response::Immediate { seq, code, result } => {
            sink.write_bytes(b"{\"kind\":\"immediate\"")?;
            write_identifiers(*seq, *code, sink)?;
            sink.write_bytes(b",\"result\":")?;
            result.encode_json(sink)?;
        }
        Response::Stream { seq, code, index, result } => {
            sink.write_bytes(b"{\"kind\":\"stream\"")?;
            write_identifiers(*seq, *code, sink)?;
            sink.write_bytes(b",\"index\":")?;
            write_u64(*index, sink)?;
            sink.write_bytes(b",\"result\":")?;
            result.encode_json(sink)?;
        }
        Response::Error { seq, code, error } => {
            sink.write_bytes(b"{\"kind\":\"error\"")?;
            write_optional_seq(*seq, sink)?;
            if let Some(code) = code {
                sink.write_bytes(b",\"code\":")?;
                write_u64(u64::from(*code), sink)?;
            }
            sink.write_bytes(b",\"error\":{\"code\":")?;
            if error.code < 0 {
                sink.write_byte(b'-')?;
            }
            write_u64(u64::from(error.code.unsigned_abs()), sink)?;
            sink.write_bytes(b",\"message\":")?;
            write_json_string(error.message, sink)?;
            sink.write_bytes(b",\"retryable\":")?;
            sink.write_bytes(if error.retryable { b"true" } else { b"false" })?;
            sink.write_byte(b'}')?;
        }
        Response::Log { seq, level, message } => {
            sink.write_bytes(b"{\"kind\":\"log\"")?;
            write_optional_seq(*seq, sink)?;
            sink.write_bytes(b",\"level\":")?;
            write_json_string(level, sink)?;
            sink.write_bytes(b",\"message\":")?;
            write_json_string(message, sink)?;
        }
        Response::Forwarded { seq, code, target } => {
            sink.write_bytes(b"{\"kind\":\"forwarded\"")?;
            write_identifiers(*seq, *code, sink)?;
            sink.write_bytes(b",\"target\":")?;
            write_json_string(target, sink)?;
        }
        Response::Close { seq, reason } => {
            sink.write_bytes(b"{\"kind\":\"close\"")?;
            write_optional_seq(*seq, sink)?;
            sink.write_bytes(b",\"reason\":")?;
            write_json_string(reason, sink)?;
        }
    }
    sink.write_byte(b'}')
}

fn encode_endpoint_libs_response(response: &Response<'_>, sink: &mut dyn ByteSink) -> Result<()> {
    match response {
        Response::Immediate { seq, result, .. } => {
            sink.write_bytes(b"{\"type\":\"Immediate\",\"seq\":")?;
            write_u64(*seq, sink)?;
            sink.write_bytes(b",\"data\":")?;
            result.encode_json(sink)?;
        }
        Response::Stream { seq, code, index, result } => {
            sink.write_bytes(b"{\"type\":\"Stream\",\"original_seq\":")?;
            write_u64(*seq, sink)?;
            sink.write_bytes(b",\"stream_seq\":")?;
            write_u64(*index, sink)?;
            sink.write_bytes(b",\"stream_code\":")?;
            write_u64(u64::from(*code), sink)?;
            sink.write_bytes(b",\"data\":")?;
            result.encode_json(sink)?;
        }
        Response::Error { seq, error, .. } => {
            sink.write_bytes(b"{\"type\":\"Error\"")?;
            write_optional_seq(*seq, sink)?;
            sink.write_bytes(b",\"code\":")?;
            if error.code < 0 { sink.write_byte(b'-')?; }
            write_u64(u64::from(error.code.unsigned_abs()), sink)?;
            sink.write_bytes(b",\"message\":")?;
            write_json_string(error.message, sink)?;
        }
        Response::Log { seq, level, message } => {
            sink.write_bytes(b"{\"type\":\"Log\"")?;
            write_optional_seq(*seq, sink)?;
            sink.write_bytes(b",\"level\":")?;
            write_json_string(level, sink)?;
            sink.write_bytes(b",\"message\":")?;
            write_json_string(message, sink)?;
        }
        Response::Forwarded { seq, code, target } => {
            sink.write_bytes(b"{\"type\":\"Forwarded\",\"seq\":")?;
            write_u64(*seq, sink)?;
            sink.write_bytes(b",\"code\":")?;
            write_u64(u64::from(*code), sink)?;
            sink.write_bytes(b",\"target\":")?;
            write_json_string(target, sink)?;
        }
        Response::Close { seq, reason } => {
            sink.write_bytes(b"{\"type\":\"Close\"")?;
            write_optional_seq(*seq, sink)?;
            sink.write_bytes(b",\"reason\":")?;
            write_json_string(reason, sink)?;
        }
    }
    sink.write_byte(b'}')
}

/// Encodes into an owned, bounded payload, returning no partial result on error.
///
/// The limit includes all envelope punctuation and escaped UTF-8 string bytes.
/// A zero limit cannot hold a response and yields `PayloadTooLarge` for valid
/// metadata. Allocation failures are reported by `BufferSink` as
/// `BudgetExhausted`.
pub fn response_payload(response: &Response<'_>, max_bytes: u32) -> Result<Payload> {
    let mut sink = BufferSink::new(max_bytes);
    encode_response(response, &mut sink)?;
    Ok(sink.into_payload())
}

/// Encodes a bounded response payload in the requested protocol dialect.
pub fn response_payload_dialect(
    response: &Response<'_>, dialect: ResponseDialect, max_bytes: u32,
) -> Result<Payload> {
    let mut sink = BufferSink::new(max_bytes);
    encode_response_dialect(response, dialect, &mut sink)?;
    Ok(sink.into_payload())
}

impl JsonEncode for Response<'_> {
    fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
        encode_response(self, sink)
    }
}

fn write_identifiers(seq: u64, code: u32, sink: &mut dyn ByteSink) -> Result<()> {
    write_optional_seq(Some(seq), sink)?;
    sink.write_bytes(b",\"code\":")?;
    write_u64(u64::from(code), sink)
}

fn write_optional_seq(seq: Option<u64>, sink: &mut dyn ByteSink) -> Result<()> {
    if let Some(seq) = seq {
        sink.write_bytes(b",\"seq\":")?;
        write_u64(seq, sink)?;
    }
    Ok(())
}

// A u64 has at most 20 decimal digits. Write backwards in a fixed stack buffer
// so all integer fields are exact and require neither allocation nor formatting.
fn write_u64(mut value: u64, sink: &mut dyn ByteSink) -> Result<()> {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return sink.write_bytes(&digits[start..]);
        }
    }
}
