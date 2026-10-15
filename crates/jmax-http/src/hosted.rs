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
    check_proof(body.slots(), body.proof().schema_fingerprint(), body.payload().as_slice())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{rc::Rc, sync::Arc, vec};
    use core::cell::{Cell, RefCell};
    use jmax_abi::{
        FieldId, SchemaFieldIr, SchemaId, SchemaIr, SlotValue, Span32, SpanSource, StrRef,
        syntax::ScalarKind,
    };

    const REQUEST_SCHEMA: u64 = 0x5734_0030;
    const REPLY_SCHEMA: u64 = REQUEST_SCHEMA + 1;
    const ERROR_SCHEMA: u64 = REQUEST_SCHEMA + 2;
    const OTHER_SCHEMA: u64 = REQUEST_SCHEMA + 3;
    const RAW: &[u8] = br#"{"text":"a"}"#;
    const PADDED: &[u8] = b" \n{\"text\":\"a\"}\t ";
    const ESCAPED: &[u8] = br#"{"text":"a\n"}"#;
    const UNICODE: &[u8] = "{\"text\":\"é😀\"}".as_bytes();
    const ACK: &[u8] = br#"{"ok":true}"#;
    const ERROR_JSON: &[u8] = br#"{"error":"denied"}"#;
    const URI: &str = "/hooks/v1?case=Keep%2f&empty=";

    type Events = Rc<RefCell<Vec<&'static str>>>;

    fn header(name: &str, value: &str) -> Header {
        Header::new(name, value).expect("valid fixture header")
    }

    fn headers(bytes: &[u8]) -> Vec<Header> {
        vec![
            header("Content-Type", JSON_MEDIA_TYPE),
            header("Content-Length", &alloc::format!("{}", bytes.len())),
        ]
    }

    fn input(bytes: &[u8]) -> IncomingRequest {
        IncomingRequest {
            method: HttpMethod::Post,
            uri: String::from(URI),
            headers: headers(bytes),
            body: Payload::from_slice(bytes),
            completion: BodyCompletion::Complete,
        }
    }

    fn error<T>(result: Result<T>) -> Error {
        match result {
            Err(failure) => failure,
            Ok(_) => panic!("expected failure"),
        }
    }

    fn assert_local<T>(result: Result<T>, code: ErrorCode) {
        let failure = error(result);
        assert_eq!(failure.code, code);
        assert_eq!(failure.source, ErrorSource::Transport);
        assert_eq!(failure.byte_offset, 0);
        assert_eq!((failure.schema_id, failure.field_id), (0, 0));
    }

    fn syntax_error() -> Error {
        Error::new(ErrorCode::InvalidSyntax, ErrorSource::Syntax, 7,
            "expected a complete fixture object").with_schema(30, 1)
    }

    fn string_slot(offset: u32, len: u32, scalar_len: u32, source: SpanSource) -> SlotValue {
        SlotValue::Str(StrRef { span: Span32::new(offset, len), source, scalar_len })
    }

    /// Exact matching validates complete literals for three distinct shapes.
    /// OTHER_SCHEMA deliberately registers the request shape under another ID.
    fn literal_slots(payload: Payload, fingerprint: u64) -> Result<ValidatedSlots> {
        let (slot, decoded) = match (fingerprint, payload.as_slice()) {
            (REQUEST_SCHEMA | OTHER_SCHEMA, RAW) =>
                (string_slot(9, 1, 1, SpanSource::RawPayload), Vec::new()),
            (REQUEST_SCHEMA | OTHER_SCHEMA, PADDED) =>
                (string_slot(11, 1, 1, SpanSource::RawPayload), Vec::new()),
            (REQUEST_SCHEMA | OTHER_SCHEMA, ESCAPED) =>
                (string_slot(0, 2, 2, SpanSource::DecodedArena), b"a\n".to_vec()),
            (REQUEST_SCHEMA | OTHER_SCHEMA, UNICODE) =>
                (string_slot(9, 6, 2, SpanSource::RawPayload), Vec::new()),
            (REPLY_SCHEMA, ACK) => (SlotValue::Bool(true), Vec::new()),
            (ERROR_SCHEMA, ERROR_JSON) =>
                (string_slot(10, 6, 6, SpanSource::RawPayload), Vec::new()),
            _ => return Err(syntax_error()),
        };
        let payload_len = u32::try_from(payload.len()).expect("fixture payload fits ABI");
        let decoded_len = u32::try_from(decoded.len()).expect("fixture arena fits ABI");
        // SAFETY: Exact literal matching validates the entire object, its only
        // required field and the schema above. Raw spans cover valid UTF-8 a,
        // é😀, or denied, with exact scalar counts. The escape fixture's arena
        // contains a and newline with a matching decoded span. All references
        // are in bounds, and checked lengths describe these immutable owners.
        let proof = unsafe {
            ValidationProof::issue_unchecked(fingerprint, payload_len, decoded_len)
        };
        Ok(ValidatedSlots {
            payload, decoded: decoded.into_boxed_slice(), slots: vec![slot], proof,
        })
    }

    #[derive(Clone, Copy, Default)]
    enum Fault {
        #[default]
        None,
        Fingerprint,
        ChangedBytes,
        PayloadLength,
        DecodedLength,
    }

    #[derive(Default)]
    struct Validator {
        calls: RefCell<Vec<(Payload, u64, Limits)>>,
        events: Events,
        failure: Option<Error>,
        fault: Fault,
    }

    impl BodyValidator for Validator {
        fn validate(&self, payload: Payload, fingerprint: u64, limits: Limits) -> Result<ValidatedSlots> {
            self.events.borrow_mut().push("validate");
            self.calls.borrow_mut().push((payload.clone(), fingerprint, limits));
            if let Some(failure) = &self.failure { return Err(failure.clone()); }
            let issued_schema = if matches!(self.fault, Fault::Fingerprint) {
                OTHER_SCHEMA
            } else { fingerprint };
            let mut slots = literal_slots(payload, issued_schema)?;
            // Faults corrupt safely mutable storage only after genuine proof
            // issuance. The HTTP boundary must reject it before consumption.
            match self.fault {
                Fault::ChangedBytes => slots.payload = Payload::from_slice(br#"{"text":"b"}"#),
                Fault::PayloadLength => slots.payload = Payload::from_slice(b""),
                Fault::DecodedLength => slots.decoded = vec![b'x'].into_boxed_slice(),
                Fault::None | Fault::Fingerprint => {}
            }
            // Leave arena-cap enforcement to the production boundary so the
            // tests exercise its defensive checks as well as error forwarding.
            Ok(slots)
        }
    }

    struct Verifier {
        expected_headers: Vec<Header>,
        expected_body: Payload,
        calls: Cell<usize>,
        events: Events,
        failure: Option<Error>,
    }

    impl Verifier {
        fn new(request: &IncomingRequest, events: Events) -> Self {
            Self {
                expected_headers: request.headers.clone(), expected_body: request.body.clone(),
                calls: Cell::new(0), events, failure: None,
            }
        }
    }

    impl WebhookVerifier for Verifier {
        fn verify(&self, headers: &[Header], body: &[u8]) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            self.events.borrow_mut().push("verify");
            assert_eq!(headers, self.expected_headers);
            assert_eq!(body, self.expected_body.as_slice());
            assert_eq!(body.as_ptr(), self.expected_body.as_slice().as_ptr());
            match &self.failure {
                Some(failure) => Err(failure.clone()),
                None => Ok(()),
            }
        }
    }

    struct Handler {
        calls: Cell<usize>,
        events: Events,
        received: RefCell<Vec<HostedRequest>>,
        output: RefCell<Option<Result<OutgoingResponse>>>,
    }

    impl Handler {
        fn new(output: Result<OutgoingResponse>, events: Events) -> Self {
            Self {
                calls: Cell::new(0), events, received: RefCell::new(Vec::new()),
                output: RefCell::new(Some(output)),
            }
        }
    }

    impl HostedHandler for Handler {
        fn handle(&self, request: HostedRequest) -> Result<OutgoingResponse> {
            self.calls.set(self.calls.get() + 1);
            self.events.borrow_mut().push("handle");
            self.received.borrow_mut().push(request);
            self.output.borrow_mut().take().expect("handler called exactly once")
        }
    }

    fn response(status: u16, bytes: &[u8], fingerprint: u64) -> OutgoingResponse {
        let body = validate_body(Payload::from_slice(bytes), &Validator::default(),
            fingerprint, HttpLimits::default()).unwrap();
        OutgoingResponse { status, headers: headers(bytes), body }
    }

    fn success() -> OutgoingResponse { response(200, ACK, REPLY_SCHEMA) }

    fn run_response(output: OutgoingResponse, limits: HttpLimits) -> Result<OutgoingResponse> {
        let validator = Validator::default();
        let handler = Handler::new(Ok(output), validator.events.clone());
        let result = dispatch(input(RAW), None, &validator, REQUEST_SCHEMA, &handler, limits);
        assert_eq!(handler.calls.get(), 1);
        assert_eq!(validator.calls.borrow().len(), 1);
        assert_eq!(&*validator.events.borrow(), &["validate", "handle"]);
        result
    }

    fn reject_request(request: IncomingRequest, limits: HttpLimits, code: ErrorCode) {
        // Exercise both exported entry points, and prove that metadata failures
        // prevent authentication, JSON validation, and handler side effects.
        for do_dispatch in [false, true] {
            let validator = Validator::default();
            let verifier = Verifier::new(&request, validator.events.clone());
            let handler = Handler::new(Ok(success()), validator.events.clone());
            if do_dispatch {
                assert_local(dispatch(request.clone(), Some(&verifier), &validator,
                    REQUEST_SCHEMA, &handler, limits), code);
            } else {
                assert_local(accept_request(request.clone(), Some(&verifier), &validator,
                    REQUEST_SCHEMA, limits), code);
            }
            assert_eq!(verifier.calls.get(), 0);
            assert!(validator.calls.borrow().is_empty());
            assert_eq!(handler.calls.get(), 0);
            assert!(validator.events.borrow().is_empty());
        }
    }

    #[test]
    fn signed_vectors_preserve_original_metadata_bytes_and_owners() {
        for bytes in [RAW, PADDED, ESCAPED, UNICODE] {
            for shared in [false, true] {
                let mut request = input(bytes);
                if !shared { request.body = Payload::from_vec(bytes.to_vec()); }
                request.headers = vec![
                    header("X-Signature", " v1=ABC\t"),
                    header("Content-Length", &alloc::format!(" \t000{}\t ", bytes.len())),
                    header("Content-Type", " APPLICATION/Problem+JSON ; charset = \"UTF-8\"\t"),
                    header("X-Trace", "first"), header("X-Trace", "é"),
                ];
                let original = request.clone();
                let uri_pointer = request.uri.as_ptr();
                let headers_pointer = request.headers.as_ptr();
                let validator = Validator::default();
                let verifier = Verifier::new(&request, validator.events.clone());
                let hosted = accept_request(request, Some(&verifier), &validator,
                    REQUEST_SCHEMA, HttpLimits::default()).unwrap();
                assert_eq!(hosted.method, original.method);
                assert_eq!(hosted.uri, original.uri);
                assert_eq!(hosted.uri.as_ptr(), uri_pointer);
                assert_eq!(hosted.headers, original.headers);
                assert_eq!(hosted.headers.as_ptr(), headers_pointer);
                assert_eq!(hosted.body.payload().as_slice(), bytes);
                assert_eq!(hosted.body.payload().as_slice().as_ptr(), original.body.as_slice().as_ptr());
                assert_eq!(hosted.body.proof().schema_fingerprint(), REQUEST_SCHEMA);
                assert_eq!(hosted.body.proof().payload_len(), u32::try_from(bytes.len()).unwrap());
                let [SlotValue::Str(reference)] = hosted.body.slots().slots.as_slice() else {
                    panic!("one string slot required");
                };
                let storage = match reference.source {
                    SpanSource::RawPayload => hosted.body.payload().as_slice(),
                    SpanSource::DecodedArena => &hosted.body.slots().decoded,
                };
                let text = core::str::from_utf8(reference.span.slice(storage).unwrap()).unwrap();
                assert_eq!(text, if bytes == ESCAPED { "a\n" }
                    else if bytes == UNICODE { "é😀" } else { "a" });
                assert_eq!(text.chars().count(), usize::try_from(reference.scalar_len).unwrap());
                assert_eq!(verifier.calls.get(), 1);
                assert_eq!(validator.calls.borrow().len(), 1);
                assert_eq!(&*validator.events.borrow(), &["verify", "validate"]);
                drop(original);
                drop(verifier);
                drop(validator);
                assert_eq!(hosted.body.payload().as_slice(), bytes);
            }
        }
    }

    #[test]
    fn supported_methods_and_targets_are_retained_with_or_without_verification() {
        for method in [HttpMethod::Get, HttpMethod::Post, HttpMethod::Put, HttpMethod::Patch,
            HttpMethod::Delete, HttpMethod::Options]
        {
            for uri in ["/", URI, "https://EXAMPLE.test.:443/a%2fb?q=Keep",
                "http://[2001:db8::1]:65535/hook"]
            {
                for verify in [false, true] {
                    let mut request = input(RAW);
                    request.method = method;
                    request.uri = String::from(uri);
                    // Completion is explicit, so incoming length is optional.
                    request.headers.retain(|field| field.name() != "content-length");
                    let validator = Validator::default();
                    let verifier = Verifier::new(&request, validator.events.clone());
                    let configured: Option<&dyn WebhookVerifier> =
                        if verify { Some(&verifier) } else { None };
                    let hosted = accept_request(request, configured, &validator,
                        REQUEST_SCHEMA, HttpLimits::default()).unwrap();
                    assert_eq!(hosted.method, method);
                    assert_eq!(hosted.uri, uri);
                    assert_eq!(hosted.headers.len(), 1);
                    assert_eq!(verifier.calls.get(), usize::from(verify));
                    assert_eq!(validator.calls.borrow().len(), 1);
                }
            }
        }
    }

    #[test]
    fn invalid_configuration_is_rejected_before_all_callbacks() {
        for case in 0..6 {
            let mut limits = HttpLimits::default();
            match case {
                0 => limits.max_body_bytes = 0,
                1 => limits.max_header_bytes = 0,
                2 => limits.max_header_bytes = 1,
                3 => limits.max_headers = 0,
                4 => limits.syntax.max_payload_bytes = 0,
                _ => limits.syntax.max_depth = 0,
            }
            let mut request = input(RAW);
            request.completion = BodyCompletion::Incomplete;
            request.method = HttpMethod::Head;
            request.uri.clear();
            request.headers.clear();
            reject_request(request, limits, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn completion_precedes_method_uri_and_headers() {
        let mut request = input(RAW);
        request.completion = BodyCompletion::Incomplete;
        request.method = HttpMethod::Head;
        request.uri.clear();
        request.headers.clear();
        reject_request(request, HttpLimits::default(), ErrorCode::TransportIncomplete);
        let mut request = input(RAW);
        request.method = HttpMethod::Head;
        reject_request(request, HttpLimits::default(), ErrorCode::UnsupportedSchemaFeature);
    }

    #[test]
    fn malformed_targets_are_rejected_before_verification() {
        for uri in ["", "relative", "*", "//example.test/hook", "HTTPS://example.test/",
            "https://user@example.test/", "https://example.test/#fragment",
            "/bad path", "/bad\\path", "/%", "/%GG", "/é",
            "https://[:::1]/", "https://host:0/", "https://host:65536/"]
        {
            let mut request = input(RAW);
            request.uri = String::from(uri);
            reject_request(request, HttpLimits::default(), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn malformed_request_headers_never_reach_verification() {
        for (name, value, code) in [
            ("content-length", "", ErrorCode::FramingError),
            ("content-length", "+12", ErrorCode::FramingError),
            ("content-length", "12,12", ErrorCode::FramingError),
            ("content-length", "18446744073709551616", ErrorCode::FramingError),
            ("content-length", "11", ErrorCode::FramingError),
            ("content-length", "13", ErrorCode::TransportIncomplete),
            ("content-type", "text/plain", ErrorCode::TypeMismatch),
            ("content-type", "application/json; charset=latin1", ErrorCode::TypeMismatch),
            ("content-type", "application/json; charset=utf-8; charset=utf-8", ErrorCode::TypeMismatch),
            ("content-encoding", "gzip", ErrorCode::FramingError),
            ("transfer-encoding", "chunked", ErrorCode::FramingError),
        ] {
            let mut request = input(RAW);
            request.headers.retain(|field| field.name() != name);
            request.headers.push(header(name, value));
            reject_request(request, HttpLimits::default(), code);
        }
        for name in ["content-length", "content-type", "content-encoding"] {
            let mut request = input(RAW);
            let value = match name {
                "content-length" => "12",
                "content-type" => JSON_MEDIA_TYPE,
                _ => "identity",
            };
            request.headers.retain(|field| field.name() != name);
            request.headers.push(header(name, value));
            request.headers.push(header(name, value));
            reject_request(request, HttpLimits::default(), ErrorCode::FramingError);
        }
        let mut request = input(RAW);
        request.headers.retain(|field| field.name() != "content-type");
        reject_request(request, HttpLimits::default(), ErrorCode::TypeMismatch);
    }

    #[test]
    fn request_payload_and_header_limits_are_inclusive() {
        let mut request = input(UNICODE);
        request.headers.push(header("x-label", "é"));
        let header_bytes = request.headers.iter().fold(2usize, |sum, field| {
            sum + field.name().len() + field.value().len() + 4
        });
        assert_eq!(UNICODE.len(), 17);
        assert_eq!(core::str::from_utf8(UNICODE).unwrap().chars().count(), 13);
        let limits = HttpLimits {
            max_body_bytes: 17,
            max_header_bytes: u32::try_from(header_bytes).unwrap(),
            max_headers: u16::try_from(request.headers.len()).unwrap(),
            ..HttpLimits::default()
        };
        let validator = Validator::default();
        let verifier = Verifier::new(&request, validator.events.clone());
        accept_request(request.clone(), Some(&verifier), &validator, REQUEST_SCHEMA, limits).unwrap();
        assert_eq!(verifier.calls.get(), 1);
        for case in 0..4 {
            let mut below = limits;
            let code = match case {
                0 => { below.max_body_bytes -= 1; ErrorCode::PayloadTooLarge }
                1 => { below.syntax.max_payload_bytes = 16; ErrorCode::PayloadTooLarge }
                2 => { below.max_header_bytes -= 1; ErrorCode::PayloadTooLarge }
                _ => { below.max_headers -= 1; ErrorCode::BudgetExhausted }
            };
            reject_request(request.clone(), below, code);
        }
    }

    #[test]
    fn verifier_errors_preserve_all_metadata_and_prevent_later_work() {
        for code in [ErrorCode::AuthFailed, ErrorCode::SignatureInvalid, ErrorCode::Cancelled] {
            for do_dispatch in [false, true] {
                // Deliberately invalid JSON would fail if validation ran first.
                let request = input(b"{broken");
                let validator = Validator::default();
                let failure = Error::new(code, ErrorSource::Auth, 4,
                    "host verification rejected").with_schema(91, 8);
                let mut verifier = Verifier::new(&request, validator.events.clone());
                verifier.failure = Some(failure.clone());
                let handler = Handler::new(Ok(success()), validator.events.clone());
                let actual = if do_dispatch {
                    error(dispatch(request, Some(&verifier), &validator, REQUEST_SCHEMA,
                        &handler, HttpLimits::default()))
                } else {
                    error(accept_request(request, Some(&verifier), &validator,
                        REQUEST_SCHEMA, HttpLimits::default()))
                };
                assert_eq!(actual, failure);
                assert_eq!(verifier.calls.get(), 1);
                assert!(validator.calls.borrow().is_empty());
                assert_eq!(handler.calls.get(), 0);
                assert_eq!(&*validator.events.borrow(), &["verify"]);
            }
        }
    }

    #[test]
    fn verification_success_does_not_bypass_json_validation() {
        for bytes in [&b""[..], &b"null"[..], &b"{}"[..], &b"NaN"[..],
            &b"{\"text\":1}"[..], &b"{\"text\":\"a\"}true"[..],
            &b"{\"text\":\"a\"}\xff"[..]]
        {
            for verify in [false, true] {
                let request = input(bytes);
                let validator = Validator::default();
                let verifier = Verifier::new(&request, validator.events.clone());
                let configured: Option<&dyn WebhookVerifier> =
                    if verify { Some(&verifier) } else { None };
                let handler = Handler::new(Ok(success()), validator.events.clone());
                assert_eq!(error(dispatch(request, configured, &validator, REQUEST_SCHEMA,
                    &handler, HttpLimits::default())), syntax_error());
                assert_eq!(verifier.calls.get(), usize::from(verify));
                assert_eq!(validator.calls.borrow().len(), 1);
                assert_eq!(handler.calls.get(), 0);
            }
        }
    }

    #[test]
    fn validator_errors_and_binding_failures_prevent_handler_execution() {
        let failure = Error::new(ErrorCode::PatternMismatch, ErrorSource::SchemaVm, 9,
            "text rejected").with_schema(30, 7);
        let validator = Validator { failure: Some(failure.clone()), ..Validator::default() };
        let request = input(RAW);
        let verifier = Verifier::new(&request, validator.events.clone());
        let handler = Handler::new(Ok(success()), validator.events.clone());
        assert_eq!(error(dispatch(request, Some(&verifier), &validator, REQUEST_SCHEMA,
            &handler, HttpLimits::default())), failure);
        assert_eq!(handler.calls.get(), 0);
        assert_eq!(&*validator.events.borrow(), &["verify", "validate"]);

        for fault in [Fault::Fingerprint, Fault::ChangedBytes, Fault::PayloadLength, Fault::DecodedLength] {
            for do_dispatch in [false, true] {
                let validator = Validator { fault, ..Validator::default() };
                let request = input(RAW);
                let verifier = Verifier::new(&request, validator.events.clone());
                let handler = Handler::new(Ok(success()), validator.events.clone());
                let code = if matches!(fault, Fault::Fingerprint) { ErrorCode::SchemaImageInvalid }
                    else { ErrorCode::InternalError };
                if do_dispatch {
                    assert_local(dispatch(request, Some(&verifier), &validator, REQUEST_SCHEMA,
                        &handler, HttpLimits::default()), code);
                } else {
                    assert_local(accept_request(request, Some(&verifier), &validator,
                        REQUEST_SCHEMA, HttpLimits::default()), code);
                }
                assert_eq!(verifier.calls.get(), 1);
                assert_eq!(validator.calls.borrow().len(), 1);
                assert_eq!(handler.calls.get(), 0);
            }
        }
    }

    #[test]
    fn effective_syntax_preserves_all_other_limits() {
        let syntax = Limits {
            max_payload_bytes: 64, max_decoded_bytes: 3, max_depth: 4,
            max_fields_per_object: 2, max_string_bytes: 8, max_number_digits: 9,
            max_array_items: 5, reject_duplicate_keys: false, reject_unknown_keys: false,
        };
        for (transport_cap, syntax_cap, expected) in [(32, 64, 32), (64, 32, 32)] {
            let limits = HttpLimits { max_body_bytes: transport_cap,
                syntax: Limits { max_payload_bytes: syntax_cap, ..syntax }, ..HttpLimits::default() };
            let validator = Validator::default();
            accept_request(input(RAW), None, &validator, REQUEST_SCHEMA, limits).unwrap();
            let calls = validator.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].1, REQUEST_SCHEMA);
            assert_eq!(calls[0].2, Limits { max_payload_bytes: expected, ..syntax });
        }
    }

    #[test]
    fn request_decoded_arena_limit_is_exact_after_verification() {
        for cap in [0, 1, 2] {
            let mut limits = HttpLimits::default();
            limits.syntax.max_decoded_bytes = cap;
            accept_request(input(RAW), None, &Validator::default(), REQUEST_SCHEMA, limits).unwrap();
            for do_dispatch in [false, true] {
                let request = input(ESCAPED);
                let validator = Validator::default();
                let verifier = Verifier::new(&request, validator.events.clone());
                let handler = Handler::new(Ok(success()), validator.events.clone());
                let result = if do_dispatch {
                    dispatch(request, Some(&verifier), &validator, REQUEST_SCHEMA, &handler, limits)
                        .map(|response| response.body)
                } else {
                    accept_request(request, Some(&verifier), &validator, REQUEST_SCHEMA, limits)
                        .map(|request| request.body)
                };
                if cap == 2 {
                    let body = result.unwrap();
                    if !do_dispatch { assert_eq!(&*body.slots().decoded, b"a\n"); }
                    assert_eq!(handler.calls.get(), usize::from(do_dispatch));
                } else {
                    assert_local(result, ErrorCode::DecodedArenaOverflow);
                    assert_eq!(handler.calls.get(), 0);
                }
                assert_eq!(verifier.calls.get(), 1);
                assert_eq!(validator.calls.borrow().len(), 1);
            }
        }
    }

    #[test]
    fn dispatch_orders_callbacks_and_transfers_both_message_owners() {
        for verify in [false, true] {
            let request = input(PADDED);
            let original_request = request.clone();
            let validator = Validator::default();
            let verifier = Verifier::new(&request, validator.events.clone());
            let output = success();
            let original_response = output.body.payload().clone();
            let output_headers = output.headers.clone();
            let header_pointer = output.headers.as_ptr();
            let handler = Handler::new(Ok(output), validator.events.clone());
            let configured: Option<&dyn WebhookVerifier> =
                if verify { Some(&verifier) } else { None };
            let output = dispatch(request, configured, &validator, REQUEST_SCHEMA,
                &handler, HttpLimits::default()).unwrap();
            assert_eq!(output.status, 200);
            assert_eq!(output.headers, output_headers);
            assert_eq!(output.headers.as_ptr(), header_pointer);
            assert_eq!(output.body.payload().as_slice(), ACK);
            assert_eq!(output.body.payload().as_slice().as_ptr(), original_response.as_slice().as_ptr());
            assert_eq!(output.body.proof().schema_fingerprint(), REPLY_SCHEMA);
            assert_eq!(handler.calls.get(), 1);
            assert!(handler.output.borrow().is_none());
            let received = handler.received.borrow();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].method, original_request.method);
            assert_eq!(received[0].uri, original_request.uri);
            assert_eq!(received[0].headers, original_request.headers);
            assert_eq!(received[0].body.payload().as_slice(), PADDED);
            assert_eq!(received[0].body.payload().as_slice().as_ptr(),
                original_request.body.as_slice().as_ptr());
            assert_eq!(verifier.calls.get(), usize::from(verify));
            assert_eq!(validator.calls.borrow().len(), 1);
            if verify {
                assert_eq!(&*validator.events.borrow(), &["verify", "validate", "handle"]);
            } else {
                assert_eq!(&*validator.events.borrow(), &["validate", "handle"]);
            }
        }
    }

    #[test]
    fn handler_errors_propagate_without_retry_or_response_conversion() {
        let failure = Error::new(ErrorCode::ConstraintViolation, ErrorSource::Decode, 9,
            "application rejected request").with_schema(72, 6);
        let validator = Validator::default();
        let request = input(RAW);
        let verifier = Verifier::new(&request, validator.events.clone());
        let handler = Handler::new(Err(failure.clone()), validator.events.clone());
        assert_eq!(error(dispatch(request, Some(&verifier), &validator, REQUEST_SCHEMA,
            &handler, HttpLimits::default())), failure);
        assert_eq!(handler.calls.get(), 1);
        assert_eq!(handler.received.borrow().len(), 1);
        assert_eq!(validator.calls.borrow().len(), 1);
        assert_eq!(&*validator.events.borrow(), &["verify", "validate", "handle"]);
    }

    #[test]
    fn every_status_boundary_is_checked_after_one_handler_call() {
        for status in (0u16..=600).chain(core::iter::once(u16::MAX)) {
            let result = run_response(response(status, ACK, REPLY_SCHEMA), HttpLimits::default());
            if (200..=599).contains(&status) && !matches!(status, 204 | 205 | 304) {
                assert_eq!(result.unwrap().status, status);
            } else {
                let code = if matches!(status, 204 | 205 | 304) { ErrorCode::TypeMismatch }
                    else { ErrorCode::ConstraintViolation };
                assert_local(result, code);
            }
        }
    }

    #[test]
    fn typed_error_responses_keep_the_handler_schema_and_error_status() {
        for status in [400, 401, 403, 404, 409, 422, 429, 500, 503, 599] {
            let mut output = response(status, ERROR_JSON, ERROR_SCHEMA);
            output.headers[0] = header("Content-Type", "application/problem+json; charset=utf-8");
            let original = output.body.payload().clone();
            let output = run_response(output, HttpLimits::default()).unwrap();
            assert_eq!(output.status, status);
            assert_eq!(output.body.payload().as_slice(), ERROR_JSON);
            assert_eq!(output.body.payload().as_slice().as_ptr(), original.as_slice().as_ptr());
            assert_eq!(output.body.proof().schema_fingerprint(), ERROR_SCHEMA);
            assert_ne!(output.body.proof().schema_fingerprint(), REQUEST_SCHEMA);
            assert_eq!(output.body.slots().slots,
                vec![string_slot(10, 6, 6, SpanSource::RawPayload)]);
        }
    }

    #[test]
    fn outgoing_response_length_must_be_present_unique_and_canonical() {
        let mut missing = success();
        missing.headers.retain(|field| field.name() != "content-length");
        assert_local(run_response(missing, HttpLimits::default()), ErrorCode::FramingError);
        for (value, code) in [
            ("", ErrorCode::FramingError), (" ", ErrorCode::FramingError),
            ("011", ErrorCode::FramingError), ("00011", ErrorCode::FramingError),
            (" 11", ErrorCode::FramingError), ("11 ", ErrorCode::FramingError),
            ("\t11\t", ErrorCode::FramingError), ("+11", ErrorCode::FramingError),
            ("-11", ErrorCode::FramingError), ("11,11", ErrorCode::FramingError),
            ("1 1", ErrorCode::FramingError), ("11.0", ErrorCode::FramingError),
            ("１１", ErrorCode::FramingError), ("18446744073709551616", ErrorCode::FramingError),
            ("0", ErrorCode::FramingError), ("10", ErrorCode::FramingError),
            ("12", ErrorCode::TransportIncomplete),
        ] {
            let mut output = success();
            output.headers[1] = header("Content-Length", value);
            assert_local(run_response(output, HttpLimits::default()), code);
        }
        let mut duplicate = success();
        duplicate.headers.push(header("Content-Length", "11"));
        assert_local(run_response(duplicate, HttpLimits::default()), ErrorCode::FramingError);
    }

    #[test]
    fn response_json_media_types_are_checked_without_rewriting() {
        for media in ["application/json", "APPLICATION/JSON", "application/problem+json",
            "application/vnd.example.v1+json", "application/json; charset=utf-8",
            "\tAPPLICATION/Problem+JSON ; CHARSET = \"UTF-8\" "]
        {
            let mut output = success();
            output.headers[0] = header("Content-Type", media);
            let output = run_response(output, HttpLimits::default()).unwrap();
            assert_eq!(output.headers[0].value(), media);
        }
        for media in ["", "text/json", "application/+json", "application/jsonp",
            "application/json-seq", "application/json,application/json", "application/a/b+json",
            "application/json;", "application/json; charset=latin1",
            "application/json; charset=\"utf-8", "application/json; charset=\"utf-8\"suffix",
            "application/json; charset=utf-8; charset=utf-8", "application/json; unknown=utf-8"]
        {
            let mut output = success();
            output.headers[0] = header("Content-Type", media);
            assert_local(run_response(output, HttpLimits::default()), ErrorCode::TypeMismatch);
        }
        let mut missing = success();
        missing.headers.remove(0);
        assert_local(run_response(missing, HttpLimits::default()), ErrorCode::TypeMismatch);
        let mut duplicate = success();
        duplicate.headers.push(header("Content-Type", JSON_MEDIA_TYPE));
        assert_local(run_response(duplicate, HttpLimits::default()), ErrorCode::FramingError);
    }

    #[test]
    fn handler_output_must_have_decoded_transport_metadata() {
        for (name, value) in [
            ("Transfer-Encoding", ""), ("Transfer-Encoding", "identity"),
            ("Transfer-Encoding", "chunked"), ("Content-Encoding", ""),
            ("Content-Encoding", "gzip"), ("Content-Encoding", "br"),
            ("Content-Encoding", "identity, identity"), ("Content-Encoding", "identity; q=1"),
        ] {
            let mut output = success();
            output.headers.push(header(name, value));
            assert_local(run_response(output, HttpLimits::default()), ErrorCode::FramingError);
        }
        let mut duplicate = success();
        duplicate.headers.push(header("Content-Encoding", "identity"));
        duplicate.headers.push(header("Content-Encoding", "identity"));
        assert_local(run_response(duplicate, HttpLimits::default()), ErrorCode::FramingError);
        let mut output = success();
        output.headers.insert(0, header("x-trace", "first"));
        output.headers.push(header("x-trace", "second"));
        output.headers.push(header("Content-Encoding", "\tIDENTITY "));
        let original = output.headers.clone();
        assert_eq!(run_response(output, HttpLimits::default()).unwrap().headers, original);
    }

    #[test]
    fn response_limits_count_exact_body_and_final_header_bytes() {
        for syntax_cap in [false, true] {
            let mut limits = HttpLimits { max_body_bytes: 17, ..HttpLimits::default() };
            limits.syntax.max_payload_bytes = 17;
            run_response(response(200, UNICODE, REQUEST_SCHEMA), limits).unwrap();
            if syntax_cap { limits.syntax.max_payload_bytes = 16; }
            else { limits.max_body_bytes = 16; }
            assert_local(run_response(response(200, UNICODE, REQUEST_SCHEMA), limits),
                ErrorCode::PayloadTooLarge);
        }
        let make_output = || {
            let mut output = success();
            output.headers.push(header("x-label", "é"));
            output
        };
        let output = make_output();
        let bytes = output.headers.iter().fold(2usize, |sum, field| {
            sum + field.name().len() + field.value().len() + 4
        });
        let limits = HttpLimits {
            max_headers: u16::try_from(output.headers.len()).unwrap(),
            max_header_bytes: u32::try_from(bytes).unwrap(), ..HttpLimits::default()
        };
        run_response(output, limits).unwrap();
        assert_local(run_response(make_output(), HttpLimits { max_headers: 2, ..limits }),
            ErrorCode::BudgetExhausted);
        assert_local(run_response(make_output(), HttpLimits {
            max_header_bytes: limits.max_header_bytes - 1, ..limits
        }), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn handler_response_proof_lengths_must_match_actual_owners() {
        let mut changed_payload = response(200, RAW, REQUEST_SCHEMA);
        // Keep HTTP framing consistent with the changed owner so this reaches
        // the separate evidence check rather than failing metadata preflight.
        changed_payload.body.slots.payload = Payload::from_slice(ACK);
        changed_payload.headers = headers(ACK);
        assert_local(run_response(changed_payload, HttpLimits::default()), ErrorCode::InternalError);
        let mut changed_arena = success();
        changed_arena.body.slots.decoded = vec![b'x'].into_boxed_slice();
        assert_local(run_response(changed_arena, HttpLimits::default()), ErrorCode::InternalError);
        let mut truncated_arena = response(200, ESCAPED, REQUEST_SCHEMA);
        truncated_arena.body.slots.decoded = Vec::new().into_boxed_slice();
        assert_local(run_response(truncated_arena, HttpLimits::default()), ErrorCode::InternalError);
    }

    #[test]
    fn handler_response_decoded_cap_is_inclusive_and_independent_of_request() {
        for cap in [0, 1, 2] {
            let mut limits = HttpLimits::default();
            limits.syntax.max_decoded_bytes = cap;
            // The request uses no decoded arena; the handler's body uses two.
            let result = run_response(response(200, ESCAPED, REQUEST_SCHEMA), limits);
            if cap == 2 {
                let output = result.unwrap();
                assert_eq!(output.body.payload().as_slice(), ESCAPED);
                assert_eq!(&*output.body.slots().decoded, b"a\n");
            } else {
                assert_local(result, ErrorCode::DecodedArenaOverflow);
            }
            run_response(success(), limits).unwrap();
        }
    }

    struct Encoded<const FINGERPRINT: u64> {
        bytes: &'static [u8],
        encodes: Cell<usize>,
    }

    impl<const FINGERPRINT: u64> SchemaProvider for Encoded<FINGERPRINT> {
        fn schema_ir() -> SchemaIr {
            let (name, kind) = match FINGERPRINT {
                REQUEST_SCHEMA | OTHER_SCHEMA => ("text", ScalarKind::String),
                REPLY_SCHEMA => ("ok", ScalarKind::True),
                ERROR_SCHEMA => ("error", ScalarKind::String),
                _ => panic!("unknown fixture schema"),
            };
            SchemaIr {
                name: String::from("HostedFixture"),
                schema_id: SchemaId(u32::try_from(FINGERPRINT).unwrap()),
                required_count: 1, reject_unknown: true,
                fields: vec![SchemaFieldIr {
                    field_id: FieldId(0), name: String::from(name), required: true,
                    scalar_kind: Some(kind), format: None, min_len: None, max_len: None,
                    min_i64: None, max_i64: None,
                }],
            }
        }

        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl<const FINGERPRINT: u64> JsonEncode for Encoded<FINGERPRINT> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.encodes.set(self.encodes.get() + 1);
            let split = self.bytes.len() / 2;
            sink.write_bytes(&self.bytes[..split])?;
            sink.write_bytes(&self.bytes[split..])
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Reply(bool);

    impl SchemaProvider for Reply {
        fn schema_ir() -> SchemaIr { Encoded::<REPLY_SCHEMA>::schema_ir() }
        fn schema_fingerprint() -> u64 { REPLY_SCHEMA }
    }

    impl JsonDecode for Reply {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            if slots.proof.schema_fingerprint() != REPLY_SCHEMA {
                return Err(Error::new(ErrorCode::SchemaImageInvalid, ErrorSource::Decode, 0,
                    "reply schema required"));
            }
            match slots.slots.as_slice() {
                [SlotValue::Bool(value)] => Ok(Self(*value)),
                _ => Err(Error::new(ErrorCode::TypeMismatch, ErrorSource::Decode, 6,
                    "reply boolean required")),
            }
        }
    }

    struct EncodingHandler<'a> {
        validator: &'a Validator,
        reply: Encoded<REPLY_SCHEMA>,
        calls: Cell<usize>,
        limits: HttpLimits,
    }

    impl HostedHandler for EncodingHandler<'_> {
        fn handle(&self, request: HostedRequest) -> Result<OutgoingResponse> {
            self.calls.set(self.calls.get() + 1);
            self.validator.events.borrow_mut().push("handle");
            assert_eq!(request.method, HttpMethod::Patch);
            assert_eq!(request.uri, "https://example.test/hooks?q=Keep%2f");
            assert_eq!(request.body.payload().as_slice(), PADDED);
            assert_eq!(request.body.proof().schema_fingerprint(), REQUEST_SCHEMA);
            outgoing::response(201, vec![header("X-Hosted", "accepted")], &self.reply,
                self.validator, self.limits)
        }
    }

    #[test]
    fn outgoing_hosted_and_incoming_modules_share_exact_validated_buffers() {
        let limits = HttpLimits::default();
        let validator = Validator::default();
        let value = Encoded::<REQUEST_SCHEMA> { bytes: PADDED, encodes: Cell::new(0) };
        let request = outgoing::request(HttpMethod::Patch, "https://example.test/hooks?q=Keep%2f",
            vec![header("X-Signature", "v1=original")], &value, &validator, limits).unwrap();
        assert_eq!(value.encodes.get(), 1);
        let request_owner = request.body.payload().clone();
        let request = IncomingRequest {
            method: request.method, uri: request.uri, headers: request.headers,
            body: request.body.into_slots().payload, completion: BodyCompletion::Complete,
        };
        let verifier = Verifier::new(&request, validator.events.clone());
        let handler = EncodingHandler {
            validator: &validator,
            reply: Encoded::<REPLY_SCHEMA> { bytes: ACK, encodes: Cell::new(0) },
            calls: Cell::new(0), limits,
        };
        let output = dispatch(request, Some(&verifier), &validator, REQUEST_SCHEMA, &handler, limits).unwrap();
        assert_eq!(output.status, 201);
        assert_eq!(output.headers, vec![header("x-hosted", "accepted"),
            header("content-type", JSON_MEDIA_TYPE), header("content-length", "11")]);
        assert_eq!(handler.calls.get(), 1);
        assert_eq!(handler.reply.encodes.get(), 1);
        assert_eq!(verifier.calls.get(), 1);
        assert_eq!(validator.calls.borrow().len(), 3);
        {
            let calls = validator.calls.borrow();
            assert_eq!(calls[0].1, REQUEST_SCHEMA);
            assert_eq!(calls[1].1, REQUEST_SCHEMA);
            assert_eq!(calls[2].1, REPLY_SCHEMA);
            assert_eq!(calls[0].0.as_slice().as_ptr(), request_owner.as_slice().as_ptr());
            assert_eq!(calls[1].0.as_slice().as_ptr(), request_owner.as_slice().as_ptr());
            assert_eq!(calls[2].0.as_slice().as_ptr(), output.body.payload().as_slice().as_ptr());
        }
        assert_eq!(&*validator.events.borrow(),
            &["validate", "verify", "validate", "handle", "validate"]);
        let response_owner = output.body.payload().clone();
        let parsed = incoming::decode_response::<Reply>(IncomingResponse {
            status: output.status, headers: output.headers,
            body: output.body.into_slots().payload, completion: BodyCompletion::Complete,
        }, &validator, limits).unwrap();
        assert_eq!(parsed.values(), &Reply(true));
        assert_eq!(parsed.raw_bytes(), ACK);
        assert_eq!(parsed.raw_bytes().as_ptr(), response_owner.as_slice().as_ptr());
        assert_eq!(parsed.proof().schema_fingerprint(), REPLY_SCHEMA);
        assert_eq!(validator.calls.borrow().len(), 4);
        assert_eq!(value.encodes.get(), 1);
        assert_eq!(handler.reply.encodes.get(), 1);
    }

    #[test]
    fn rejected_request_and_response_owners_are_released() {
        for request_failure in [false, true] {
            let owner: Arc<[u8]> = Arc::from(RAW);
            let weak_request = Arc::downgrade(&owner);
            let mut request = input(RAW);
            request.body = Payload::Shared(owner);
            let owner: Arc<[u8]> = Arc::from(ACK);
            let weak_response = Arc::downgrade(&owner);
            let body = validate_body(Payload::Shared(owner), &Validator::default(),
                REPLY_SCHEMA, HttpLimits::default()).unwrap();
            let mut output = OutgoingResponse { status: 200, headers: headers(ACK), body };
            let code = if request_failure {
                request.completion = BodyCompletion::Incomplete;
                ErrorCode::TransportIncomplete
            } else {
                output.status = 304;
                ErrorCode::TypeMismatch
            };
            let validator = Validator::default();
            let handler = Handler::new(Ok(output), validator.events.clone());
            assert_local(dispatch(request, None, &validator, REQUEST_SCHEMA, &handler,
                HttpLimits::default()), code);
            assert_eq!(handler.calls.get(), usize::from(!request_failure));
            drop(handler);
            drop(validator);
            assert!(weak_request.upgrade().is_none());
            assert!(weak_response.upgrade().is_none());
        }
    }
}
