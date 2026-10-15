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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use core::cell::Cell;

    // These fixed fixtures each contain exactly one syntactically valid JSON
    // value. No production API accepts an unchecked byte fragment.
    struct Literal(&'static [u8]);

    impl JsonEncode for Literal {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(self.0)
        }
    }

    struct Text<'a>(&'a str);

    impl JsonEncode for Text<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            write_json_string(self.0, sink)
        }
    }

    fn assert_vector(response: &Response<'_>, expected: &str) -> OwnedEnvelope {
        let limit = u32::try_from(expected.len()).unwrap();
        let payload = response_payload(response, limit).unwrap();
        assert_eq!(payload.as_slice(), expected.as_bytes());

        let mut direct = BufferSink::new(limit);
        encode_response(response, &mut direct).unwrap();
        assert_eq!(direct.as_bytes(), expected.as_bytes());

        let encoder: &dyn JsonEncode = response;
        let mut through_trait = BufferSink::new(limit);
        encoder.encode_json(&mut through_trait).unwrap();
        assert_eq!(through_trait.as_bytes(), expected.as_bytes());

        let decoded = envelope::decode_response(payload, EnvelopeLimits::default()).unwrap();
        let view = decoded.view();
        assert_eq!(view.method, None);
        assert_eq!(view.params_span, None);
        assert_eq!(view.result_span.is_some(), matches!(response,
            Response::Immediate { .. } | Response::Stream { .. }));
        assert_eq!(view.error_span.is_some(), matches!(response, Response::Error { .. }));
        decoded
    }

    fn assert_wire_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Wire);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
    }

    #[test]
    fn immediate_and_stream_vectors_cover_zero_and_maximum_identifiers() {
        let null = Literal(b"null");
        let decoded = assert_vector(&Response::Immediate { seq: 0, code: 0, result: &null },
            r#"{"kind":"immediate","seq":0,"code":0,"result":null}"#);
        assert_eq!(decoded.view().kind, EnvelopeKind::EndpointImmediate);
        assert_eq!(decoded.view().seq, Some(0));
        assert_eq!(decoded.view().code, Some(0));
        assert_eq!(decoded.resolve(decoded.view().result_span.unwrap()), Some(&b"null"[..]));

        let result = Literal(br#"{"ok":true,"items":[0,-1,"x",null]}"#);
        let decoded = assert_vector(&Response::Immediate {
            seq: u64::MAX, code: u32::MAX, result: &result,
        }, r#"{"kind":"immediate","seq":18446744073709551615,"code":4294967295,"result":{"ok":true,"items":[0,-1,"x",null]}}"#);
        assert_eq!(decoded.view().seq, Some(u64::MAX));
        assert_eq!(decoded.view().code, Some(u32::MAX));
        assert_eq!(decoded.resolve(decoded.view().result_span.unwrap()), Some(result.0));

        let decoded = assert_vector(&Response::Stream { seq: 0, code: 0, index: 0, result: &null },
            r#"{"kind":"stream","seq":0,"code":0,"index":0,"result":null}"#);
        assert_eq!(decoded.view().kind, EnvelopeKind::EndpointStream);
        let decoded = assert_vector(&Response::Stream {
            seq: u64::MAX, code: u32::MAX, index: u64::MAX, result: &result,
        }, r#"{"kind":"stream","seq":18446744073709551615,"code":4294967295,"index":18446744073709551615,"result":{"ok":true,"items":[0,-1,"x",null]}}"#);
        assert_eq!(decoded.view().seq, Some(u64::MAX));
        assert_eq!(decoded.view().code, Some(u32::MAX));
    }

    #[test]
    fn error_vectors_cover_every_optional_field_combination() {
        let error = RpcError { code: -1, message: "failed", retryable: false };
        for (seq, code, expected) in [
            (None, None, r#"{"kind":"error","error":{"code":-1,"message":"failed","retryable":false}}"#),
            (Some(0), None, r#"{"kind":"error","seq":0,"error":{"code":-1,"message":"failed","retryable":false}}"#),
            (None, Some(0), r#"{"kind":"error","code":0,"error":{"code":-1,"message":"failed","retryable":false}}"#),
            (Some(0), Some(0), r#"{"kind":"error","seq":0,"code":0,"error":{"code":-1,"message":"failed","retryable":false}}"#),
        ] {
            let decoded = assert_vector(&Response::Error { seq, code, error }, expected);
            assert_eq!(decoded.view().kind, EnvelopeKind::EndpointError);
            assert_eq!(decoded.view().seq, seq);
            assert_eq!(decoded.view().code, code);
            assert_eq!(decoded.resolve(decoded.view().error_span.unwrap()),
                Some(&br#"{"code":-1,"message":"failed","retryable":false}"#[..]));
        }
    }

    #[test]
    fn signed_error_code_vectors_include_both_extremes_and_zero() {
        for (code, digits) in [(i32::MIN, "-2147483648"), (-1, "-1"), (0, "0"),
            (1, "1"), (i32::MAX, "2147483647")]
        {
            for retryable in [false, true] {
                let expected = format!(
                    "{{\"kind\":\"error\",\"seq\":18446744073709551615,\"code\":4294967295,\"error\":{{\"code\":{digits},\"message\":\"\",\"retryable\":{retryable}}}}}");
                assert_vector(&Response::Error { seq: Some(u64::MAX), code: Some(u32::MAX),
                    error: RpcError { code, message: "", retryable } }, &expected);
            }
        }
    }

    #[test]
    fn log_vectors_cover_all_levels_and_optional_sequence() {
        for level in ["trace", "debug", "info", "warn", "error"] {
            for seq in [None, Some(0), Some(u64::MAX)] {
                let seq_field = match seq {
                    None => String::new(),
                    Some(seq) => format!(",\"seq\":{seq}"),
                };
                let expected = format!(
                    "{{\"kind\":\"log\"{seq_field},\"level\":\"{level}\",\"message\":\"\"}}");
                let decoded = assert_vector(&Response::Log { seq, level, message: "" }, &expected);
                assert_eq!(decoded.view().kind, EnvelopeKind::EndpointLog);
                assert_eq!(decoded.view().seq, seq);
                assert_eq!(decoded.view().code, None);
            }
        }
    }

    #[test]
    fn forwarded_and_close_vectors_preserve_metadata() {
        for (seq, code, expected) in [
            (0, 0, r#"{"kind":"forwarded","seq":0,"code":0,"target":"worker.1"}"#),
            (u64::MAX, u32::MAX, r#"{"kind":"forwarded","seq":18446744073709551615,"code":4294967295,"target":"worker.1"}"#),
        ] {
            let decoded = assert_vector(&Response::Forwarded { seq, code, target: "worker.1" }, expected);
            assert_eq!(decoded.view().kind, EnvelopeKind::EndpointForwarded);
            assert_eq!(decoded.view().seq, Some(seq));
            assert_eq!(decoded.view().code, Some(code));
        }
        for (seq, expected) in [
            (None, r#"{"kind":"close","reason":""}"#),
            (Some(0), r#"{"kind":"close","seq":0,"reason":""}"#),
            (Some(u64::MAX), r#"{"kind":"close","seq":18446744073709551615,"reason":""}"#),
        ] {
            let decoded = assert_vector(&Response::Close { seq, reason: "" }, expected);
            assert_eq!(decoded.view().kind, EnvelopeKind::EndpointClose);
            assert_eq!(decoded.view().seq, seq);
            assert_eq!(decoded.view().code, None);
        }
        // Nonempty means byte-nonempty, without trimming or normalization.
        assert_vector(&Response::Forwarded { seq: 1, code: 2, target: " " },
            r#"{"kind":"forwarded","seq":1,"code":2,"target":" "}"#);
    }

    #[test]
    fn all_string_positions_escape_controls_and_preserve_unicode() {
        let text = "\u{00}\u{01}\u{02}\u{03}\u{04}\u{05}\u{06}\u{07}\u{08}\u{09}\u{0a}\u{0b}\u{0c}\u{0d}\u{0e}\u{0f}\u{10}\u{11}\u{12}\u{13}\u{14}\u{15}\u{16}\u{17}\u{18}\u{19}\u{1a}\u{1b}\u{1c}\u{1d}\u{1e}\u{1f}\"\\/\u{7f}é中😀\u{2028}\u{2029}";
        let escaped = concat!(
            r#""\u0000\u0001\u0002\u0003\u0004\u0005\u0006\u0007\b\t\n\u000b\f\r\u000e\u000f"#,
            r#"\u0010\u0011\u0012\u0013\u0014\u0015\u0016\u0017\u0018\u0019\u001a\u001b\u001c\u001d\u001e\u001f\"\\/"#,
            "\u{7f}é中😀\u{2028}\u{2029}\"",
        );
        let value = Text(text);
        assert_vector(&Response::Immediate { seq: 1, code: 2, result: &value },
            &format!("{{\"kind\":\"immediate\",\"seq\":1,\"code\":2,\"result\":{escaped}}}"));
        assert_vector(&Response::Stream { seq: 1, code: 2, index: 3, result: &value },
            &format!("{{\"kind\":\"stream\",\"seq\":1,\"code\":2,\"index\":3,\"result\":{escaped}}}"));
        assert_vector(&Response::Error { seq: None, code: None,
            error: RpcError { code: 0, message: text, retryable: false } },
            &format!("{{\"kind\":\"error\",\"error\":{{\"code\":0,\"message\":{escaped},\"retryable\":false}}}}"));
        assert_vector(&Response::Log { seq: None, level: "info", message: text },
            &format!("{{\"kind\":\"log\",\"level\":\"info\",\"message\":{escaped}}}"));
        assert_vector(&Response::Forwarded { seq: 1, code: 2, target: text },
            &format!("{{\"kind\":\"forwarded\",\"seq\":1,\"code\":2,\"target\":{escaped}}}"));
        assert_vector(&Response::Close { seq: None, reason: text },
            &format!("{{\"kind\":\"close\",\"reason\":{escaped}}}"));
    }

    #[test]
    fn nested_results_accept_every_json_value_category_without_reformatting() {
        for bytes in [
            &b"null"[..], &b"true"[..], &b"false"[..], &b"-0"[..],
            &b"1.25e+3"[..], &br#""""#[..], &br#""\uD83D\uDE00""#[..],
            &b"[]"[..], &b"{}"[..], &br#" [ { "x" : [true, null] } ] "#[..],
        ] {
            let result = Literal(bytes);
            let expected = format!("{{\"kind\":\"immediate\",\"seq\":7,\"code\":9,\"result\":{}}}",
                core::str::from_utf8(bytes).unwrap());
            assert_vector(&Response::Immediate { seq: 7, code: 9, result: &result }, &expected);
        }
    }

    #[test]
    fn decimal_writer_vectors_cross_digit_and_machine_integer_boundaries() {
        for (value, expected) in [
            (0, "0"), (1, "1"), (9, "9"), (10, "10"), (99, "99"), (100, "100"),
            (999, "999"), (1_000, "1000"), (65_535, "65535"),
            (4_294_967_295, "4294967295"), (4_294_967_296, "4294967296"),
            (9_223_372_036_854_775_807, "9223372036854775807"),
            (9_223_372_036_854_775_808, "9223372036854775808"),
            (9_999_999_999_999_999_999, "9999999999999999999"),
            (10_000_000_000_000_000_000, "10000000000000000000"),
            (u64::MAX, "18446744073709551615"),
        ] {
            let mut sink = BufferSink::new(20);
            write_u64(value, &mut sink).unwrap();
            assert_eq!(sink.as_bytes(), expected.as_bytes());
        }
    }

    struct UntouchedSink {
        touches: usize,
    }

    impl ByteSink for UntouchedSink {
        fn reserve(&mut self, _: usize) -> Result<()> {
            self.touches += 1;
            panic!("invalid metadata must be rejected before reserve")
        }
        fn write_bytes(&mut self, _: &[u8]) -> Result<()> {
            self.touches += 1;
            panic!("invalid metadata must be rejected before write")
        }
        fn len(&self) -> usize { 0 }
    }

    #[test]
    fn invalid_metadata_is_rejected_before_any_sink_operation() {
        let mut sink = UntouchedSink { touches: 0 };
        for level in ["", "INFO", "Warn", "warning", "fatal", " info", "info ", "info\0", "é"] {
            let response = Response::Log { seq: Some(0), level, message: "ignored" };
            assert_wire_error(encode_response(&response, &mut sink), ErrorCode::ConstraintViolation);
            assert_wire_error(response.encode_json(&mut sink), ErrorCode::ConstraintViolation);
            assert_wire_error(response_payload(&response, 0), ErrorCode::ConstraintViolation);
        }
        let response = Response::Forwarded { seq: 0, code: 0, target: "" };
        assert_wire_error(encode_response(&response, &mut sink), ErrorCode::ConstraintViolation);
        assert_wire_error(response.encode_json(&mut sink), ErrorCode::ConstraintViolation);
        assert_wire_error(response_payload(&response, u32::MAX), ErrorCode::ConstraintViolation);
        assert_eq!(sink.touches, 0);
        let mut existing = BufferSink::new(6);
        existing.write_bytes(b"prefix").unwrap();
        assert_wire_error(encode_response(&response, &mut existing), ErrorCode::ConstraintViolation);
        assert_eq!(existing.as_bytes(), b"prefix");
    }

    fn responses<'a>(result: &'a dyn JsonEncode) -> [Response<'a>; 6] {
        [
            Response::Immediate { seq: u64::MAX, code: u32::MAX, result },
            Response::Stream { seq: 0, code: 0, index: u64::MAX, result },
            Response::Error { seq: Some(0), code: Some(0),
                error: RpcError { code: i32::MIN, message: "a\n\"é", retryable: true } },
            Response::Log { seq: Some(0), level: "debug", message: "a\n\"é" },
            Response::Forwarded { seq: 0, code: 0, target: "a\n\"é" },
            Response::Close { seq: Some(0), reason: "a\n\"é" },
        ]
    }

    #[test]
    fn every_response_obeys_all_output_limits_including_exact_fit() {
        let result = Text("\u{0}é😀");
        for response in responses(&result) {
            let full = response_payload(&response, u32::MAX).unwrap();
            let length = u32::try_from(full.len()).unwrap();
            for limit in 0..length {
                assert_wire_error(response_payload(&response, limit), ErrorCode::PayloadTooLarge);
                let mut sink = BufferSink::new(limit);
                assert_wire_error(encode_response(&response, &mut sink), ErrorCode::PayloadTooLarge);
                assert!(sink.len() <= limit as usize);
                assert!(full.as_slice().starts_with(sink.as_bytes()));
            }
            assert_eq!(response_payload(&response, length).unwrap().as_slice(), full.as_slice());
            assert_eq!(response_payload(&response, length + 1).unwrap().as_slice(), full.as_slice());
        }
    }

    struct FailingSink {
        bytes: Vec<u8>,
        fail_at: usize,
        writes: usize,
        error: Error,
    }

    impl ByteSink for FailingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            ByteSink::reserve(&mut self.bytes, additional)
        }
        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            let call = self.writes;
            self.writes += 1;
            if call == self.fail_at {
                // Sinks may fail even after accepting part of a write.
                if let Some(byte) = bytes.first() {
                    self.bytes.write_byte(*byte)?;
                }
                return Err(self.error.clone());
            }
            self.bytes.write_bytes(bytes)
        }
        fn len(&self) -> usize { self.bytes.len() }
    }

    #[test]
    fn sink_errors_at_every_write_preserve_metadata_and_stop_encoding() {
        let result = Text("\u{0}\n\"é");
        let error = Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 37,
            "deterministic sink failure").with_schema(19, 23);
        for response in responses(&result) {
            let mut baseline = FailingSink { bytes: Vec::new(), fail_at: usize::MAX,
                writes: 0, error: error.clone() };
            encode_response(&response, &mut baseline).unwrap();
            for fail_at in 0..baseline.writes {
                let mut sink = FailingSink { bytes: Vec::new(), fail_at,
                    writes: 0, error: error.clone() };
                assert_eq!(encode_response(&response, &mut sink).unwrap_err(), error);
                assert_eq!(sink.writes, fail_at + 1);
                assert!(baseline.bytes.starts_with(&sink.bytes));
            }
        }
    }

    struct ObservedEncoder {
        calls: Cell<usize>,
        failure: Option<Error>,
        write_prefix: bool,
    }

    impl JsonEncode for ObservedEncoder {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            // Exercise the reservation contract on the original destination.
            let length = sink.len();
            sink.reserve(4)?;
            assert_eq!(sink.len(), length);
            if self.write_prefix {
                sink.write_bytes(b"null")?;
            }
            if let Some(error) = &self.failure {
                return Err(error.clone());
            }
            if !self.write_prefix {
                sink.write_bytes(b"null")?;
            }
            Ok(())
        }
    }

    #[test]
    fn nested_encoder_is_called_once_and_its_failure_is_not_wrapped() {
        let error = Error::new(ErrorCode::Cancelled, ErrorSource::Encode, 41,
            "deterministic nested failure").with_schema(29, 31);
        for stream in [false, true] {
            for write_prefix in [false, true] {
                let result = ObservedEncoder { calls: Cell::new(0),
                    failure: Some(error.clone()), write_prefix };
                let response = if stream {
                    Response::Stream { seq: 1, code: 2, index: 3, result: &result }
                } else {
                    Response::Immediate { seq: 1, code: 2, result: &result }
                };
                let mut sink = BufferSink::new(256);
                assert_eq!(encode_response(&response, &mut sink).unwrap_err(), error);
                assert_eq!(result.calls.get(), 1);
                let prefix = if stream {
                    r#"{"kind":"stream","seq":1,"code":2,"index":3,"result":"#
                } else {
                    r#"{"kind":"immediate","seq":1,"code":2,"result":"#
                };
                let expected = format!("{prefix}{}", if write_prefix { "null" } else { "" });
                assert_eq!(sink.as_bytes(), expected.as_bytes());
                assert_eq!(response_payload(&response, 256).unwrap_err(), error);
                assert_eq!(result.calls.get(), 2);
            }
            let result = ObservedEncoder { calls: Cell::new(0), failure: None, write_prefix: true };
            let response = if stream {
                Response::Stream { seq: 1, code: 2, index: 3, result: &result }
            } else {
                Response::Immediate { seq: 1, code: 2, result: &result }
            };
            response_payload(&response, 256).unwrap();
            assert_eq!(result.calls.get(), 1);
            let mut sink = BufferSink::new(0);
            assert_wire_error(encode_response(&response, &mut sink), ErrorCode::PayloadTooLarge);
            assert_eq!(result.calls.get(), 1);
        }
    }

    #[test]
    fn direct_and_trait_encoders_append_without_rewriting_existing_bytes() {
        let first = Response::Close { seq: None, reason: "first" };
        let second = Response::Log { seq: None, level: "info", message: "second" };
        let mut sink = BufferSink::new(256);
        sink.write_bytes(b"prefix:").unwrap();
        encode_response(&first, &mut sink).unwrap();
        second.encode_json(&mut sink).unwrap();
        assert_eq!(sink.as_bytes(),
            br#"prefix:{"kind":"close","reason":"first"}{"kind":"log","level":"info","message":"second"}"#);
    }

    #[test]
    fn owned_response_payloads_survive_borrowed_inputs_and_frame_incrementally() {
        let payload = {
            let text = String::from("owned\n😀");
            let result = Text(&text);
            response_payload(&Response::Stream { seq: u64::MAX, code: u32::MAX,
                index: u64::MAX, result: &result }, 256).unwrap()
        };
        let decoded = envelope::decode_response(payload.clone(), EnvelopeLimits::default()).unwrap();
        assert_eq!(decoded.view().kind, EnvelopeKind::EndpointStream);
        assert_eq!(decoded.resolve(decoded.view().result_span.unwrap()),
            Some("\"owned\\n😀\"".as_bytes()));

        let length = u32::try_from(payload.len()).unwrap();
        let limits = FrameLimits { max_payload_bytes: length, allowed_kinds: alloc::vec![0xa5] };
        let frame = Frame { header: FramedHeader { payload_len: length, frame_kind: 0xa5 }, payload };
        let mut bytes = BufferSink::new(length + FRAME_HEADER_BYTES as u32);
        framed::encode_frame(&frame, &limits, &mut bytes).unwrap();
        let mut expected_header = [0u8; FRAME_HEADER_BYTES];
        expected_header[..4].copy_from_slice(&length.to_le_bytes());
        expected_header[4] = 0xa5;
        assert_eq!(&bytes.as_bytes()[..FRAME_HEADER_BYTES], &expected_header);
        assert_eq!(&bytes.as_bytes()[FRAME_HEADER_BYTES..], frame.payload.as_slice());

        let mut decoder = FrameDecoder::new(limits).unwrap();
        for (index, byte) in bytes.as_bytes().iter().enumerate() {
            assert_eq!(decoder.push(core::slice::from_ref(byte)).unwrap(), 1);
            if index + 1 < bytes.len() {
                assert!(decoder.next_frame().unwrap().is_none());
            }
        }
        let received = decoder.next_frame().unwrap().unwrap();
        assert_eq!(received.header, frame.header);
        assert_eq!(received.payload.as_slice(), frame.payload.as_slice());
        let received = envelope::decode_response(received.payload, EnvelopeLimits::default()).unwrap();
        assert_eq!(received.view(), decoded.view());
        assert_eq!(received.payload().as_slice(), decoded.payload().as_slice());
        assert_eq!(decoder.buffered_len(), 0);
        decoder.finish().unwrap();
    }
}
