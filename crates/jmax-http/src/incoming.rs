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
    Ok(unsafe { ParsedMessage::new_proven(payload, decoded, value, proof) })
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, task::Wake, vec};
    use core::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };
    use jmax_abi::{
        FieldId, SchemaFieldIr, SchemaId, SchemaIr, SlotValue, Span32, SpanSource, StrRef,
        syntax::{ScalarKind, Scratch},
    };
    use jmax_nagoya::{Admission, AdmissionLimits, ResourceRequest};

    const FINGERPRINT: u64 = 0x5732_0030;
    const OTHER_FINGERPRINT: u64 = FINGERPRINT + 1;
    const RAW: &[u8] = br#"{"text":"a"}"#;
    const ESCAPED: &[u8] = br#"{"text":"a\n"}"#;
    const PADDED: &[u8] = b" \n{\"text\":\"a\"}\t ";

    fn header(name: &str, value: &str) -> Header {
        Header::new(name, value).expect("valid header fixture")
    }

    fn headers() -> Vec<Header> {
        vec![header("Content-Type", JSON_MEDIA_TYPE)]
    }

    fn response(bytes: &[u8]) -> IncomingResponse {
        IncomingResponse {
            status: 200,
            headers: headers(),
            body: Payload::from_slice(bytes),
            completion: BodyCompletion::Complete,
        }
    }

    fn request() -> IncomingRequest {
        IncomingRequest {
            method: HttpMethod::Post,
            uri: String::from("/v1/text?format=json"),
            headers: headers(),
            body: Payload::from_slice(RAW),
            completion: BodyCompletion::Complete,
        }
    }

    fn error<T>(result: Result<T>) -> Error {
        match result {
            Err(error) => error,
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

    fn parser_error() -> Error {
        Error::new(ErrorCode::TrailingContent, ErrorSource::Syntax, 12, "literal JSON required")
            .with_schema(30, 7)
    }

    fn decoder_error() -> Error {
        Error::new(ErrorCode::TypeMismatch, ErrorSource::Decode, 9, "typed decode rejected")
            .with_schema(30, 8)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Text(StrRef);

    impl SchemaProvider for Text {
        fn schema_ir() -> SchemaIr {
            SchemaIr {
                name: String::from("IncomingText"),
                schema_id: SchemaId(30),
                required_count: 1,
                reject_unknown: true,
                fields: vec![SchemaFieldIr {
                    field_id: FieldId(0), name: String::from("text"), required: true,
                    scalar_kind: Some(ScalarKind::String), format: None,
                    min_len: None, max_len: None, min_i64: None, max_i64: None,
                }],
            }
        }

        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl JsonDecode for Text {
        fn decode_slots(slots: &ValidatedSlots) -> Result<Self> {
            assert_eq!(slots.proof.schema_fingerprint(), Self::schema_fingerprint());
            match slots.slots.as_slice() {
                [SlotValue::Str(reference)] => Ok(Self(*reference)),
                _ => Err(decoder_error()),
            }
        }
    }

    struct RejectDecode;

    impl SchemaProvider for RejectDecode {
        fn schema_ir() -> SchemaIr { Text::schema_ir() }
        fn schema_fingerprint() -> u64 { FINGERPRINT }
    }

    impl JsonDecode for RejectDecode {
        fn decode_slots(_: &ValidatedSlots) -> Result<Self> { Err(decoder_error()) }
    }

    /// A finite, exact validator for the complete literal objects above. Both
    /// registered fingerprints describe the same shape under different IDs.
    fn literal_slots(payload: Payload, fingerprint: u64) -> Result<ValidatedSlots> {
        if !matches!(fingerprint, FINGERPRINT | OTHER_FINGERPRINT) {
            return Err(http_error(ErrorCode::SchemaImageInvalid, "unknown fixture schema"));
        }
        let (offset, decoded, source, scalar_len) = match payload.as_slice() {
            RAW => (9, Vec::new(), SpanSource::RawPayload, 1),
            PADDED => (11, Vec::new(), SpanSource::RawPayload, 1),
            ESCAPED => (0, b"a\n".to_vec(), SpanSource::DecodedArena, 2),
            _ => return Err(parser_error()),
        };
        let reference = StrRef { span: Span32::new(offset, scalar_len), source, scalar_len };
        let payload_len = u32::try_from(payload.len()).expect("fixture payload fits ABI");
        let decoded_len = u32::try_from(decoded.len()).expect("fixture arena fits ABI");
        // SAFETY: Exact matching validates a complete object with precisely one
        // required string field and no unknown fields under the registered
        // schema. The raw spans identify ASCII `a`; the decoded span identifies
        // the two UTF-8 scalars `a` and newline decoded from the escape. Checked
        // lengths describe these exact immutable owners and all spans fit them.
        let proof = unsafe { ValidationProof::issue_unchecked(fingerprint, payload_len, decoded_len) };
        Ok(ValidatedSlots {
            payload, decoded: decoded.into_boxed_slice(),
            slots: vec![SlotValue::Str(reference)], proof,
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
        calls: AtomicUsize,
        observed_payload_limit: AtomicUsize,
        fault: Fault,
        failure: Option<Error>,
        cancel_during_validation: Option<CancellationToken>,
    }

    impl Validator {
        fn validate_literal(&self, payload: Payload, limits: Limits) -> Result<ValidatedSlots> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.observed_payload_limit.store(
                usize::try_from(limits.max_payload_bytes).expect("test limit fits"), Ordering::Relaxed,
            );
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            let fingerprint = if matches!(self.fault, Fault::Fingerprint) {
                OTHER_FINGERPRINT
            } else {
                FINGERPRINT
            };
            let mut slots = literal_slots(payload, fingerprint)?;
            // Deliberate safe mutations of already proven output exercise the
            // defensive ownership checks, rather than issuing invalid proofs.
            match self.fault {
                Fault::Payload => slots.payload = Payload::from_slice(br#"{"text":"b"}"#),
                Fault::PayloadLength => slots.payload = Payload::from_slice(b""),
                Fault::DecodedLength => slots.decoded = vec![b'x'].into_boxed_slice(),
                Fault::None | Fault::Fingerprint => {}
            }
            if let Some(token) = &self.cancel_during_validation {
                token.cancel();
            }
            Ok(slots)
        }
    }

    impl BodyValidator for Validator {
        fn validate(&self, payload: Payload, fingerprint: u64, limits: Limits) -> Result<ValidatedSlots> {
            assert_eq!(fingerprint, FINGERPRINT, "HTTP must pass the exact requested schema");
            self.validate_literal(payload, limits)
        }
    }

    impl ParseValidator for Validator {
        fn validate(
            &self,
            payload: Payload,
            limits: Limits,
            scratch: &mut Scratch,
            scratch_budget_bytes: usize,
            retained_budget_bytes: usize,
            cancellation: &CancellationToken,
        ) -> Result<ValidatedSlots> {
            cancellation.check()?;
            assert!(scratch.decoded_buf.is_empty());
            assert!(scratch.stack.is_empty());
            assert!(scratch.seen_words.is_empty());
            assert_eq!(scratch_budget_bytes, 128);
            assert!(retained_budget_bytes >= payload.len());
            let slots = self.validate_literal(payload, limits)?;
            if slots.decoded.len() > usize::try_from(limits.max_decoded_bytes).expect("test limit fits") {
                return Err(Error::new(
                    ErrorCode::DecodedArenaOverflow, ErrorSource::SchemaVm, 10, "fixture decoded cap",
                ));
            }
            Ok(slots)
        }
    }

    #[test]
    fn accepted_json_media_types_and_identity_codings() {
        for media in [
            "application/json", "APPLICATION/JSON", " application/problem+json\t",
            "application/vnd.example.v2+JSON", "application/a+b+json",
            "application/json;charset=utf-8", "application/json ; CHARSET = UTF-8",
            "\tapplication/problem+json; charset=\"Utf-8\" \t",
        ] {
            let mut fields = vec![header("Content-Type", media)];
            fields.push(header("CONTENT-ENCODING", " \tIdEnTiTy\t "));
            check_headers(&fields, 4, HttpLimits::default()).unwrap();
        }
    }

    #[test]
    fn media_type_rejects_prefixes_invalid_tokens_and_parameters() {
        for media in [
            "", "json", "text/json", "application/", "application/+json",
            "application/jsonp", "application/json,application/json", "application/json/json",
            "application /json", "application/ json", "application/a b+json",
            "application/é+json", "application/json;", "application/json;;charset=utf-8",
            "application/json;charset", "application/json;charset=", "application/json;charset=utf8",
            "application/json;charset=latin1", "application/json;charset='utf-8'",
            "application/json;charset=\"utf-8", "application/json;charset=utf-8\"",
            "application/json;charset=\"utf-8\"extra", "application/json;charset=\"utf\\-8\"",
            "application/json;profile=x", "application/json;charset=utf-8;charset=utf-8",
            "application/json;charset=utf-8;profile=x", "application/json;charset=utf-8=extra",
            "application/json;charset=\"utf-8;\"", "application/json\u{a0}",
        ] {
            assert_local(check_headers(&[header("content-type", media)], 4, HttpLimits::default()),
                ErrorCode::TypeMismatch);
        }
        assert_local(check_headers(&[], 4, HttpLimits::default()), ErrorCode::TypeMismatch);
    }

    #[test]
    fn repeated_unknown_fields_are_legal_but_representation_fields_are_unique() {
        let mut fields = headers();
        fields.extend([header("x-tag", "a"), header("X-TAG", "b")]);
        check_headers(&fields, 4, HttpLimits::default()).unwrap();
        for (name, value) in [
            ("content-length", "4"), ("content-type", JSON_MEDIA_TYPE),
            ("content-encoding", "identity"),
        ] {
            let mut fields = headers();
            if name != "content-type" { fields.push(header(name, value)); }
            fields.push(header(name, value));
            assert_local(check_headers(&fields, 4, HttpLimits::default()), ErrorCode::FramingError);
        }
    }

    #[test]
    fn content_length_accepts_only_bounded_decimal_with_outer_ows() {
        for value in ["4", "0004", "\t 0004 \t"] {
            let mut fields = headers();
            fields.push(header("content-length", value));
            check_headers(&fields, 4, HttpLimits::default()).unwrap();
        }
        for value in [
            "", " \t", "+4", "-4", "4,4", "4, 4", "4 0", "4\t0", "0x4", "4.0", "４",
            "18446744073709551616", "9999999999999999999999999999999999",
        ] {
            let mut fields = headers();
            fields.push(header("content-length", value));
            assert_local(check_headers(&fields, 4, HttpLimits::default()), ErrorCode::FramingError);
        }
        assert_eq!(parse_content_length("0000").unwrap(), 0);
        assert_eq!(parse_content_length(&alloc::format!("{}", usize::MAX)).unwrap(), usize::MAX);
        if usize::BITS < 64 {
            assert_local(parse_content_length("18446744073709551615"), ErrorCode::FramingError);
        } else {
            assert_eq!(parse_content_length("18446744073709551615").unwrap(), usize::MAX);
        }
    }

    #[test]
    fn declared_length_mismatch_distinguishes_incomplete_and_excess_bytes() {
        for (value, code) in [("5", ErrorCode::TransportIncomplete), ("3", ErrorCode::FramingError)] {
            let mut fields = headers();
            fields.push(header("content-length", value));
            assert_local(check_headers(&fields, 4, HttpLimits::default()), code);
        }
        let mut fields = headers();
        fields.push(header("content-length", "0"));
        check_headers(&fields, 0, HttpLimits::default()).unwrap();
        check_headers(&headers(), 0, HttpLimits::default()).unwrap();
    }

    #[test]
    fn transfer_metadata_and_undecoded_codings_are_rejected() {
        for (name, values) in [
            ("transfer-encoding", &["", "identity", "chunked", "gzip, chunked"][..]),
            ("content-encoding", &["", "gzip", "br", "identity, identity", "identity; q=1"][..]),
        ] {
            for value in values {
                let mut fields = headers();
                fields.push(header(name, value));
                assert_local(check_headers(&fields, 4, HttpLimits::default()), ErrorCode::FramingError);
            }
        }
    }

    #[test]
    fn header_byte_accounting_includes_separators_and_final_crlf() {
        let fields = headers();
        let exact = u32::try_from(2 + "content-type".len() + JSON_MEDIA_TYPE.len() + 4).unwrap();
        let mut limits = HttpLimits { max_header_bytes: exact, ..HttpLimits::default() };
        check_headers(&fields, 4, limits).unwrap();
        limits.max_header_bytes -= 1;
        assert_local(check_headers(&fields, 4, limits), ErrorCode::PayloadTooLarge);
        limits.max_header_bytes = 2;
        assert_local(check_headers(&[], 4, limits), ErrorCode::TypeMismatch);
        assert_eq!(header_storage(2, 1, 2).unwrap(), 9);
        for (current, name, value) in [(usize::MAX, 1, 0), (0, usize::MAX, 1), (0, 0, usize::MAX)] {
            assert_local(header_storage(current, name, value), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn header_count_and_effective_body_caps_are_exact() {
        let mut limits = HttpLimits { max_headers: 1, max_body_bytes: 4, ..HttpLimits::default() };
        check_headers(&headers(), 4, limits).unwrap();
        assert_local(check_headers(&[header("content-type", JSON_MEDIA_TYPE), header("x", "")], 4, limits),
            ErrorCode::BudgetExhausted);
        assert_local(check_headers(&headers(), 5, limits), ErrorCode::PayloadTooLarge);
        limits.max_body_bytes = 8;
        limits.syntax.max_payload_bytes = 3;
        check_headers(&headers(), 3, limits).unwrap();
        assert_local(check_headers(&headers(), 4, limits), ErrorCode::PayloadTooLarge);
        if let Ok(too_large) = usize::try_from(u64::from(u32::MAX) + 1) {
            assert_local(check_headers(&headers(), too_large, HttpLimits::default()), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn header_storage_counts_utf8_bytes_and_complete_bodies_are_not_sniffed() {
        let fields = vec![header("content-type", JSON_MEDIA_TYPE), header("x-label", "é")];
        let bytes = 2 + "content-type".len() + JSON_MEDIA_TYPE.len() + 4
            + "x-label".len() + "é".len() + 4;
        let mut limits = HttpLimits {
            max_body_bytes: 2,
            max_header_bytes: u32::try_from(bytes).unwrap(),
            ..HttpLimits::default()
        };
        // Metadata checks measure bytes, leaving JSON syntax to the validator.
        check_headers(&fields, "é".len(), limits).unwrap();
        limits.max_header_bytes -= 1;
        assert_local(check_headers(&fields, "é".len(), limits), ErrorCode::PayloadTooLarge);
        limits.max_header_bytes += 1;
        limits.max_body_bytes = 1;
        assert_local(check_headers(&fields, "é".len(), limits), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn every_limits_bearing_entry_point_preflights_invalid_configuration() {
        for case in 0..5 {
            let mut limits = HttpLimits::default();
            match case {
                0 => limits.max_body_bytes = 0,
                1 => limits.max_header_bytes = 1,
                2 => limits.max_headers = 0,
                3 => limits.syntax.max_payload_bytes = 0,
                _ => limits.syntax.max_depth = 0,
            }
            let validator = Validator::default();
            let mut input = response(RAW);
            input.completion = BodyCompletion::Incomplete;
            input.status = 0;
            assert_local(check_headers(&[], usize::MAX, limits), ErrorCode::ConstraintViolation);
            assert_local(check_response(&input, limits), ErrorCode::ConstraintViolation);
            assert_local(check_request(&request(), limits), ErrorCode::ConstraintViolation);
            assert_local(validate_response(input.clone(), &validator, FINGERPRINT, limits), ErrorCode::ConstraintViolation);
            assert_local(decode_response::<Text>(input, &validator, limits), ErrorCode::ConstraintViolation);
            assert_eq!(validator.calls.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn valid_absolute_and_origin_targets_preserve_all_legal_components() {
        for uri in [
            "http://a", "https://EXAMPLE.test.", "https://a-b.test:1/",
            "https://a:65535?", "https://a:00080/path?x=1?y=2",
            "http://127.0.0.1/", "http://[::1]/", "http://[2001:db8::1]:443/a",
            "https://[::ffff:192.0.2.1]/?x=y", "http://[1:2:3:4:5:6:7:8]:65535",
            "http://a/a-z_A.~!$&'()*+,;=:@/%2f?/?x=%00&y=%FF",
        ] {
            validate_uri(uri, true).unwrap();
            validate_uri(uri, false).unwrap();
        }
        for uri in ["/", "/a//b", "/?", "/a:b@c;d=x?q=one/two?three", "/%2F%5C%23%20"] {
            validate_uri(uri, false).unwrap();
            assert_local(validate_uri(uri, true), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn uri_rejects_ambiguous_authorities_and_bad_targets() {
        for uri in [
            "", "*", "relative", "?query", "//host/path", "///host", "HTTP://host", "Https://host",
            "ftp://host", "http:/host", "http://", "http:///path", "http://?query",
            "https://user@host", "https://user:pass@host", "https://host#fragment",
            "https://host/a b", "https://host/a\tb", "https://host/a\nb", "https://host/\0",
            "https://host/\u{7f}", "https://é.test/", "https://host/é", "https://host\\evil/",
            "https://host/%", "https://host/%0", "https://host/%gg", "https://host/?x=%Q0",
            "https://host/<", "https://host/>", "https://host/\"", "https://host/{x}",
            "https://host/[x]", "https://host/|", "https://host/^", "https://host/`",
            "https://.host", "https://host..test", "https://host..", "https://.",
            "https://-host", "https://host-", "https://bad_host", "https://h%6fst",
            "https://host:", "https://host:0", "https://host:65536", "https://host:99999999999999",
            "https://host:+80", "https://host:-1", "https://host:8x", "https://host:80:90",
            "https://::1/", "https://[::1", "https://[]/", "https://[host]/",
            "https://[1:2:3:4:5:6:7]/", "https://[1:2:3:4:5:6:7:8:9]/",
            "https://[1::2::3]/", "https://[12345::]/", "https://[::ffff:256.1.1.1]/",
            "https://[fe80::1%25eth0]/", "https://[::1]extra/", "https://[::1]:/",
            "https://[::1]:0/", "https://[::1]:65536/", "https://[::1]@host/",
        ] {
            assert_local(validate_uri(uri, true), ErrorCode::ConstraintViolation);
            assert_local(validate_uri(uri, false), ErrorCode::ConstraintViolation);
        }
        for uri in ["//host", "/#fragment", "/a b", "/%0x", "/[bad]", "/\\host", "/é"] {
            assert_local(validate_uri(uri, false), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn uri_hostname_length_and_label_boundaries() {
        let label = "a".repeat(63);
        validate_uri(&alloc::format!("https://{label}/"), true).unwrap();
        assert_local(validate_uri(&alloc::format!("https://{}a/", label), true), ErrorCode::ConstraintViolation);
        let host = alloc::format!("{label}.{label}.{label}.{}", "b".repeat(61));
        assert_eq!(host.len(), 253);
        validate_uri(&alloc::format!("http://{host}/"), true).unwrap();
        assert_local(validate_uri(&alloc::format!("http://{host}./"), true), ErrorCode::ConstraintViolation);
        let rooted = alloc::format!("{label}.{label}.{label}.{}.", "b".repeat(60));
        assert_eq!(rooted.len(), 253);
        validate_uri(&alloc::format!("http://{rooted}/"), true).unwrap();
    }

    #[test]
    fn response_status_boundaries_and_completion_precedence() {
        let mut input = response(RAW);
        for status in 0u16..=600 {
            input.status = status;
            if (200..=299).contains(&status) && !matches!(status, 204 | 205) {
                check_response(&input, HttpLimits::default()).unwrap();
            } else {
                let code = if matches!(status, 204 | 205) { ErrorCode::TypeMismatch }
                    else { ErrorCode::ConstraintViolation };
                assert_local(check_response(&input, HttpLimits::default()), code);
            }
        }
        input.status = u16::MAX;
        assert_local(check_response(&input, HttpLimits::default()), ErrorCode::ConstraintViolation);
        input.completion = BodyCompletion::Incomplete;
        input.headers.clear();
        for status in [0, 100, 200, 204, 205, 400, 599, u16::MAX] {
            input.status = status;
            assert_local(check_response(&input, HttpLimits::default()), ErrorCode::TransportIncomplete);
        }
    }

    #[test]
    fn nominal_success_never_bypasses_header_checks() {
        let mut input = response(RAW);
        input.headers.push(header("transfer-encoding", "chunked"));
        assert_local(check_response(&input, HttpLimits::default()), ErrorCode::FramingError);
        input.headers = vec![header("content-type", "text/plain")];
        assert_local(check_response(&input, HttpLimits::default()), ErrorCode::TypeMismatch);
    }

    #[test]
    fn requests_check_completion_head_uri_headers_and_size() {
        let mut input = request();
        for method in [HttpMethod::Get, HttpMethod::Post, HttpMethod::Put, HttpMethod::Patch,
            HttpMethod::Delete, HttpMethod::Options]
        {
            input.method = method;
            check_request(&input, HttpLimits::default()).unwrap();
        }
        input.uri = String::from("https://example.test/v1");
        check_request(&input, HttpLimits::default()).unwrap();
        input.method = HttpMethod::Head;
        assert_local(check_request(&input, HttpLimits::default()), ErrorCode::UnsupportedSchemaFeature);
        input.completion = BodyCompletion::Incomplete;
        input.uri.clear();
        input.headers.clear();
        assert_local(check_request(&input, HttpLimits::default()), ErrorCode::TransportIncomplete);
        input = request();
        input.uri = String::from("//host");
        assert_local(check_request(&input, HttpLimits::default()), ErrorCode::ConstraintViolation);
        input = request();
        input.headers.clear();
        assert_local(check_request(&input, HttpLimits::default()), ErrorCode::TypeMismatch);
        input = request();
        let limits = HttpLimits { max_body_bytes: 1, ..HttpLimits::default() };
        assert_local(check_request(&input, limits), ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn validation_keeps_exact_original_payload_and_effective_limits() {
        let validator = Validator::default();
        let input = response(PADDED);
        let original = input.body.clone();
        let limits = HttpLimits { max_body_bytes: 20, ..HttpLimits::default() };
        let body = validate_response(input, &validator, FINGERPRINT, limits).unwrap();
        assert_eq!(body.payload().as_slice(), PADDED);
        assert_eq!(body.payload().as_slice().as_ptr(), original.as_slice().as_ptr());
        assert_eq!(body.proof().schema_fingerprint(), FINGERPRINT);
        assert_eq!(validator.calls.load(Ordering::Relaxed), 1);
        assert_eq!(validator.observed_payload_limit.load(Ordering::Relaxed), 20);
    }

    #[test]
    fn decoding_retains_raw_and_decoded_owners_and_resolves_references() {
        for bytes in [RAW, ESCAPED, PADDED] {
            for shared in [false, true] {
                let validator = Validator::default();
                let mut input = response(bytes);
                if !shared { input.body = Payload::from_vec(bytes.to_vec()); }
                let original = input.body.clone();
                let parsed = decode_response::<Text>(input, &validator, HttpLimits::default()).unwrap();
                assert_eq!(parsed.raw_bytes(), bytes);
                assert_eq!(parsed.raw_bytes().as_ptr(), original.as_slice().as_ptr());
                assert_eq!(parsed.resolve_str(parsed.values().0), Some(if bytes == ESCAPED { "a\n" } else { "a" }));
                assert_eq!(parsed.proof().payload_len(), u32::try_from(bytes.len()).unwrap());
                assert_eq!(parsed.proof().decoded_len(), u32::try_from(parsed.decoded_bytes().len()).unwrap());
                assert_eq!(validator.calls.load(Ordering::Relaxed), 1);
            }
        }
    }

    #[test]
    fn validator_and_decoder_errors_retain_all_metadata() {
        let failure = parser_error();
        let validator = Validator { failure: Some(failure.clone()), ..Validator::default() };
        assert_eq!(error(validate_response(response(RAW), &validator, FINGERPRINT, HttpLimits::default())), failure);
        let validator = Validator::default();
        assert_eq!(error(decode_response::<RejectDecode>(response(RAW), &validator, HttpLimits::default())), decoder_error());
        for bytes in [&b""[..], &b"null"[..], &b"{\"text\":\"a\"}true"[..]] {
            assert_eq!(error(decode_response::<Text>(response(bytes), &validator, HttpLimits::default())), parser_error());
        }
    }

    #[test]
    fn inconsistent_validator_output_cannot_reach_typed_decode() {
        for fault in [Fault::Fingerprint, Fault::Payload, Fault::PayloadLength, Fault::DecodedLength] {
            let validator = Validator { fault, ..Validator::default() };
            let code = if matches!(fault, Fault::Fingerprint) { ErrorCode::SchemaImageInvalid }
                else { ErrorCode::InternalError };
            assert_local(decode_response::<Text>(response(RAW), &validator, HttpLimits::default()), code);
        }
        // This validator deliberately ignores the decoded cap so the HTTP root's
        // defensive check, rather than a fixture parser check, detects excess.
        let validator = Validator::default();
        let mut limits = HttpLimits::default();
        limits.syntax.max_decoded_bytes = 1;
        assert_local(validate_response(response(ESCAPED), &validator, FINGERPRINT, limits), ErrorCode::DecodedArenaOverflow);
        limits.syntax.max_decoded_bytes = 0;
        decode_response::<Text>(response(RAW), &validator, limits).unwrap();
        limits.syntax.max_decoded_bytes = 2;
        decode_response::<Text>(response(ESCAPED), &validator, limits).unwrap();
    }

    #[test]
    fn transport_failure_prevents_validator_invocation() {
        let validator = Validator::default();
        for case in 0..4 {
            let mut input = response(RAW);
            let code = match case {
                0 => { input.completion = BodyCompletion::Incomplete; ErrorCode::TransportIncomplete }
                1 => { input.status = 404; ErrorCode::ConstraintViolation }
                2 => { input.headers.clear(); ErrorCode::TypeMismatch }
                _ => { input.headers.push(header("content-length", "1")); ErrorCode::FramingError }
            };
            assert_local(decode_response::<Text>(input, &validator, HttpLimits::default()), code);
        }
        assert_eq!(validator.calls.load(Ordering::Relaxed), 0);
    }

    // CPU execution occurs only when the test driver calls run_cpu. Polling and
    // submitting do not execute the closure, and no runtime is needed.
    struct ManualTask<T> {
        job: Option<Box<dyn FnOnce() -> Result<T> + Send + 'static>>,
        output: Option<Result<T>>,
    }

    impl<T> Unpin for ManualTask<T> {}

    impl<T> ManualTask<T> {
        fn run_cpu(&mut self) {
            let job = self.job.take().expect("CPU job executes exactly once");
            self.output = Some(job());
        }
    }

    impl<T> Future for ManualTask<T> {
        type Output = Result<T>;

        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            match self.output.take() {
                Some(result) => Poll::Ready(result),
                None => Poll::Pending,
            }
        }
    }

    #[derive(Default)]
    struct ManualExecutor {
        submissions: AtomicUsize,
        rejection: Option<Error>,
    }

    impl CpuExecutor for ManualExecutor {
        type Task<T> = ManualTask<T> where T: Send + 'static;

        fn submit<T: Send + 'static>(
            &self,
            job: Box<dyn FnOnce() -> Result<T> + Send + 'static>,
        ) -> Result<Self::Task<T>> {
            self.submissions.fetch_add(1, Ordering::Relaxed);
            if let Some(failure) = &self.rejection {
                drop(job);
                return Err(failure.clone());
            }
            Ok(ManualTask { job: Some(job), output: None })
        }
    }

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn poll<T>(task: &mut ManualTask<T>) -> Poll<Result<T>> {
        let waker = Waker::from(Arc::new(NoopWake));
        Pin::new(task).poll(&mut Context::from_waker(&waker))
    }

    fn complete<T>(mut task: ManualTask<T>) -> Result<T> {
        assert!(poll(&mut task).is_pending());
        task.run_cpu();
        match poll(&mut task) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("CPU driver completed the closure"),
        }
    }

    fn reservation() -> ResourceRequest {
        ResourceRequest { jobs: 1, retained_bytes: 1024, scratch_bytes: 128 }
    }

    fn admission() -> Admission {
        Admission::new(AdmissionLimits {
            max_jobs: 1, max_retained_bytes: 1024, max_scratch_bytes: 128,
        }).unwrap()
    }

    fn assert_released(admission: &Admission) {
        let snapshot = admission.snapshot();
        assert_eq!(snapshot.jobs, 0);
        assert_eq!(snapshot.retained_bytes, 0);
        assert_eq!(snapshot.scratch_bytes, 0);
    }

    #[test]
    fn cpu_preflight_releases_lease_without_submission_or_validation() {
        for case in 0..7 {
            let executor = ManualExecutor::default();
            let validator = Arc::new(Validator::default());
            let admission = admission();
            let lease = admission.try_scratch(reservation()).unwrap();
            let mut input = response(RAW);
            let mut limits = HttpLimits::default();
            let code = match case {
                0 => { limits.max_body_bytes = 0; ErrorCode::ConstraintViolation }
                1 => { input.completion = BodyCompletion::Incomplete; ErrorCode::TransportIncomplete }
                2 => { input.status = 500; ErrorCode::ConstraintViolation }
                3 => { input.status = 204; ErrorCode::TypeMismatch }
                4 => { input.headers.clear(); ErrorCode::TypeMismatch }
                5 => { input.headers.push(header("transfer-encoding", "chunked")); ErrorCode::FramingError }
                _ => { limits.max_body_bytes = 1; ErrorCode::PayloadTooLarge }
            };
            assert_local(decode_on::<_, _, Text>(&executor, validator.clone(), input, limits, lease,
                CancellationToken::new()), code);
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
            assert_eq!(validator.calls.load(Ordering::Relaxed), 0);
            assert_released(&admission);
        }
    }

    #[test]
    fn cpu_decode_keeps_accounting_through_task_and_result_retention() {
        let executor = ManualExecutor::default();
        let validator = Arc::new(Validator::default());
        let admission = admission();
        let mut limits = HttpLimits { max_body_bytes: 40, ..HttpLimits::default() };
        limits.syntax.max_payload_bytes = 30;
        let input = response(ESCAPED);
        let original = input.body.clone();
        let mut task = decode_on::<_, _, Text>(&executor, validator.clone(), input, limits,
            admission.try_scratch(reservation()).unwrap(), CancellationToken::new()).unwrap();
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 1);
        assert_eq!(validator.calls.load(Ordering::Relaxed), 0);
        assert!(poll(&mut task).is_pending());
        assert_eq!(admission.snapshot().jobs, 1);
        task.run_cpu();
        assert_eq!(admission.snapshot().retained_bytes, 1024);
        assert_eq!(validator.calls.load(Ordering::Relaxed), 1);
        assert_eq!(validator.observed_payload_limit.load(Ordering::Relaxed), 30);
        let admitted = match poll(&mut task) {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => panic!("CPU driver has run"),
        };
        drop(task);
        assert_eq!(admitted.reservation(), reservation());
        assert_eq!(admission.snapshot().jobs, 1);
        let message = admitted.value();
        assert_eq!(message.raw_bytes().as_ptr(), original.as_slice().as_ptr());
        assert_eq!(message.raw_bytes(), ESCAPED);
        assert_eq!(message.resolve_str(message.values().0), Some("a\n"));
        drop(admitted);
        assert_released(&admission);
    }

    #[test]
    fn cpu_rejection_and_dropped_tasks_release_all_owned_captures() {
        for case in 0..3 {
            let rejection = Error::new(ErrorCode::BudgetExhausted, ErrorSource::Decode, 17,
                "host executor rejected").with_schema(9, 4);
            let executor = ManualExecutor {
                rejection: if case == 0 { Some(rejection.clone()) } else { None },
                ..ManualExecutor::default()
            };
            let validator = Arc::new(Validator::default());
            let weak_validator = Arc::downgrade(&validator);
            let owner: Arc<[u8]> = Arc::from(RAW);
            let weak_payload = Arc::downgrade(&owner);
            let mut input = response(RAW);
            input.body = Payload::Shared(owner);
            let admission = admission();
            let result = decode_on::<_, _, Text>(&executor, validator, input, HttpLimits::default(),
                admission.try_scratch(reservation()).unwrap(), CancellationToken::new());
            if case == 0 {
                assert_eq!(error(result), rejection);
            } else {
                let mut task = result.unwrap();
                assert!(weak_payload.upgrade().is_some());
                if case == 2 { task.run_cpu(); }
                assert_eq!(admission.snapshot().jobs, 1);
                drop(task);
            }
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 1);
            assert!(weak_validator.upgrade().is_none());
            assert!(weak_payload.upgrade().is_none());
            assert_released(&admission);
        }
    }

    #[test]
    fn cpu_cancellation_before_submission_before_work_and_during_validation() {
        for case in 0..3 {
            let executor = ManualExecutor::default();
            let cancellation = CancellationToken::new();
            let validator = Arc::new(Validator {
                cancel_during_validation: if case == 2 { Some(cancellation.clone()) } else { None },
                ..Validator::default()
            });
            let admission = admission();
            if case == 0 { cancellation.cancel(); }
            let result = decode_on::<_, _, Text>(&executor, validator.clone(), response(RAW),
                HttpLimits::default(), admission.try_scratch(reservation()).unwrap(), cancellation.clone());
            let failure = if case == 0 {
                error(result)
            } else {
                let task = result.unwrap();
                if case == 1 { cancellation.cancel(); }
                error(complete(task))
            };
            assert_eq!(failure, cancellation.check().unwrap_err());
            assert_eq!(executor.submissions.load(Ordering::Relaxed), usize::from(case != 0));
            assert_eq!(validator.calls.load(Ordering::Relaxed), usize::from(case == 2));
            assert_released(&admission);
        }
    }

    #[test]
    fn cpu_errors_preserve_metadata_and_release_admission() {
        for case in 0..5 {
            let executor = ManualExecutor::default();
            let validator = Arc::new(Validator {
                failure: if case == 0 { Some(parser_error()) } else { None },
                fault: match case {
                    1 => Fault::Fingerprint,
                    2 => Fault::Payload,
                    _ => Fault::None,
                },
                ..Validator::default()
            });
            let admission = admission();
            let mut limits = HttpLimits::default();
            if case == 4 { limits.syntax.max_decoded_bytes = 1; }
            let lease = admission.try_scratch(reservation()).unwrap();
            let failure = if case == 3 {
                error(complete(decode_on::<_, _, RejectDecode>(&executor, validator, response(RAW),
                    limits, lease, CancellationToken::new()).unwrap()))
            } else {
                let bytes = if case == 4 { ESCAPED } else { RAW };
                error(complete(decode_on::<_, _, Text>(&executor, validator, response(bytes),
                    limits, lease, CancellationToken::new()).unwrap()))
            };
            match case {
                0 => assert_eq!(failure, parser_error()),
                1 => assert_eq!(failure.code, ErrorCode::SchemaImageInvalid),
                2 => assert_eq!(failure.code, ErrorCode::InternalError),
                3 => assert_eq!(failure, decoder_error()),
                _ => assert_eq!(failure.code, ErrorCode::DecodedArenaOverflow),
            }
            assert_eq!(executor.submissions.load(Ordering::Relaxed), 1);
            assert_released(&admission);
        }
    }

    #[test]
    fn cpu_insufficient_retained_reservation_fails_before_executor_use() {
        let executor = ManualExecutor::default();
        let validator = Arc::new(Validator::default());
        let admission = admission();
        let mut request = reservation();
        request.retained_bytes = RAW.len() - 1;
        let failure = error(decode_on::<_, _, Text>(&executor, validator.clone(), response(RAW),
            HttpLimits::default(), admission.try_scratch(request).unwrap(), CancellationToken::new()));
        assert_eq!(failure.code, ErrorCode::BudgetExhausted);
        assert_eq!(executor.submissions.load(Ordering::Relaxed), 0);
        assert_eq!(validator.calls.load(Ordering::Relaxed), 0);
        assert_released(&admission);
    }
}
