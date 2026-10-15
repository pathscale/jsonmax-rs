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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::cell::{Cell, RefCell};
    use jmax_abi::{
        FieldId, SchemaFieldIr, SchemaId, SchemaIr, SlotValue, Span32, SpanSource, StrRef,
        syntax::ScalarKind,
    };

    const FINGERPRINT: u64 = 0x5733_0030;
    const OTHER_FINGERPRINT: u64 = FINGERPRINT + 1;
    const RAW: &[u8] = br#"{"text":"a"}"#;
    const ESCAPED: &[u8] = br#"{"text":"a\n"}"#;
    const UNICODE: &[u8] = "{\"text\":\"é😀\"}".as_bytes();
    const PADDED: &[u8] = b" \n{\"text\":\"a\"}\t ";
    const URI: &str = "https://api.example.test:443/v1/text?q=%2f&case=Keep";

    struct Value<'a> {
        bytes: &'a [u8],
        encodes: Cell<usize>,
        failure: Option<Error>,
    }

    impl<'a> Value<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, encodes: Cell::new(0), failure: None }
        }
    }

    impl SchemaProvider for Value<'_> {
        fn schema_ir() -> SchemaIr {
            SchemaIr {
                name: String::from("OutgoingText"), schema_id: SchemaId(30),
                required_count: 1, reject_unknown: true,
                fields: vec![SchemaFieldIr {
                    field_id: FieldId(0), name: String::from("text"), required: true,
                    scalar_kind: Some(ScalarKind::String), format: None,
                    min_len: None, max_len: None, min_i64: None, max_i64: None,
                }],
            }
        }

        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl JsonEncode for Value<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.encodes.set(self.encodes.get() + 1);
            // Chunked emission exercises reservation and failure after a prefix.
            let middle = self.bytes.len() / 2;
            sink.write_bytes(&self.bytes[..middle])?;
            if let Some(failure) = &self.failure { return Err(failure.clone()); }
            sink.write_bytes(&self.bytes[middle..])
        }
    }

    fn parser_error() -> Error {
        Error::new(ErrorCode::InvalidSyntax, ErrorSource::Syntax, 7, "literal object required")
            .with_schema(30, 1)
    }

    /// An exact validator for a finite set of complete JSON string objects.
    /// Both registered fingerprints describe the same shape under distinct IDs.
    fn literal_slots(payload: Payload, fingerprint: u64) -> Result<ValidatedSlots> {
        if !matches!(fingerprint, FINGERPRINT | OTHER_FINGERPRINT) {
            return Err(http_error(ErrorCode::SchemaImageInvalid, "unknown fixture schema"));
        }
        let (span, source, scalar_len, decoded) = match payload.as_slice() {
            RAW => (Span32::new(9, 1), SpanSource::RawPayload, 1, Vec::new()),
            PADDED => (Span32::new(11, 1), SpanSource::RawPayload, 1, Vec::new()),
            UNICODE => (Span32::new(9, 6), SpanSource::RawPayload, 2, Vec::new()),
            ESCAPED => (Span32::new(0, 2), SpanSource::DecodedArena, 2, b"a\n".to_vec()),
            _ => return Err(parser_error()),
        };
        let payload_len = u32::try_from(payload.len()).expect("fixture payload fits ABI");
        let decoded_len = u32::try_from(decoded.len()).expect("fixture arena fits ABI");
        // SAFETY: Exact matching validates one complete object with exactly the
        // required string field under either registered schema. Raw spans cover
        // `a` or the valid UTF-8 scalars é and 😀; the decoded span covers `a`
        // and newline from the escape. Scalar counts, ranges and both checked
        // lengths describe these exact immutable buffers and the sole slot.
        let proof = unsafe {
            ValidationProof::issue_unchecked(fingerprint, payload_len, decoded_len)
        };
        Ok(ValidatedSlots {
            payload, decoded: decoded.into_boxed_slice(), proof,
            slots: vec![SlotValue::Str(StrRef { span, source, scalar_len })],
        })
    }

    #[derive(Clone, Copy, Default)]
    enum Fault {
        #[default]
        None,
        Fingerprint,
        Payload,
        PayloadLength,
        DecodedLength,
    }

    #[derive(Default)]
    struct Validator {
        calls: RefCell<Vec<(Payload, u64, Limits)>>,
        failure: Option<Error>,
        fault: Fault,
    }

    impl BodyValidator for Validator {
        fn validate(&self, payload: Payload, fingerprint: u64, limits: Limits) -> Result<ValidatedSlots> {
            self.calls.borrow_mut().push((payload.clone(), fingerprint, limits));
            if let Some(failure) = &self.failure { return Err(failure.clone()); }
            assert_eq!(fingerprint, FINGERPRINT);
            let issued_fingerprint = if matches!(self.fault, Fault::Fingerprint) {
                OTHER_FINGERPRINT
            } else {
                fingerprint
            };
            let mut slots = literal_slots(payload, issued_fingerprint)?;
            // Mutate already proven fixtures safely to exercise defensive
            // rejection. No invalid ABI proof is issued for these fault cases.
            match self.fault {
                Fault::Payload => slots.payload = Payload::from_slice(br#"{"text":"b"}"#),
                Fault::PayloadLength => slots.payload = Payload::from_slice(b""),
                Fault::DecodedLength => slots.decoded = vec![b'x'].into_boxed_slice(),
                Fault::None | Fault::Fingerprint => {}
            }
            // Deliberately leave decoded-cap enforcement to the HTTP root.
            Ok(slots)
        }
    }

    fn header(name: &str, value: &str) -> Header {
        Header::new(name, value).expect("valid header fixture")
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

    fn build_request(headers: Vec<Header>, limits: HttpLimits) -> Result<OutgoingRequest> {
        request(HttpMethod::Post, URI, headers, &Value::new(RAW), &Validator::default(), limits)
    }

    fn build_response(headers: Vec<Header>, limits: HttpLimits) -> Result<OutgoingResponse> {
        response(200, headers, &Value::new(RAW), &Validator::default(), limits)
    }

    fn assert_both(headers: Vec<Header>, code: ErrorCode) {
        assert_local(build_request(headers.clone(), HttpLimits::default()), code);
        assert_local(build_response(headers, HttpLimits::default()), code);
    }

    #[test]
    fn encoding_once_validates_and_retains_the_exact_transmitted_owner() {
        for bytes in [RAW, ESCAPED, UNICODE, PADDED] {
            let value = Value::new(bytes);
            let validator = Validator::default();
            let body = encode_body(&value, &validator, HttpLimits::default()).unwrap();
            assert_eq!(value.encodes.get(), 1);
            let calls = validator.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].1, FINGERPRINT);
            assert_eq!(body.payload().as_slice(), bytes);
            assert_eq!(body.payload().as_slice().as_ptr(), calls[0].0.as_slice().as_ptr());
            assert_eq!(body.proof().schema_fingerprint(), FINGERPRINT);
            assert_eq!(body.proof().payload_len(), u32::try_from(bytes.len()).unwrap());
            assert_eq!(body.proof().decoded_len(), u32::try_from(body.slots().decoded.len()).unwrap());
            let [SlotValue::Str(reference)] = body.slots().slots.as_slice() else {
                panic!("one proven string slot required");
            };
            let owner = match reference.source {
                SpanSource::RawPayload => body.payload().as_slice(),
                SpanSource::DecodedArena => &body.slots().decoded,
            };
            let text = core::str::from_utf8(reference.span.slice(owner).unwrap()).unwrap();
            assert_eq!(text, if bytes == ESCAPED { "a\n" } else if bytes == UNICODE { "é😀" } else { "a" });
            assert_eq!(text.chars().count(), usize::try_from(reference.scalar_len).unwrap());
            drop(calls);
            drop(validator);
            assert_eq!(body.payload().as_slice(), bytes);
        }
    }

    #[test]
    fn generated_headers_and_request_target_are_exact() {
        let request = build_request(Vec::new(), HttpLimits::default()).unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.uri, URI);
        assert_eq!(request.body.payload().as_slice(), RAW);
        let expected = vec![header("content-type", JSON_MEDIA_TYPE), header("content-length", "12")];
        assert_eq!(request.headers, expected);
        let response = build_response(Vec::new(), HttpLimits::default()).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.headers, expected);
        assert_eq!(response.body.payload().as_slice(), RAW);
    }

    #[test]
    fn builders_encode_and_validate_once_without_rewriting_json() {
        for bytes in [RAW, ESCAPED, UNICODE, PADDED] {
            for is_response in [false, true] {
                let value = Value::new(bytes);
                let validator = Validator::default();
                let (body, headers) = if is_response {
                    let output = response(201, Vec::new(), &value, &validator, HttpLimits::default()).unwrap();
                    (output.body, output.headers)
                } else {
                    let output = request(HttpMethod::Patch, URI, Vec::new(), &value, &validator,
                        HttpLimits::default()).unwrap();
                    (output.body, output.headers)
                };
                assert_eq!(value.encodes.get(), 1);
                let calls = validator.calls.borrow();
                assert_eq!(calls.len(), 1);
                assert_eq!(body.payload().as_slice(), bytes);
                assert_eq!(body.payload().as_slice().as_ptr(), calls[0].0.as_slice().as_ptr());
                assert_eq!(headers[1].value(), alloc::format!("{}", bytes.len()));
                incoming::check_headers(&headers, bytes.len(), HttpLimits::default()).unwrap();
            }
        }
    }

    #[test]
    fn normalization_preserves_order_values_unknown_duplicates_and_identity() {
        let supplied = vec![
            header("X-Trace", "first"), header("Content-Length", " \t00012\t "),
            header("Content-Type", " APPLICATION/Problem+JSON ; charset = \"UTF-8\"\t"),
            header("X-Trace", "é"), header("Content-Encoding", "\tIDENTITY "),
        ];
        let expected = vec![
            supplied[0].clone(), supplied[2].clone(), supplied[3].clone(), supplied[4].clone(),
            header("content-length", "12"),
        ];
        assert_eq!(build_request(supplied.clone(), HttpLimits::default()).unwrap().headers, expected);
        assert_eq!(build_response(supplied, HttpLimits::default()).unwrap().headers, expected);
        let supplied = vec![header("x", "1"), header("content-length", "0012"), header("x", "2")];
        let expected = vec![header("x", "1"), header("x", "2"),
            header("content-type", JSON_MEDIA_TYPE), header("content-length", "12")];
        assert_eq!(build_request(supplied.clone(), HttpLimits::default()).unwrap().headers, expected);
        assert_eq!(build_response(supplied, HttpLimits::default()).unwrap().headers, expected);
    }

    #[test]
    fn supplied_lengths_are_verified_before_normalization() {
        for value in ["12", "0012", " \t000012\t "] {
            let supplied = vec![header("Content-Length", value)];
            assert_eq!(build_request(supplied.clone(), HttpLimits::default()).unwrap().headers[1].value(), "12");
            assert_eq!(build_response(supplied, HttpLimits::default()).unwrap().headers[1].value(), "12");
        }
        for (value, code) in [
            ("", ErrorCode::FramingError), (" \t ", ErrorCode::FramingError),
            ("+12", ErrorCode::FramingError), ("-12", ErrorCode::FramingError),
            ("12,12", ErrorCode::FramingError), ("1 2", ErrorCode::FramingError),
            ("1\t2", ErrorCode::FramingError), ("12.0", ErrorCode::FramingError),
            ("0x0c", ErrorCode::FramingError), ("１２", ErrorCode::FramingError),
            ("12\u{a0}", ErrorCode::FramingError), ("18446744073709551616", ErrorCode::FramingError),
            ("0", ErrorCode::FramingError), ("11", ErrorCode::FramingError),
            ("13", ErrorCode::TransportIncomplete),
        ] {
            assert_both(vec![header("content-length", value)], code);
        }
    }

    #[test]
    fn duplicate_representation_headers_are_rejected_even_if_identical() {
        for (name, first, second) in [
            ("Content-Length", "12", "12"), ("Content-Length", "12", "13"),
            ("Content-Type", JSON_MEDIA_TYPE, JSON_MEDIA_TYPE),
            ("Content-Encoding", "identity", "identity"),
        ] {
            assert_both(vec![header(name, first), header(name, second)], ErrorCode::FramingError);
        }
    }

    #[test]
    fn valid_json_media_types_are_preserved_exactly() {
        for media in [
            "application/json", "APPLICATION/JSON", "application/problem+json",
            "application/vnd.example.v1+json", "application/json; charset=utf-8",
            "\tapplication/json ;\tCHARSET = \"UTF-8\" ",
        ] {
            let fields = vec![header("Content-Type", media)];
            assert_eq!(build_request(fields.clone(), HttpLimits::default()).unwrap().headers[0].value(), media);
            assert_eq!(build_response(fields, HttpLimits::default()).unwrap().headers[0].value(), media);
        }
    }

    #[test]
    fn invalid_media_types_and_parameters_are_never_repaired() {
        for media in [
            "", "text/json", "application/+json", "application/json-seq", "application/jsonp",
            "application/json,application/json", "application/foo/bar+json", "application/json;",
            "application/json; charset=latin1", "application/json; charset=\"utf-8",
            "application/json; charset=utf-8\"", "application/json; charset=\"utf-8\"junk",
            "application/json; unknown=utf-8", "application/json; charset=utf-8; charset=utf-8",
            "application/json; charset=utf-8; unknown=value",
        ] {
            assert_both(vec![header("Content-Type", media)], ErrorCode::TypeMismatch);
        }
    }

    #[test]
    fn transfer_framing_and_nonidentity_codings_are_forbidden() {
        for (name, value) in [
            ("Transfer-Encoding", ""), ("Transfer-Encoding", "chunked"),
            ("Transfer-Encoding", "identity"), ("Content-Encoding", ""),
            ("Content-Encoding", "gzip"), ("Content-Encoding", "br"),
            ("Content-Encoding", "identity, identity"), ("Content-Encoding", "identity; q=1"),
        ] {
            assert_both(vec![header(name, value)], ErrorCode::FramingError);
        }
    }

    #[test]
    fn all_supported_methods_and_absolute_uri_forms_are_preserved() {
        for method in [HttpMethod::Get, HttpMethod::Post, HttpMethod::Put, HttpMethod::Patch,
            HttpMethod::Delete, HttpMethod::Options]
        {
            for uri in ["http://localhost", "https://EXAMPLE.test./a%2fb?q=%20",
                "https://[2001:db8::1]:65535/v1?x=/a?b", URI]
            {
                let value = Value::new(RAW);
                let validator = Validator::default();
                let output = request(method, uri, Vec::new(), &value, &validator, HttpLimits::default()).unwrap();
                assert_eq!(output.method, method);
                assert_eq!(output.uri, uri);
                assert_eq!(value.encodes.get(), 1);
                assert_eq!(validator.calls.borrow().len(), 1);
            }
        }
    }

    #[test]
    fn head_and_invalid_targets_fail_before_encoding_or_validation() {
        let value = Value::new(RAW);
        let validator = Validator::default();
        assert_local(request(HttpMethod::Head, URI, Vec::new(), &value, &validator,
            HttpLimits::default()), ErrorCode::UnsupportedSchemaFeature);
        for uri in [
            "", "/v1", "//example.test/v1", "ftp://example.test/", "HTTPS://example.test/",
            "https:///", "https://user@example.test/", "https://example.test/#fragment",
            "https://example.test/a b", "https://example.test/a\tb", "https://example.test/a\nb",
            "https://example.test/\0", "https://example.test/\\x", "https://example.test/%",
            "https://example.test/%G0", "https://example.test:0", "https://example.test:65536",
            "https://-bad.test", "https://bad..test", "https://[:::1]/", "https://é.test/",
        ] {
            assert_local(request(HttpMethod::Post, uri, Vec::new(), &value, &validator,
                HttpLimits::default()), ErrorCode::ConstraintViolation);
        }
        assert_eq!(value.encodes.get(), 0);
        assert!(validator.calls.borrow().is_empty());
    }

    #[test]
    fn response_status_policy_covers_the_entire_boundary() {
        for status in (0u16..=600).chain(core::iter::once(u16::MAX)) {
            let value = Value::new(RAW);
            let validator = Validator::default();
            let output = response(status, Vec::new(), &value, &validator, HttpLimits::default());
            if (200..=299).contains(&status) && !matches!(status, 204 | 205) {
                assert_eq!(output.unwrap().status, status);
                assert_eq!(value.encodes.get(), 1);
                assert_eq!(validator.calls.borrow().len(), 1);
            } else {
                let code = if matches!(status, 204 | 205) { ErrorCode::TypeMismatch }
                    else { ErrorCode::ConstraintViolation };
                assert_local(output, code);
                assert_eq!(value.encodes.get(), 0);
                assert!(validator.calls.borrow().is_empty());
            }
        }
    }

    #[test]
    fn every_entry_point_rejects_invalid_limits_before_other_work() {
        for case in 0..6 {
            let mut limits = HttpLimits::default();
            match case {
                0 => limits.max_body_bytes = 0,
                1 => limits.max_headers = 0,
                2 => limits.max_header_bytes = 0,
                3 => limits.max_header_bytes = 1,
                4 => limits.syntax.max_payload_bytes = 0,
                _ => limits.syntax.max_depth = 0,
            }
            let value = Value::new(RAW);
            let validator = Validator::default();
            assert_local(encode_body(&value, &validator, limits), ErrorCode::ConstraintViolation);
            assert_local(request(HttpMethod::Head, "", Vec::new(), &value, &validator, limits),
                ErrorCode::ConstraintViolation);
            assert_local(response(204, Vec::new(), &value, &validator, limits), ErrorCode::ConstraintViolation);
            assert_eq!(value.encodes.get(), 0);
            assert!(validator.calls.borrow().is_empty());
        }
    }

    #[test]
    fn payload_cap_is_inclusive_and_uses_the_smaller_limit() {
        for syntax_cap in [false, true] {
            for exact in [false, true] {
                let mut limits = HttpLimits::default();
                let cap = u32::try_from(RAW.len() - usize::from(!exact)).unwrap();
                if syntax_cap { limits.syntax.max_payload_bytes = cap; }
                else { limits.max_body_bytes = cap; }
                for entry in 0..3 {
                    let value = Value::new(RAW);
                    let validator = Validator::default();
                    let output = match entry {
                        0 => encode_body(&value, &validator, limits),
                        1 => request(HttpMethod::Post, URI, Vec::new(), &value, &validator, limits)
                            .map(|message| message.body),
                        _ => response(200, Vec::new(), &value, &validator, limits)
                            .map(|message| message.body),
                    };
                    assert_eq!(value.encodes.get(), 1);
                    if exact {
                        assert_eq!(output.unwrap().payload().as_slice(), RAW);
                        assert_eq!(validator.calls.borrow().len(), 1);
                        assert_eq!(validator.calls.borrow()[0].2.max_payload_bytes, cap);
                    } else {
                        assert_local(output, ErrorCode::PayloadTooLarge);
                        assert!(validator.calls.borrow().is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn remaining_syntax_limits_are_forwarded_without_widening() {
        let syntax = Limits {
            max_payload_bytes: 64, max_decoded_bytes: 5, max_depth: 3,
            max_fields_per_object: 2, max_string_bytes: 7, max_number_digits: 9,
            max_array_items: 4, reject_duplicate_keys: false, reject_unknown_keys: false,
        };
        let limits = HttpLimits { max_body_bytes: 32, syntax, ..HttpLimits::default() };
        let validator = Validator::default();
        encode_body(&Value::new(RAW), &validator, limits).unwrap();
        assert_eq!(validator.calls.borrow()[0].2, Limits { max_payload_bytes: 32, ..syntax });
    }

    #[test]
    fn final_header_limits_include_generated_fields_and_utf8_storage() {
        let fields = vec![header("X-Label", "é")];
        let output = build_request(fields.clone(), HttpLimits::default()).unwrap();
        let bytes = output.headers.iter().fold(2usize, |sum, field| {
            sum + field.name().len() + field.value().len() + 4
        });
        let limits = HttpLimits {
            max_headers: u16::try_from(output.headers.len()).unwrap(),
            max_header_bytes: u32::try_from(bytes).unwrap(), ..HttpLimits::default()
        };
        assert_eq!(build_request(fields.clone(), limits).unwrap().headers, output.headers);
        assert_eq!(build_response(fields.clone(), limits).unwrap().headers, output.headers);
        for is_count in [false, true] {
            let mut below = limits;
            let code = if is_count {
                below.max_headers -= 1;
                ErrorCode::BudgetExhausted
            } else {
                below.max_header_bytes -= 1;
                ErrorCode::PayloadTooLarge
            };
            assert_local(build_request(fields.clone(), below), code);
            assert_local(build_response(fields.clone(), below), code);
        }
        assert_both_limits_generated_headers();
    }

    fn assert_both_limits_generated_headers() {
        let limits = HttpLimits { max_headers: 2, max_header_bytes: 54, ..HttpLimits::default() };
        // These two fields occupy exactly 54 bytes including the final CRLF.
        build_request(Vec::new(), limits).unwrap();
        build_response(Vec::new(), limits).unwrap();
        assert_local(build_request(Vec::new(), HttpLimits { max_headers: 1, ..limits }), ErrorCode::BudgetExhausted);
        assert_local(build_response(Vec::new(), HttpLimits { max_headers: 1, ..limits }), ErrorCode::BudgetExhausted);
        assert_local(build_request(Vec::new(), HttpLimits { max_header_bytes: 53, ..limits }), ErrorCode::PayloadTooLarge);
        assert_local(build_response(Vec::new(), HttpLimits { max_header_bytes: 53, ..limits }), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn limits_apply_to_final_headers_after_length_normalization() {
        let limits = HttpLimits { max_headers: 2, max_header_bytes: 54, ..HttpLimits::default() };
        let supplied = vec![header("Content-Length", &alloc::format!(" {}12\t ", "0".repeat(128)))];
        let expected = vec![header("content-type", JSON_MEDIA_TYPE), header("content-length", "12")];
        assert_eq!(build_request(supplied.clone(), limits).unwrap().headers, expected);
        assert_eq!(build_response(supplied, limits).unwrap().headers, expected);
        let supplied = expected.clone();
        assert_eq!(build_request(supplied.clone(), limits).unwrap().headers, expected);
        assert_eq!(build_response(supplied, limits).unwrap().headers, expected);
    }

    #[test]
    fn utf8_lengths_measure_bytes_and_never_character_count() {
        assert_eq!(UNICODE.len(), 17);
        assert_eq!(core::str::from_utf8(UNICODE).unwrap().chars().count(), 13);
        let supplied = vec![header("content-length", "17")];
        let output = response(200, supplied, &Value::new(UNICODE), &Validator::default(),
            HttpLimits { max_body_bytes: 17, ..HttpLimits::default() }).unwrap();
        assert_eq!(output.headers[1].value(), "17");
        assert_eq!(output.body.payload().as_slice(), UNICODE);
        assert_local(response(200, vec![header("content-length", "13")], &Value::new(UNICODE),
            &Validator::default(), HttpLimits::default()), ErrorCode::FramingError);
        assert_local(encode_body(&Value::new(UNICODE), &Validator::default(),
            HttpLimits { max_body_bytes: 16, ..HttpLimits::default() }), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn encoder_failures_discard_partial_bodies_and_preserve_metadata() {
        let failure = Error::new(ErrorCode::InvalidNumber, ErrorSource::Encode, 9, "nonfinite number")
            .with_schema(30, 4);
        for entry in 0..3 {
            let value = Value { failure: Some(failure.clone()), ..Value::new(RAW) };
            let validator = Validator::default();
            let result = match entry {
                0 => encode_body(&value, &validator, HttpLimits::default()),
                1 => request(HttpMethod::Post, URI, Vec::new(), &value, &validator, HttpLimits::default())
                    .map(|message| message.body),
                _ => response(200, Vec::new(), &value, &validator, HttpLimits::default())
                    .map(|message| message.body),
            };
            assert_eq!(error(result), failure);
            assert_eq!(value.encodes.get(), 1);
            assert!(validator.calls.borrow().is_empty());
        }
    }

    #[test]
    fn validator_failures_preserve_metadata_through_every_builder() {
        let failure = Error::new(ErrorCode::PatternMismatch, ErrorSource::SchemaVm, 9, "text rejected")
            .with_schema(30, 8);
        for entry in 0..3 {
            let value = Value::new(RAW);
            let validator = Validator { failure: Some(failure.clone()), ..Validator::default() };
            let result = match entry {
                0 => encode_body(&value, &validator, HttpLimits::default()),
                1 => request(HttpMethod::Post, URI, Vec::new(), &value, &validator, HttpLimits::default())
                    .map(|message| message.body),
                _ => response(200, Vec::new(), &value, &validator, HttpLimits::default())
                    .map(|message| message.body),
            };
            assert_eq!(error(result), failure);
            assert_eq!(value.encodes.get(), 1);
            assert_eq!(validator.calls.borrow().len(), 1);
        }
    }

    #[test]
    fn encoder_success_cannot_replace_schema_validation() {
        for bytes in [
            &b""[..], &b"null"[..], &b"NaN"[..], &b"Infinity"[..], &b"{\"text\":1}"[..],
            &b"{\"text\":\"a\"}true"[..], &b"{\"text\":\"a\"}\xff"[..],
        ] {
            let value = Value::new(bytes);
            let validator = Validator::default();
            assert_eq!(error(encode_body(&value, &validator, HttpLimits::default())), parser_error());
            assert_eq!(value.encodes.get(), 1);
            assert_eq!(validator.calls.borrow()[0].0.as_slice(), bytes);
        }
    }

    #[test]
    fn inconsistent_proofs_and_changed_owners_are_rejected_by_every_builder() {
        for fault in [Fault::Fingerprint, Fault::Payload, Fault::PayloadLength, Fault::DecodedLength] {
            let code = if matches!(fault, Fault::Fingerprint) { ErrorCode::SchemaImageInvalid }
                else { ErrorCode::InternalError };
            for entry in 0..3 {
                let value = Value::new(RAW);
                let validator = Validator { fault, ..Validator::default() };
                let result = match entry {
                    0 => encode_body(&value, &validator, HttpLimits::default()),
                    1 => request(HttpMethod::Post, URI, Vec::new(), &value, &validator, HttpLimits::default())
                        .map(|message| message.body),
                    _ => response(200, Vec::new(), &value, &validator, HttpLimits::default())
                        .map(|message| message.body),
                };
                assert_local(result, code);
                assert_eq!(value.encodes.get(), 1);
                assert_eq!(validator.calls.borrow().len(), 1);
            }
        }
    }

    #[test]
    fn decoded_arena_cap_is_exact_and_zero_allows_unescaped_strings() {
        for cap in [0, 1, 2] {
            let mut limits = HttpLimits::default();
            limits.syntax.max_decoded_bytes = cap;
            encode_body(&Value::new(RAW), &Validator::default(), limits).unwrap();
            let result = encode_body(&Value::new(ESCAPED), &Validator::default(), limits);
            if cap == 2 { assert_eq!(&*result.unwrap().slots().decoded, b"a\n"); }
            else { assert_local(result, ErrorCode::DecodedArenaOverflow); }
        }
    }

    #[test]
    fn decimal_helpers_cover_zero_abi_maximum_and_overflow() {
        for length in [0u32, 1, 9, 10, 99, 100, u32::MAX] {
            let length = usize::try_from(length).unwrap();
            let field = length_header(length).unwrap();
            assert_eq!(field.name(), "content-length");
            assert_eq!(field.value(), alloc::format!("{length}"));
            verify_supplied_length(field.value(), length).unwrap();
        }
        verify_supplied_length(" \t0000\t ", 0).unwrap();
        assert_local(verify_supplied_length("18446744073709551616", 0), ErrorCode::FramingError);
        if let Ok(length) = usize::try_from(u64::MAX) {
            verify_supplied_length("18446744073709551615", length).unwrap();
        } else {
            assert_local(verify_supplied_length("18446744073709551615", 0), ErrorCode::FramingError);
        }
        if let Ok(length) = usize::try_from(u64::from(u32::MAX) + 1) {
            assert_local(length_header(length), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn body_sink_failed_write_and_reservation_retain_existing_prefix() {
        let mut sink = BodySink::new(3);
        sink.write_bytes(b"ab").unwrap();
        assert_local(sink.reserve(usize::MAX), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.as_bytes(), b"ab");
        assert_local(sink.write_bytes(b"cd"), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.as_bytes(), b"ab");
        sink.reserve(1).unwrap();
        assert_eq!(sink.as_bytes(), b"ab");
        sink.write_byte(b'c').unwrap();
        sink.write_bytes(b"").unwrap();
        assert_eq!(sink.into_payload().as_slice(), b"abc");
    }
}
