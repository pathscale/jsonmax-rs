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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{sync::Arc, task::Wake, vec};
    use core::{
        cell::{Cell, RefCell},
        future::{ready, Ready},
        pin::{pin, Pin},
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };
    use jmax_abi::{FieldId, SchemaFieldIr, SchemaId, SchemaIr, SlotValue};
    use jmax_abi::syntax::ScalarKind;

    const REQUEST_SCHEMA: u64 = 11;
    const RESPONSE_SCHEMA: u64 = 22;
    const TRUE_JSON: &[u8] = br#"{"value":true}"#;
    const FALSE_JSON: &[u8] = br#"{"value":false}"#;
    const PADDED_JSON: &[u8] = b" \n{\"value\":true}\t ";
    const URI: &str = "https://api.example.test:443/v1/value?mode=exact%20bytes";

    #[derive(Debug, PartialEq, Eq)]
    struct Message<const FINGERPRINT: u64>(bool);

    impl<const FINGERPRINT: u64> SchemaProvider for Message<FINGERPRINT> {
        fn schema_ir() -> SchemaIr {
            SchemaIr {
                name: String::from("BooleanMessage"),
                schema_id: SchemaId(u32::try_from(FINGERPRINT).expect("test schema ID fits")),
                required_count: 1,
                reject_unknown: true,
                fields: vec![SchemaFieldIr {
                    field_id: FieldId(0), name: String::from("value"), required: true,
                    scalar_kind: Some(ScalarKind::True), format: None,
                    min_len: None, max_len: None, min_i64: None, max_i64: None,
                }],
            }
        }

        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl<const FINGERPRINT: u64> JsonEncode for Message<FINGERPRINT> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(if self.0 { TRUE_JSON } else { FALSE_JSON })
        }
    }

    impl<const FINGERPRINT: u64> JsonDecode for Message<FINGERPRINT> {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            if slots.proof.schema_fingerprint() != FINGERPRINT {
                return Err(Error::new(
                    ErrorCode::SchemaImageInvalid, ErrorSource::Decode, 3,
                    "test decoder received another schema",
                ));
            }
            match slots.slots.as_slice() {
                [SlotValue::Bool(value)] => Ok(Self(*value)),
                _ => Err(Error::new(
                    ErrorCode::TypeMismatch, ErrorSource::Decode, 9,
                    "test decoder requires one boolean slot",
                )),
            }
        }
    }

    /// Validates a finite set of complete JSON literals, not caller assertions.
    /// The padded form verifies that validated bytes need not be canonical JSON.
    #[derive(Default)]
    struct LiteralValidator {
        calls: RefCell<Vec<(u64, Limits, Payload)>>,
        failure: Option<Error>,
        decoded: &'static [u8],
    }

    impl BodyValidator for LiteralValidator {
        fn validate(&self, payload: Payload, fingerprint: u64, limits: Limits) -> Result<ValidatedSlots> {
            self.calls.borrow_mut().push((fingerprint, limits, payload.clone()));
            if let Some(error) = &self.failure { return Err(error.clone()); }
            if !matches!(fingerprint, REQUEST_SCHEMA | RESPONSE_SCHEMA) {
                return Err(http_error(ErrorCode::SchemaImageInvalid, "unknown test schema"));
            }
            if payload.len() > limits.max_payload_bytes as usize {
                return Err(http_error(ErrorCode::PayloadTooLarge, "test payload limit"));
            }
            if self.decoded.len() > limits.max_decoded_bytes as usize {
                return Err(http_error(ErrorCode::DecodedArenaOverflow, "test arena limit"));
            }
            if limits.max_depth < 1 || limits.max_fields_per_object < 1 || limits.max_string_bytes < 5 {
                return Err(http_error(ErrorCode::ConstraintViolation, "test object limits"));
            }
            let value = match payload.as_slice() {
                TRUE_JSON | PADDED_JSON => true,
                FALSE_JSON => false,
                _ => return Err(Error::new(
                    ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 9,
                    "expected a complete boolean message literal",
                ).with_schema(u32::try_from(fingerprint).expect("known schema"), 0)),
            };
            let payload_len = u32::try_from(payload.len()).expect("literal payload length fits");
            let decoded_len = u32::try_from(self.decoded.len()).expect("test arena length fits");
            assert!(core::str::from_utf8(self.decoded).is_ok());
            // SAFETY: Exact byte matching above validates complete JSON and the
            // sole required boolean field for either registered test schema.
            // Bool(value) is the matching value. The immutable arena is valid
            // UTF-8, contains no referenced slots, and both lengths are checked.
            let proof = unsafe {
                ValidationProof::issue_unchecked(fingerprint, payload_len, decoded_len)
            };
            Ok(ValidatedSlots {
                payload, decoded: self.decoded.to_vec().into_boxed_slice(),
                slots: vec![SlotValue::Bool(value)], proof,
            })
        }
    }

    struct RecordingTransport {
        sent: RefCell<Vec<OutgoingRequest>>,
        reply: RefCell<Option<Result<IncomingResponse>>>,
    }

    impl RecordingTransport {
        fn new(reply: Result<IncomingResponse>) -> Self {
            Self { sent: RefCell::new(Vec::new()), reply: RefCell::new(Some(reply)) }
        }

        fn success() -> Self { Self::new(Ok(response())) }
    }

    impl HttpTransport for RecordingTransport {
        type ResponseFuture<'a> = Ready<Result<IncomingResponse>> where Self: 'a;

        fn send(&self, request: OutgoingRequest) -> Self::ResponseFuture<'_> {
            self.sent.borrow_mut().push(request);
            ready(self.reply.borrow_mut().take().expect("client must send exactly once"))
        }
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    fn ready_result<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(WakeCounter::default()));
        let mut context = Context::from_waker(&waker);
        match pin!(future).as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("deterministic ready transport unexpectedly suspended"),
        }
    }

    fn headers(length: usize) -> Vec<Header> {
        vec![
            Header::new("Content-Type", JSON_MEDIA_TYPE).unwrap(),
            Header::new("Content-Length", &alloc::format!("{length}")).unwrap(),
        ]
    }

    fn response() -> IncomingResponse {
        IncomingResponse {
            status: 200, headers: headers(FALSE_JSON.len()),
            body: Payload::from_slice(FALSE_JSON), completion: BodyCompletion::Complete,
        }
    }

    fn request_with(bytes: &[u8], validator: &dyn BodyValidator) -> OutgoingRequest {
        OutgoingRequest {
            method: HttpMethod::Post,
            uri: String::from(URI),
            headers: headers(bytes.len()),
            body: validate_body(
                Payload::from_slice(bytes), validator, REQUEST_SCHEMA, HttpLimits::default(),
            ).unwrap(),
        }
    }

    fn request() -> OutgoingRequest { request_with(TRUE_JSON, &LiteralValidator::default()) }

    fn invoke(
        transport: &RecordingTransport,
        validator: &dyn BodyValidator,
        request: OutgoingRequest,
        limits: HttpLimits,
    ) -> Result<ParsedMessage<Message<RESPONSE_SCHEMA>>> {
        ready_result(call(transport, validator, request, limits))
    }

    fn assert_preflight_error(request: OutgoingRequest, limits: HttpLimits, code: ErrorCode) {
        let transport = RecordingTransport::success();
        let validator = LiteralValidator::default();
        let error = invoke(&transport, &validator, request, limits).unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Transport);
        assert_eq!(error.byte_offset, 0);
        assert!(transport.sent.borrow().is_empty());
        assert!(validator.calls.borrow().is_empty());
    }

    #[test]
    fn call_preserves_proven_request_and_response_owners() {
        let validator = LiteralValidator { decoded: b"arena", ..LiteralValidator::default() };
        let mut request = request_with(PADDED_JSON, &LiteralValidator::default());
        request.method = HttpMethod::Patch;
        request.headers.push(Header::new("X-Trace", "keep this value").unwrap());
        let original_headers = request.headers.clone();
        let original_proof = request.body.proof();
        let request_owner = request.body.payload().clone();
        let reply = response();
        let response_owner = reply.body.clone();
        let transport = RecordingTransport::new(Ok(reply));
        let parsed = invoke(&transport, &validator, request, HttpLimits::default()).unwrap();

        assert_eq!(parsed.values(), &Message::<RESPONSE_SCHEMA>(false));
        assert_eq!(parsed.raw_bytes(), FALSE_JSON);
        assert_eq!(parsed.raw_bytes().as_ptr(), response_owner.as_slice().as_ptr());
        assert_eq!(parsed.decoded_bytes(), b"arena");
        assert_eq!(parsed.proof().schema_fingerprint(), RESPONSE_SCHEMA);
        let sent = transport.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, HttpMethod::Patch);
        assert_eq!(sent[0].uri, URI);
        assert_eq!(sent[0].headers, original_headers);
        assert_eq!(sent[0].body.proof(), original_proof);
        assert_eq!(sent[0].body.payload().as_slice(), PADDED_JSON);
        assert_eq!(sent[0].body.payload().as_slice().as_ptr(), request_owner.as_slice().as_ptr());
        let calls = validator.calls.borrow();
        assert_eq!(calls.len(), 1, "outgoing proof must not be revalidated");
        assert_eq!(calls[0].0, RESPONSE_SCHEMA);
        assert_eq!(calls[0].2.as_slice(), FALSE_JSON);
        drop(calls);
        drop(sent);
        drop(transport);
        drop(validator);
        assert_eq!(parsed.raw_bytes(), FALSE_JSON);
        assert_eq!(parsed.decoded_bytes(), b"arena");
    }

    #[test]
    fn supported_methods_reach_the_adapter_without_changes() {
        for method in [
            HttpMethod::Get, HttpMethod::Post, HttpMethod::Put, HttpMethod::Patch,
            HttpMethod::Delete, HttpMethod::Options,
        ] {
            let mut request = request();
            request.method = method;
            let transport = RecordingTransport::success();
            invoke(&transport, &LiteralValidator::default(), request, HttpLimits::default()).unwrap();
            assert_eq!(transport.sent.borrow()[0].method, method);
        }
    }

    struct CountedValue<'a> {
        encodes: &'a Cell<usize>,
        failure: Option<Error>,
    }

    impl SchemaProvider for CountedValue<'_> {
        fn schema_ir() -> SchemaIr { Message::<REQUEST_SCHEMA>::schema_ir() }
        fn schema_fingerprint() -> u64 { REQUEST_SCHEMA }
    }

    impl JsonEncode for CountedValue<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.encodes.set(self.encodes.get() + 1);
            if let Some(error) = &self.failure {
                sink.write_bytes(b"{")?;
                return Err(error.clone());
            }
            sink.write_bytes(TRUE_JSON)
        }
    }

    #[test]
    fn call_json_encodes_once_and_uses_separate_request_and_response_schemas() {
        let encodes = Cell::new(0);
        let value = CountedValue { encodes: &encodes, failure: None };
        let transport = RecordingTransport::success();
        let validator = LiteralValidator::default();
        let supplied = vec![
            Header::new("X-First", "1").unwrap(),
            Header::new("Content-Length", " \t0014\t ").unwrap(),
            Header::new("X-First", "2").unwrap(),
        ];
        let parsed = ready_result(call_json::<_, _, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, HttpMethod::Put, URI, supplied, &value, HttpLimits::default(),
        )).unwrap();
        assert_eq!(encodes.get(), 1);
        assert_eq!(parsed.values(), &Message::<RESPONSE_SCHEMA>(false));
        let calls = validator.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, REQUEST_SCHEMA);
        assert_eq!(calls[1].0, RESPONSE_SCHEMA);
        let sent = transport.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, HttpMethod::Put);
        assert_eq!(sent[0].body.payload().as_slice(), TRUE_JSON);
        assert_eq!(sent[0].body.payload().as_slice().as_ptr(), calls[0].2.as_slice().as_ptr());
        assert_eq!(sent[0].headers, vec![
            Header::new("x-first", "1").unwrap(),
            Header::new("x-first", "2").unwrap(),
            Header::new("content-type", JSON_MEDIA_TYPE).unwrap(),
            Header::new("content-length", "14").unwrap(),
        ]);
    }

    #[test]
    fn dropping_unpolled_calls_does_no_work() {
        let transport = RecordingTransport::success();
        let validator = LiteralValidator::default();
        let encodes = Cell::new(0);
        let value = CountedValue { encodes: &encodes, failure: None };
        drop(call::<_, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, request(), HttpLimits::default(),
        ));
        drop(call_json::<_, _, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, HttpMethod::Post, URI, Vec::new(), &value, HttpLimits::default(),
        ));
        assert!(transport.sent.borrow().is_empty());
        assert!(validator.calls.borrow().is_empty());
        assert_eq!(encodes.get(), 0);
    }

    #[test]
    fn invalid_limit_configuration_never_sends() {
        let defaults = HttpLimits::default();
        let mut zero_payload = defaults;
        zero_payload.syntax.max_payload_bytes = 0;
        let mut zero_depth = defaults;
        zero_depth.syntax.max_depth = 0;
        for limits in [
            HttpLimits { max_body_bytes: 0, ..defaults },
            HttpLimits { max_header_bytes: 0, ..defaults },
            HttpLimits { max_header_bytes: 1, ..defaults },
            HttpLimits { max_headers: 0, ..defaults },
            zero_payload, zero_depth,
        ] {
            assert_preflight_error(request(), limits, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn head_is_rejected_before_transport() {
        let mut request = request();
        request.method = HttpMethod::Head;
        assert_preflight_error(request, HttpLimits::default(), ErrorCode::UnsupportedSchemaFeature);
    }

    #[test]
    fn outgoing_requires_a_valid_absolute_http_uri() {
        for uri in [
            "", "/v1/value", "//example.test/value", "ftp://example.test/value",
            "HTTPS://example.test/value", "https:///value", "https://user@example.test/",
            "https://example.test/#fragment", "https://example.test/a b",
            "https://example.test/\\x", "https://example.test/%", "https://example.test/%G0",
            "https://example.test:0/", "https://example.test:65536/", "https://-bad.test/",
            "https://bad..test/", "https://[:::1]/", "https://é.test/",
        ] {
            let mut request = request();
            request.uri = String::from(uri);
            assert_preflight_error(request, HttpLimits::default(), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn valid_absolute_uri_vectors_are_sent_verbatim() {
        for uri in [
            "http://localhost/", "https://example.test", "https://example.test./x?",
            "http://127.0.0.1:1/v", "https://[::1]:65535/a%20b?q=%2F",
            "https://[2001:db8::1]/x;y?x=/a?b&v=1",
        ] {
            let mut request = request();
            request.uri = String::from(uri);
            let transport = RecordingTransport::success();
            invoke(&transport, &LiteralValidator::default(), request, HttpLimits::default()).unwrap();
            assert_eq!(transport.sent.borrow()[0].uri, uri);
        }
    }

    #[test]
    fn public_requests_require_exactly_one_canonical_content_length() {
        for value in ["014", "00014", " 14", "14 ", "\t14\t", "14\t"] {
            let mut request = request();
            request.headers[1] = Header::new("Content-Length", value).unwrap();
            assert_preflight_error(request, HttpLimits::default(), ErrorCode::FramingError);
        }
        let mut missing = request();
        missing.headers.pop();
        assert_preflight_error(missing, HttpLimits::default(), ErrorCode::FramingError);
        for value in ["14", "15"] {
            let mut duplicate = request();
            duplicate.headers.push(Header::new("Content-Length", value).unwrap());
            assert_preflight_error(duplicate, HttpLimits::default(), ErrorCode::FramingError);
        }
    }

    #[test]
    fn malformed_or_mismatching_lengths_never_send() {
        for (value, code) in [
            ("", ErrorCode::FramingError), ("+14", ErrorCode::FramingError),
            ("-14", ErrorCode::FramingError), ("14,14", ErrorCode::FramingError),
            ("1 4", ErrorCode::FramingError), ("14.0", ErrorCode::FramingError),
            ("18446744073709551616", ErrorCode::FramingError),
            ("13", ErrorCode::FramingError), ("0", ErrorCode::FramingError),
            ("15", ErrorCode::TransportIncomplete),
        ] {
            let mut request = request();
            request.headers[1] = Header::new("Content-Length", value).unwrap();
            assert_preflight_error(request, HttpLimits::default(), code);
        }
    }

    #[test]
    fn missing_duplicate_or_incompatible_media_types_never_send() {
        let mut missing = request();
        missing.headers.remove(0);
        assert_preflight_error(missing, HttpLimits::default(), ErrorCode::TypeMismatch);
        let mut duplicate = request();
        duplicate.headers.push(Header::new("Content-Type", JSON_MEDIA_TYPE).unwrap());
        assert_preflight_error(duplicate, HttpLimits::default(), ErrorCode::FramingError);
        for media in [
            "text/json", "application/+json", "application/json-seq", "application/json, text/plain",
            "application/json; charset=latin1", "application/json; other=utf-8",
            "application/json; charset=utf-8; charset=utf-8", "application/json; charset=\"utf-8",
        ] {
            let mut request = request();
            request.headers[0] = Header::new("Content-Type", media).unwrap();
            assert_preflight_error(request, HttpLimits::default(), ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn valid_json_media_types_and_identity_encoding_are_preserved() {
        for media in [
            "application/json", "APPLICATION/JSON", "application/problem+json",
            "application/vnd.example+json; charset=\"UTF-8\"",
            "application/json;\tcharset = utf-8",
        ] {
            let mut request = request();
            request.headers[0] = Header::new("Content-Type", media).unwrap();
            request.headers.push(Header::new("Content-Encoding", "identity").unwrap());
            let original = request.headers.clone();
            let transport = RecordingTransport::success();
            invoke(&transport, &LiteralValidator::default(), request, HttpLimits::default()).unwrap();
            assert_eq!(transport.sent.borrow()[0].headers, original);
        }
    }

    #[test]
    fn transfer_framing_and_unsupported_content_encodings_are_rejected() {
        for (name, value) in [
            ("Transfer-Encoding", "chunked"), ("Transfer-Encoding", "identity"),
            ("Content-Encoding", "gzip"), ("Content-Encoding", "identity, identity"),
        ] {
            let mut request = request();
            request.headers.push(Header::new(name, value).unwrap());
            assert_preflight_error(request, HttpLimits::default(), ErrorCode::FramingError);
        }
        let mut duplicate = request();
        duplicate.headers.push(Header::new("Content-Encoding", "identity").unwrap());
        duplicate.headers.push(Header::new("Content-Encoding", "identity").unwrap());
        assert_preflight_error(duplicate, HttpLimits::default(), ErrorCode::FramingError);
    }

    #[test]
    fn outgoing_header_storage_and_count_boundaries_are_inclusive() {
        let request = request();
        let stored = request.headers.iter().fold(2usize, |sum, header| {
            sum + header.name().len() + header.value().len() + 4
        });
        assert_eq!(stored, 54);
        // The response also has two headers and a two-digit body length.
        let limits = HttpLimits {
            max_header_bytes: u32::try_from(stored).unwrap(), max_headers: 2,
            ..HttpLimits::default()
        };
        invoke(&RecordingTransport::success(), &LiteralValidator::default(), request, limits).unwrap();
        assert_preflight_error(
            self::request(), HttpLimits { max_header_bytes: 53, ..limits }, ErrorCode::PayloadTooLarge,
        );
        assert_preflight_error(
            self::request(), HttpLimits { max_headers: 1, ..limits }, ErrorCode::BudgetExhausted,
        );
    }

    #[test]
    fn body_limit_uses_the_smaller_transport_and_syntax_caps() {
        // A true reply keeps both directions exactly at the 14-byte boundary.
        for use_syntax in [false, true] {
            let mut limits = HttpLimits::default();
            if use_syntax { limits.syntax.max_payload_bytes = 14; }
            else { limits.max_body_bytes = 14; }
            let mut reply = response();
            reply.body = Payload::from_slice(TRUE_JSON);
            reply.headers = headers(TRUE_JSON.len());
            let transport = RecordingTransport::new(Ok(reply));
            let validator = LiteralValidator::default();
            invoke(&transport, &validator, request(), limits).unwrap();
            assert_eq!(validator.calls.borrow()[0].1.max_payload_bytes, 14);
            if use_syntax { limits.syntax.max_payload_bytes = 13; }
            else { limits.max_body_bytes = 13; }
            assert_preflight_error(request(), limits, ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn outgoing_proof_lengths_are_rechecked_before_transport() {
        let mut wrong_payload = request();
        wrong_payload.body.slots.payload = Payload::from_slice(FALSE_JSON);
        wrong_payload.headers = headers(FALSE_JSON.len());
        assert_preflight_error(wrong_payload, HttpLimits::default(), ErrorCode::InternalError);
        let mut wrong_arena = request();
        wrong_arena.body.slots.decoded = vec![b'x'].into_boxed_slice();
        assert_preflight_error(wrong_arena, HttpLimits::default(), ErrorCode::InternalError);
    }

    #[test]
    fn outgoing_decoded_arena_cap_is_inclusive_and_is_not_revalidated() {
        let validator = LiteralValidator { decoded: b"arena", ..LiteralValidator::default() };
        let make_request = || request_with(TRUE_JSON, &validator);
        let mut limits = HttpLimits::default();
        limits.syntax.max_decoded_bytes = 5;
        invoke(
            &RecordingTransport::success(), &LiteralValidator::default(), make_request(), limits,
        ).unwrap();
        limits.syntax.max_decoded_bytes = 4;
        assert_preflight_error(make_request(), limits, ErrorCode::DecodedArenaOverflow);
    }

    #[test]
    fn transport_error_metadata_propagates_without_validation_or_retry() {
        for code in [ErrorCode::Cancelled, ErrorCode::TransportIncomplete, ErrorCode::AuthFailed] {
            let expected = Error::new(code, ErrorSource::Transport, 123, "adapter failure")
                .with_schema(7, 8);
            let transport = RecordingTransport::new(Err(expected.clone()));
            let validator = LiteralValidator::default();
            let error = invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err();
            assert_eq!(error, expected);
            assert_eq!(transport.sent.borrow().len(), 1);
            assert!(validator.calls.borrow().is_empty());
        }
    }

    #[test]
    fn response_status_policy_is_enforced_without_redirect_following() {
        for (status, code) in [
            (0, ErrorCode::ConstraintViolation), (99, ErrorCode::ConstraintViolation),
            (100, ErrorCode::ConstraintViolation), (199, ErrorCode::ConstraintViolation),
            (204, ErrorCode::TypeMismatch), (205, ErrorCode::TypeMismatch),
            (300, ErrorCode::ConstraintViolation), (302, ErrorCode::ConstraintViolation),
            (400, ErrorCode::ConstraintViolation), (599, ErrorCode::ConstraintViolation),
            (600, ErrorCode::ConstraintViolation),
        ] {
            let mut reply = response();
            reply.status = status;
            reply.headers.push(Header::new("Location", "https://other.test/").unwrap());
            let transport = RecordingTransport::new(Ok(reply));
            let validator = LiteralValidator::default();
            let error = invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(transport.sent.borrow().len(), 1);
            assert!(validator.calls.borrow().is_empty());
        }
        for status in [200, 201, 206, 299] {
            let mut reply = response();
            reply.status = status;
            invoke(
                &RecordingTransport::new(Ok(reply)), &LiteralValidator::default(),
                request(), HttpLimits::default(),
            ).unwrap();
        }
    }

    #[test]
    fn incomplete_response_wins_over_status_and_never_reaches_validator() {
        for status in [0, 200, 204, 302, 500] {
            let mut reply = response();
            reply.status = status;
            reply.completion = BodyCompletion::Incomplete;
            reply.headers.clear();
            let transport = RecordingTransport::new(Ok(reply));
            let validator = LiteralValidator::default();
            let error = invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::TransportIncomplete);
            assert!(validator.calls.borrow().is_empty());
            assert_eq!(transport.sent.borrow().len(), 1);
        }
    }

    #[test]
    fn nominal_success_does_not_bypass_response_framing() {
        for (name, value, code) in [
            ("Content-Length", "16", ErrorCode::TransportIncomplete),
            ("Content-Length", "14", ErrorCode::FramingError),
            ("Content-Type", "text/plain", ErrorCode::TypeMismatch),
            ("Transfer-Encoding", "chunked", ErrorCode::FramingError),
            ("Content-Encoding", "gzip", ErrorCode::FramingError),
        ] {
            let mut reply = response();
            match name {
                "Content-Type" => reply.headers[0] = Header::new(name, value).unwrap(),
                "Content-Length" => reply.headers[1] = Header::new(name, value).unwrap(),
                _ => reply.headers.push(Header::new(name, value).unwrap()),
            }
            let transport = RecordingTransport::new(Ok(reply));
            let validator = LiteralValidator::default();
            let error = invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err();
            assert_eq!(error.code, code);
            assert!(validator.calls.borrow().is_empty());
        }
    }

    #[test]
    fn completed_response_may_omit_length_but_must_fit_the_effective_cap() {
        let mut reply = response();
        reply.headers.pop();
        invoke(
            &RecordingTransport::new(Ok(reply)), &LiteralValidator::default(),
            request(), HttpLimits::default(),
        ).unwrap();
        let transport = RecordingTransport::success();
        let validator = LiteralValidator::default();
        let error = invoke(
            &transport, &validator, request(),
            HttpLimits { max_body_bytes: 14, ..HttpLimits::default() },
        ).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(transport.sent.borrow().len(), 1);
        assert!(validator.calls.borrow().is_empty());
    }

    #[test]
    fn empty_malformed_or_trailing_response_json_is_never_fabricated_as_a_value() {
        for bytes in [
            &b""[..], &b"null"[..], &b"{"[..], &b"{\"value\":true"[..],
            &b"{\"value\":true} false"[..], &b"{\"value\":true}{\"value\":false}"[..],
            &b"{\"value\":true,\"value\":false}"[..], &b"{\"value\":\xff}"[..],
        ] {
            let mut reply = response();
            reply.body = Payload::from_slice(bytes);
            reply.headers = headers(bytes.len());
            let transport = RecordingTransport::new(Ok(reply));
            let validator = LiteralValidator::default();
            let error = invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::TypeMismatch);
            assert_eq!(error.source, ErrorSource::SchemaVm);
            assert_eq!(error.byte_offset, 9);
            assert_eq!(transport.sent.borrow().len(), 1);
            assert_eq!(validator.calls.borrow().len(), 1);
            assert_eq!(validator.calls.borrow()[0].2.as_slice(), bytes);
        }
    }

    #[test]
    fn effective_syntax_policy_is_forwarded_without_widening_other_caps() {
        let mut limits = HttpLimits { max_body_bytes: 64, ..HttpLimits::default() };
        limits.syntax.max_payload_bytes = 128;
        limits.syntax.max_decoded_bytes = 0;
        limits.syntax.max_depth = 1;
        limits.syntax.max_fields_per_object = 1;
        limits.syntax.max_string_bytes = 5;
        limits.syntax.max_number_digits = 0;
        limits.syntax.max_array_items = 0;
        let validator = LiteralValidator::default();
        let transport = RecordingTransport::success();
        invoke(&transport, &validator, request(), limits).unwrap();
        let calls = validator.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, Limits { max_payload_bytes: 64, ..limits.syntax });
        assert!(calls[0].1.reject_duplicate_keys);
        assert!(calls[0].1.reject_unknown_keys);
    }

    #[test]
    fn call_json_rejects_invalid_metadata_before_sending() {
        for (method, uri, supplied, expected) in [
            (HttpMethod::Head, URI, Vec::new(), ErrorCode::UnsupportedSchemaFeature),
            (HttpMethod::Post, "/relative", Vec::new(), ErrorCode::ConstraintViolation),
            (HttpMethod::Post, URI, vec![
                Header::new("Content-Length", "13").unwrap(),
            ], ErrorCode::FramingError),
            (HttpMethod::Post, URI, vec![
                Header::new("Content-Type", "text/plain").unwrap(),
            ], ErrorCode::TypeMismatch),
            (HttpMethod::Post, URI, vec![
                Header::new("Content-Length", "14").unwrap(),
                Header::new("Content-Length", "14").unwrap(),
            ], ErrorCode::FramingError),
        ] {
            let transport = RecordingTransport::success();
            let validator = LiteralValidator::default();
            let error = ready_result(call_json::<_, _, Message<RESPONSE_SCHEMA>>(
                &transport, &validator, method, uri, supplied,
                &Message::<REQUEST_SCHEMA>(true), HttpLimits::default(),
            )).unwrap_err();
            assert_eq!(error.code, expected);
            assert!(transport.sent.borrow().is_empty());
        }
    }

    #[test]
    fn validator_and_encoder_failures_keep_all_metadata() {
        let expected = Error::new(
            ErrorCode::PatternMismatch, ErrorSource::SchemaVm, 9, "response validation failed",
        ).with_schema(22, 17);
        let validator = LiteralValidator { failure: Some(expected.clone()), ..LiteralValidator::default() };
        let transport = RecordingTransport::success();
        assert_eq!(
            invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err(), expected,
        );
        assert_eq!(validator.calls.borrow().len(), 1);
        assert_eq!(transport.sent.borrow().len(), 1);

        let encodes = Cell::new(0);
        let expected = Error::new(
            ErrorCode::NumberOverflow, ErrorSource::Encode, 10, "request encoding failed",
        ).with_schema(11, 2);
        let value = CountedValue { encodes: &encodes, failure: Some(expected.clone()) };
        let transport = RecordingTransport::success();
        let validator = LiteralValidator::default();
        let error = ready_result(call_json::<_, _, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, HttpMethod::Post, URI, Vec::new(), &value, HttpLimits::default(),
        )).unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(encodes.get(), 1);
        assert!(validator.calls.borrow().is_empty());
        assert!(transport.sent.borrow().is_empty());
    }

    #[test]
    fn call_json_request_validation_and_encoding_limits_fail_before_send() {
        let expected = Error::new(
            ErrorCode::MissingRequiredField, ErrorSource::SchemaVm, 1, "request validation failed",
        ).with_schema(11, 4);
        let validator = LiteralValidator { failure: Some(expected.clone()), ..LiteralValidator::default() };
        let transport = RecordingTransport::success();
        let error = ready_result(call_json::<_, _, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, HttpMethod::Post, URI, Vec::new(),
            &Message::<REQUEST_SCHEMA>(true), HttpLimits::default(),
        )).unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(validator.calls.borrow().len(), 1);
        assert_eq!(validator.calls.borrow()[0].0, REQUEST_SCHEMA);
        assert!(transport.sent.borrow().is_empty());

        let validator = LiteralValidator::default();
        let error = ready_result(call_json::<_, _, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, HttpMethod::Post, URI, Vec::new(),
            &Message::<REQUEST_SCHEMA>(true), HttpLimits { max_body_bytes: 13, ..HttpLimits::default() },
        )).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert!(validator.calls.borrow().is_empty());
        assert!(transport.sent.borrow().is_empty());
    }

    /// Returns independently valid evidence for a different schema or payload.
    /// Root validation must reject this misbinding before a typed decode.
    struct MisboundValidator {
        schema: Option<u64>,
        bytes: Option<&'static [u8]>,
    }

    impl BodyValidator for MisboundValidator {
        fn validate(&self, payload: Payload, fingerprint: u64, limits: Limits) -> Result<ValidatedSlots> {
            LiteralValidator::default().validate(
                self.bytes.map(Payload::from_slice).unwrap_or(payload),
                self.schema.unwrap_or(fingerprint), limits,
            )
        }
    }

    #[test]
    fn response_evidence_is_bound_to_exact_schema_and_unmodified_bytes() {
        for (validator, code) in [
            (MisboundValidator { schema: Some(REQUEST_SCHEMA), bytes: None }, ErrorCode::SchemaImageInvalid),
            (MisboundValidator { schema: None, bytes: Some(TRUE_JSON) }, ErrorCode::InternalError),
        ] {
            let transport = RecordingTransport::success();
            let error = invoke(&transport, &validator, request(), HttpLimits::default()).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.source, ErrorSource::Transport);
            assert_eq!(transport.sent.borrow().len(), 1);
        }
        // Equal-length, independently valid JSON still may not replace bytes.
        let mut reply = response();
        reply.body = Payload::from_slice(b" {\"value\":true}");
        reply.headers = headers(reply.body.len());
        let validator = MisboundValidator { schema: None, bytes: Some(FALSE_JSON) };
        let error = invoke(
            &RecordingTransport::new(Ok(reply)), &validator, request(), HttpLimits::default(),
        ).unwrap_err();
        assert_eq!(error.code, ErrorCode::InternalError);
    }

    #[derive(Debug)]
    struct TrueOnly;

    impl SchemaProvider for TrueOnly {
        fn schema_ir() -> SchemaIr { Message::<RESPONSE_SCHEMA>::schema_ir() }
        fn schema_fingerprint() -> u64 { RESPONSE_SCHEMA }
    }

    impl JsonDecode for TrueOnly {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            if Message::<RESPONSE_SCHEMA>::decode_slots(slots)?.0 { return Ok(Self); }
            Err(Error::new(
                ErrorCode::ConstraintViolation, ErrorSource::Decode, 9, "application requires true",
            ).with_schema(22, 6))
        }
    }

    #[test]
    fn typed_decoder_error_is_not_converted_to_success() {
        let transport = RecordingTransport::success();
        let validator = LiteralValidator::default();
        let error = ready_result(call::<_, TrueOnly>(
            &transport, &validator, request(), HttpLimits::default(),
        )).unwrap_err();
        assert_eq!(error, Error::new(
            ErrorCode::ConstraintViolation, ErrorSource::Decode, 9, "application requires true",
        ).with_schema(22, 6));
        assert_eq!(transport.sent.borrow().len(), 1);
        assert_eq!(validator.calls.borrow().len(), 1);
    }

    /// An adapter future which suspends once, then completes on its next poll.
    struct SuspendedResponse {
        request: OutgoingRequest,
        reply: Option<IncomingResponse>,
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Future for SuspendedResponse {
        type Output = Result<IncomingResponse>;

        fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            assert_eq!(this.request.body.payload().as_slice(), TRUE_JSON);
            if this.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                context.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Ready(Ok(this.reply.take().expect("completed future is not repolled")))
        }
    }

    impl Drop for SuspendedResponse {
        fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); }
    }

    struct SuspendedTransport {
        sends: Cell<usize>,
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl SuspendedTransport {
        fn new() -> Self {
            Self {
                sends: Cell::new(0), polls: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl HttpTransport for SuspendedTransport {
        type ResponseFuture<'a> = SuspendedResponse where Self: 'a;

        fn send(&self, request: OutgoingRequest) -> Self::ResponseFuture<'_> {
            self.sends.set(self.sends.get() + 1);
            SuspendedResponse {
                request, reply: Some(response()), polls: self.polls.clone(), drops: self.drops.clone(),
            }
        }
    }

    #[test]
    fn adapter_suspension_is_awaited_without_reissuing_request() {
        let transport = SuspendedTransport::new();
        let validator = LiteralValidator::default();
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        let mut future = pin!(call::<_, Message<RESPONSE_SCHEMA>>(
            &transport, &validator, request(), HttpLimits::default(),
        ));
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(transport.sends.get(), 1);
        assert!(validator.calls.borrow().is_empty());
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        let parsed = match future.as_mut().poll(&mut context) {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => panic!("adapter must complete on second poll"),
        };
        assert_eq!(parsed.values(), &Message::<RESPONSE_SCHEMA>(false));
        assert_eq!(transport.sends.get(), 1);
        assert_eq!(transport.polls.load(Ordering::SeqCst), 2);
        assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
        assert_eq!(validator.calls.borrow().len(), 1);
    }

    #[test]
    fn dropping_pending_call_drops_adapter_future_without_response_validation() {
        let transport = SuspendedTransport::new();
        let validator = LiteralValidator::default();
        let waker = Waker::from(Arc::new(WakeCounter::default()));
        let mut context = Context::from_waker(&waker);
        {
            let mut future = pin!(call::<_, Message<RESPONSE_SCHEMA>>(
                &transport, &validator, request(), HttpLimits::default(),
            ));
            assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
            assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
        }
        assert_eq!(transport.sends.get(), 1);
        assert_eq!(transport.polls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
        assert!(validator.calls.borrow().is_empty());
    }
}
