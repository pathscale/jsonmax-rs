//! Deterministic JSON-RPC responses and bounded MCP result payloads.
//!
//! Nested encoders run once into owned, bounded storage before their JSON is
//! admitted to an envelope. Streaming errors can leave a prefix in the caller's
//! sink; owned helpers publish a payload only after the entire result succeeds.

use super::*;

/// Append exactly one response, without a newline or transport framing.
///
/// A nested encoder must produce exactly one complete JSON value. Its output is
/// validated before inclusion. On any error the caller must discard the bytes
/// appended to `sink`, including a possibly incomplete envelope prefix.
pub fn encode_response(response: &McpResponse<'_>, sink: &mut dyn ByteSink) -> Result<()> {
    encode_response_bounded(response, sink, u32::MAX)
}

/// Encode an independently owned response within the complete wire byte limit.
pub fn response_payload(response: &McpResponse<'_>, max_bytes: u32) -> Result<Payload> {
    let mut sink = BufferSink::new(max_bytes);
    encode_response_bounded(response, &mut sink, max_bytes)?;
    Ok(sink.into_payload())
}

/// Encode a negotiated initialization result after validating configuration.
pub fn initialize_result(
    server: &ServerInfo,
    protocol_version: &str,
    max_bytes: u32,
) -> Result<Payload> {
    validate_server(server, protocol_version)?;
    let mut sink = BufferSink::new(max_bytes);
    sink.write_bytes(b"{\"protocolVersion\":")?;
    write_json_string(protocol_version, &mut sink)?;
    sink.write_bytes(b",\"capabilities\":{")?;
    if server.capabilities.tools {
        sink.write_bytes(b"\"tools\":{\"listChanged\":")?;
        write_bool(server.capabilities.tools_list_changed, &mut sink)?;
        sink.write_byte(b'}')?;
    }
    sink.write_bytes(b"},\"serverInfo\":{\"name\":")?;
    write_json_string(&server.implementation.name, &mut sink)?;
    sink.write_bytes(b",\"version\":")?;
    write_json_string(&server.implementation.version, &mut sink)?;
    sink.write_byte(b'}')?;
    if let Some(instructions) = &server.instructions {
        sink.write_bytes(b",\"instructions\":")?;
        write_json_string(instructions, &mut sink)?;
    }
    sink.write_byte(b'}')?;
    Ok(sink.into_payload())
}

/// Encode tool discovery, validating manually constructed definitions as well.
///
/// The wire result preserves input order and schema bytes, and excludes the
/// internal endpoint code. All definitions are validated before output begins.
pub fn tools_list_result(tools: &[ToolDefinition], max_bytes: u32) -> Result<Payload> {
    for (index, tool) in tools.iter().enumerate() {
        let valid = tool.name.split_once('.').map(|(service, endpoint)| {
            valid_identifier(service) && valid_identifier(endpoint)
        }).unwrap_or(false);
        if !valid {
            return Err(local_error(ErrorCode::ConstraintViolation, "invalid tool name"));
        }
        if tools[..index].iter().any(|previous| previous.name == tool.name) {
            return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate tool name"));
        }
        if tool.input_schema.len() > max_bytes as usize {
            return Err(local_error(ErrorCode::PayloadTooLarge, "tool schema exceeds output limit"));
        }
        validate_value(&tool.input_schema, true)?;
    }

    let mut sink = BufferSink::new(max_bytes);
    sink.write_bytes(b"{\"tools\":[")?;
    for (index, tool) in tools.iter().enumerate() {
        if index != 0 {
            sink.write_byte(b',')?;
        }
        sink.write_bytes(b"{\"name\":")?;
        write_json_string(&tool.name, &mut sink)?;
        sink.write_bytes(b",\"description\":")?;
        write_json_string(&tool.description, &mut sink)?;
        sink.write_bytes(b",\"inputSchema\":")?;
        sink.write_bytes(tool.input_schema.as_slice())?;
        sink.write_byte(b'}')?;
    }
    sink.write_bytes(b"]}")?;
    Ok(sink.into_payload())
}

/// Encode text and optional structured content as an MCP tool execution result.
///
/// `is_error` records the execution outcome inside a successful RPC response.
/// Encoder and validation failures remain ABI errors, never fabricated content.
pub fn tool_result_payload(result: &ToolResult<'_>, max_bytes: u32) -> Result<Payload> {
    let structured = match result.structured_content {
        Some(value) => Some(encoded_value(value, max_bytes, true)?),
        None => None,
    };
    let mut sink = BufferSink::new(max_bytes);
    sink.write_bytes(b"{\"content\":[")?;
    for (index, content) in result.content.iter().enumerate() {
        if index != 0 {
            sink.write_byte(b',')?;
        }
        sink.write_bytes(b"{\"type\":\"text\",\"text\":")?;
        write_json_string(content.text, &mut sink)?;
        sink.write_byte(b'}')?;
    }
    sink.write_byte(b']')?;
    if let Some(structured) = structured {
        sink.write_bytes(b",\"structuredContent\":")?;
        sink.write_bytes(structured.as_slice())?;
    }
    sink.write_bytes(b",\"isError\":")?;
    write_bool(result.is_error, &mut sink)?;
    sink.write_byte(b'}')?;
    Ok(sink.into_payload())
}

impl JsonEncode for McpResponse<'_> {
    fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
        encode_response(self, sink)
    }
}

fn encode_response_bounded(
    response: &McpResponse<'_>,
    sink: &mut dyn ByteSink,
    nested_limit: u32,
) -> Result<()> {
    sink.write_bytes(b"{\"jsonrpc\":\"2.0\",\"id\":")?;
    match response {
        McpResponse::Success { id, result } => {
            write_id(id, sink)?;
            sink.write_bytes(b",\"result\":")?;
            let payload = encoded_value(*result, nested_limit, false)?;
            sink.write_bytes(payload.as_slice())?;
        }
        McpResponse::Error { id, code, message, data } => {
            write_id(id, sink)?;
            sink.write_bytes(b",\"error\":{\"code\":")?;
            write_integer(i64::from(*code), sink)?;
            sink.write_bytes(b",\"message\":")?;
            write_json_string(message, sink)?;
            if let Some(data) = data {
                sink.write_bytes(b",\"data\":")?;
                let payload = encoded_value(*data, nested_limit, false)?;
                sink.write_bytes(payload.as_slice())?;
            }
            sink.write_byte(b'}')?;
        }
    }
    sink.write_byte(b'}')
}

fn encoded_value(value: &dyn JsonEncode, max_bytes: u32, object: bool) -> Result<Payload> {
    let mut sink = BufferSink::new(max_bytes);
    value.encode_json(&mut sink)?;
    let payload = sink.into_payload();
    validate_value(&payload, object)?;
    Ok(payload)
}

fn validate_value(payload: &Payload, object: bool) -> Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| {
        local_error(ErrorCode::PayloadTooLarge, "JSON value exceeds ABI byte limit")
    })?;
    // Output size already bounds decoded bytes and fields. Do not silently
    // impose default request limits on server-generated schemas or results.
    let limits = EnvelopeLimits {
        max_payload_bytes: length.max(1),
        max_decoded_bytes: length.max(1),
        max_depth: u16::MAX,
        max_fields: length.max(1),
    };
    envelope::validate_json(payload, limits)?;
    if object {
        let start = payload.as_slice().iter().position(|byte| {
            !matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
        }).unwrap_or(payload.len());
        if payload.as_slice().get(start) != Some(&b'{') {
            return Err(Error::new(
                ErrorCode::TypeMismatch, ErrorSource::Wire, start as u32,
                "expected JSON object",
            ));
        }
    }
    Ok(())
}

fn write_id(id: &McpId, sink: &mut dyn ByteSink) -> Result<()> {
    match id {
        McpId::Null => sink.write_bytes(b"null"),
        McpId::Number(value) => write_integer(*value, sink),
        McpId::String(value) => write_json_string(value, sink),
    }
}

fn write_integer(value: i64, sink: &mut dyn ByteSink) -> Result<()> {
    // Twenty bytes hold the sign and all nineteen digits of i64::MIN.
    let mut bytes = [0u8; 20];
    let mut start = bytes.len();
    let mut magnitude = value.unsigned_abs();
    loop {
        start -= 1;
        bytes[start] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        start -= 1;
        bytes[start] = b'-';
    }
    sink.write_bytes(&bytes[start..])
}

fn write_bool(value: bool, sink: &mut dyn ByteSink) -> Result<()> {
    sink.write_bytes(if value { b"true" } else { b"false" })
}

fn valid_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_server(server: &ServerInfo, selected: &str) -> Result<()> {
    if server.implementation.name.is_empty() || server.implementation.version.is_empty()
        || server.protocol_versions.is_empty()
        || (server.capabilities.tools_list_changed && !server.capabilities.tools)
    {
        return Err(local_error(ErrorCode::ConstraintViolation, "invalid server configuration"));
    }
    for (index, version) in server.protocol_versions.iter().enumerate() {
        if version.is_empty() {
            return Err(local_error(ErrorCode::ConstraintViolation, "empty protocol version"));
        }
        if server.protocol_versions[..index].contains(version) {
            return Err(local_error(ErrorCode::DuplicateObjectKey, "duplicate protocol version"));
        }
    }
    if !server.protocol_versions.iter().any(|version| version == selected) {
        return Err(local_error(ErrorCode::ConstraintViolation, "unconfigured protocol version"));
    }
    Ok(())
}

fn local_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Wire, 0, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use core::cell::Cell;

    struct Raw<'a>(&'a [u8]);

    impl JsonEncode for Raw<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(self.0)
        }
    }

    struct Encoder<'a> {
        bytes: &'a [u8],
        calls: Cell<usize>,
        error: Option<Error>,
    }

    impl JsonEncode for Encoder<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            sink.write_bytes(self.bytes)?;
            match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    fn server() -> ServerInfo {
        ServerInfo {
            implementation: ImplementationInfo {
                name: String::from("jsonmax"), version: String::from("1.0"),
            },
            protocol_versions: vec![String::from(DEFAULT_PROTOCOL_VERSION), String::from("custom")],
            capabilities: ServerCapabilities { tools: true, tools_list_changed: false },
            instructions: None,
        }
    }

    fn tool(name: &str, schema: &[u8]) -> ToolDefinition {
        ToolDefinition {
            name: String::from(name), description: String::from("description"),
            endpoint_code: u32::MAX, input_schema: Payload::from_slice(schema),
        }
    }

    fn assert_local(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Wire);
    }

    fn assert_json(payload: &Payload) {
        validate_value(payload, false).unwrap();
    }

    fn assert_budget<F>(expected: &[u8], encode: F)
    where F: Fn(u32) -> Result<Payload> {
        let exact = expected.len() as u32;
        assert_eq!(encode(exact).unwrap().as_slice(), expected);
        assert_eq!(encode(exact + 1).unwrap().as_slice(), expected);
        for limit in [0, 1, exact - 1] {
            assert_local(encode(limit).unwrap_err(), ErrorCode::PayloadTooLarge);
        }
        assert_json(&encode(exact).unwrap());
    }

    #[test]
    fn success_envelopes_preserve_exact_integer_and_id_identity() {
        let result = Raw(b"{}");
        for (id, wire) in [
            (McpId::Null, "null"),
            (McpId::Number(0), "0"),
            (McpId::Number(-1), "-1"),
            (McpId::Number(i64::MIN), "-9223372036854775808"),
            (McpId::Number(i64::MAX), "9223372036854775807"),
            (McpId::Number(9_007_199_254_740_993), "9007199254740993"),
            (McpId::String(String::from("001")), "\"001\""),
            (McpId::String(String::new()), "\"\""),
        ] {
            let response = McpResponse::Success { id: &id, result: &result };
            let expected = format!("{{\"jsonrpc\":\"2.0\",\"id\":{wire},\"result\":{{}}}}");
            assert_budget(expected.as_bytes(), |limit| response_payload(&response, limit));
            let mut streamed = BufferSink::new(u32::MAX);
            encode_response(&response, &mut streamed).unwrap();
            assert_eq!(streamed.as_bytes(), expected.as_bytes());
            let mut via_trait = BufferSink::new(u32::MAX);
            response.encode_json(&mut via_trait).unwrap();
            assert_eq!(via_trait.as_bytes(), streamed.as_bytes());
        }
    }

    #[test]
    fn strings_escape_quotes_backslashes_controls_and_preserve_unicode() {
        let id = McpId::String(String::from("\"\\\n\r\t\u{8}\u{c}\0é🚀"));
        let response = McpResponse::Error {
            id: &id, code: INVALID_PARAMS, message: "bad\"\\\n\0界", data: None,
        };
        let expected = "{\"jsonrpc\":\"2.0\",\"id\":\"\\\"\\\\\\n\\r\\t\\b\\f\\u0000é🚀\",\"error\":{\"code\":-32602,\"message\":\"bad\\\"\\\\\\n\\u0000界\"}}";
        assert_budget(expected.as_bytes(), |limit| response_payload(&response, limit));
        let controls: String = (0u8..32).map(char::from).collect();
        let id = McpId::String(controls.clone());
        let payload = response_payload(&McpResponse::Error {
            id: &id, code: 0, message: &controls, data: None,
        }, 4096).unwrap();
        let decoded = envelope::object_field(&payload, "id", EnvelopeLimits::default()).unwrap().unwrap();
        assert_eq!(envelope::decode_string(&decoded, EnvelopeLimits::default()).unwrap(), controls);
    }

    #[test]
    fn error_code_extremes_and_optional_data_have_exact_wire_order() {
        let id = McpId::Null;
        let data = Raw(b"[false,null,7]");
        for code in [i32::MIN, PARSE_ERROR, INTERNAL_ERROR, 0, i32::MAX] {
            let response = McpResponse::Error { id: &id, code, message: "failure", data: Some(&data) };
            let expected = format!("{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":{code},\"message\":\"failure\",\"data\":[false,null,7]}}}}");
            assert_budget(expected.as_bytes(), |limit| response_payload(&response, limit));
        }
        let response = McpResponse::Error { id: &id, code: -1, message: "", data: None };
        assert_eq!(response_payload(&response, 256).unwrap().as_slice(),
            b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-1,\"message\":\"\"}}");
        let null = Raw(b"null");
        let response = McpResponse::Error { id: &id, code: -1, message: "", data: Some(&null) };
        assert_eq!(response_payload(&response, 256).unwrap().as_slice(),
            b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-1,\"message\":\"\",\"data\":null}}");
    }

    #[test]
    fn every_json_kind_and_surrounding_whitespace_is_valid_nested_data() {
        for bytes in [&b"null"[..], b"true", b"false", b"-0", b"1.25e+3", b"\"text\"", b"[]", b"{}", b" \n{\"x\":[1]}\t"] {
            let raw = Raw(bytes);
            let id = McpId::Number(1);
            let success = response_payload(&McpResponse::Success { id: &id, result: &raw }, 1024).unwrap();
            let error = response_payload(&McpResponse::Error { id: &id, code: -1, message: "x", data: Some(&raw) }, 1024).unwrap();
            assert_json(&success);
            assert_json(&error);
            assert!(success.as_slice().windows(bytes.len()).any(|window| window == bytes));
        }
    }

    #[test]
    fn malformed_nested_values_are_rejected_for_results_data_and_structured_content() {
        for (bytes, code, offset) in [
            (&b""[..], ErrorCode::UnexpectedEof, 0),
            (b"{", ErrorCode::UnexpectedEof, 1),
            (b"{} false", ErrorCode::TrailingContent, 3),
            (br#"{"a":1,}"#, ErrorCode::InvalidSyntax, 7),
            (br#"{"a":01}"#, ErrorCode::InvalidNumber, 6),
            (br#"{"a":"\q"}"#, ErrorCode::InvalidEscape, 7),
            (br#"{"a":"\uD800"}"#, ErrorCode::LoneSurrogate, 6),
            (b"{\"a\":\"\n\"}", ErrorCode::ControlCharInString, 6),
            (b"{\"a\":\"\xff\"}", ErrorCode::InvalidUtf8, 6),
        ] {
            let raw = Raw(bytes);
            let id = McpId::Null;
            let errors = [
                response_payload(&McpResponse::Success { id: &id, result: &raw }, 1024).unwrap_err(),
                response_payload(&McpResponse::Error { id: &id, code: -1, message: "x", data: Some(&raw) }, 1024).unwrap_err(),
                tool_result_payload(&ToolResult { content: &[], structured_content: Some(&raw), is_error: false }, 1024).unwrap_err(),
                tools_list_result(&[tool("s.e", bytes)], 1024).unwrap_err(),
            ];
            for error in errors {
                assert_eq!(error.byte_offset, offset, "input: {bytes:?}");
                assert_local(error, code);
            }
        }
    }

    #[test]
    fn duplicate_decoded_keys_are_rejected_at_every_depth() {
        for bytes in [
            &br#"{"x":1,"x":2}"#[..],
            br#"{"x":1,"\u0078":2}"#,
            br#"{"a":[{"x":1,"x":2}]}"#,
            br#"{"a":{"x":1,"\u0078":2}}"#,
        ] {
            let raw = Raw(bytes);
            let id = McpId::Null;
            assert_local(response_payload(&McpResponse::Success { id: &id, result: &raw }, 1024).unwrap_err(), ErrorCode::DuplicateObjectKey);
            assert_local(response_payload(&McpResponse::Error { id: &id, code: 1, message: "", data: Some(&raw) }, 1024).unwrap_err(), ErrorCode::DuplicateObjectKey);
            assert_local(tool_result_payload(&ToolResult { content: &[], structured_content: Some(&raw), is_error: true }, 1024).unwrap_err(), ErrorCode::DuplicateObjectKey);
            assert_local(tools_list_result(&[tool("s.e", bytes)], 1024).unwrap_err(), ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn encoder_errors_keep_all_metadata_and_are_never_successful_payloads() {
        let expected = Error::new(ErrorCode::Cancelled, ErrorSource::Encode, 19, "execution cancelled").with_schema(7, 11);
        let encoder = Encoder { bytes: b"{}", calls: Cell::new(0), error: Some(expected.clone()) };
        let id = McpId::Null;
        assert_eq!(response_payload(&McpResponse::Success { id: &id, result: &encoder }, 1024).unwrap_err(), expected);
        assert_eq!(encoder.calls.get(), 1);
        assert_eq!(response_payload(&McpResponse::Error { id: &id, code: -1, message: "", data: Some(&encoder) }, 1024).unwrap_err(), expected);
        assert_eq!(encoder.calls.get(), 2);
        assert_eq!(tool_result_payload(&ToolResult { content: &[], structured_content: Some(&encoder), is_error: true }, 1024).unwrap_err(), expected);
        assert_eq!(encoder.calls.get(), 3);
        // A partial value followed by an encoder error preserves that error.
        let encoder = Encoder { bytes: b"{", calls: Cell::new(0), error: Some(expected.clone()) };
        assert_eq!(encoded_value(&encoder, 10, true).unwrap_err(), expected);
    }

    #[test]
    fn temporary_sinks_bound_encoder_writes_and_reservations() {
        let encoder = Encoder { bytes: b"{\"long\":true}", calls: Cell::new(0), error: None };
        assert_local(encoded_value(&encoder, 2, true).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(encoder.calls.get(), 1);
        struct Reserving;
        impl JsonEncode for Reserving {
            fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
                sink.reserve(usize::MAX)?;
                sink.write_bytes(b"{}")
            }
        }
        assert_local(encoded_value(&Reserving, 16, true).unwrap_err(), ErrorCode::PayloadTooLarge);
        let id = McpId::Null;
        assert_local(response_payload(&McpResponse::Success { id: &id, result: &encoder }, 2).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(encoder.calls.get(), 1);
    }

    struct FailingSink {
        bytes: Vec<u8>,
        limit: usize,
        error: Error,
    }

    impl ByteSink for FailingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            if additional > self.limit.saturating_sub(self.bytes.len()) {
                return Err(self.error.clone());
            }
            ByteSink::reserve(&mut self.bytes, additional)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.reserve(bytes.len())?;
            ByteSink::write_bytes(&mut self.bytes, bytes)
        }

        fn len(&self) -> usize { self.bytes.len() }
    }

    #[test]
    fn streaming_failures_propagate_at_every_output_boundary() {
        let id = McpId::String(String::from("a\n\"界"));
        let raw = Raw(br#"{"ok":true}"#);
        let expected_error = Error::new(ErrorCode::TransportIncomplete, ErrorSource::Transport, 31, "sink stopped").with_schema(2, 3);
        for response in [
            McpResponse::Success { id: &id, result: &raw },
            McpResponse::Error { id: &id, code: i32::MIN, message: "bad\n\0", data: Some(&raw) },
            McpResponse::Error { id: &id, code: i32::MAX, message: "", data: None },
        ] {
            let expected = response_payload(&response, 1024).unwrap();
            for limit in 0..expected.len() {
                let mut sink = FailingSink { bytes: Vec::new(), limit, error: expected_error.clone() };
                assert_eq!(encode_response(&response, &mut sink).unwrap_err(), expected_error);
                assert!(sink.bytes.len() <= limit);
                assert!(expected.as_slice().starts_with(&sink.bytes));
            }
            let mut sink = FailingSink { bytes: Vec::new(), limit: expected.len(), error: expected_error.clone() };
            encode_response(&response, &mut sink).unwrap();
            assert_eq!(sink.bytes, expected.as_slice());
        }
    }

    #[test]
    fn streaming_appends_to_existing_prefix_and_nested_failure_leaves_only_prefix() {
        let id = McpId::Number(7);
        let raw = Raw(b"null");
        let response = McpResponse::Success { id: &id, result: &raw };
        let mut sink = BufferSink::new(1024);
        sink.write_bytes(b"prefix:").unwrap();
        encode_response(&response, &mut sink).unwrap();
        assert_eq!(sink.as_bytes(), b"prefix:{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":null}");
        let raw = Raw(b"{} {}");
        let mut sink = BufferSink::new(1024);
        assert_local(encode_response(&McpResponse::Success { id: &id, result: &raw }, &mut sink).unwrap_err(), ErrorCode::TrailingContent);
        assert_eq!(sink.as_bytes(), b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":");
    }

    #[test]
    fn initialization_vectors_cover_capabilities_versions_and_instructions() {
        let mut config = server();
        let expected = b"{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":{\"name\":\"jsonmax\",\"version\":\"1.0\"}}";
        assert_budget(expected, |limit| initialize_result(&config, DEFAULT_PROTOCOL_VERSION, limit));
        config.capabilities.tools_list_changed = true;
        config.instructions = Some(String::from(""));
        assert_eq!(initialize_result(&config, "custom", 1024).unwrap().as_slice(),
            b"{\"protocolVersion\":\"custom\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"jsonmax\",\"version\":\"1.0\"},\"instructions\":\"\"}");
        config.capabilities = ServerCapabilities::default();
        config.instructions = None;
        let expected = b"{\"protocolVersion\":\"custom\",\"capabilities\":{},\"serverInfo\":{\"name\":\"jsonmax\",\"version\":\"1.0\"}}";
        assert_budget(expected, |limit| initialize_result(&config, "custom", limit));
    }

    #[test]
    fn initialization_escapes_all_configured_strings() {
        let mut config = server();
        config.implementation.name = String::from("s\"\n界");
        config.implementation.version = String::from("v\\\0");
        config.protocol_versions = vec![String::from("p\"\t")];
        config.instructions = Some(String::from("read\r🚀"));
        let expected = "{\"protocolVersion\":\"p\\\"\\t\",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":{\"name\":\"s\\\"\\n界\",\"version\":\"v\\\\\\u0000\"},\"instructions\":\"read\\r🚀\"}";
        assert_budget(expected.as_bytes(), |limit| initialize_result(&config, "p\"\t", limit));
    }

    #[test]
    fn invalid_server_configuration_is_rejected_before_output_budget() {
        let base = server();
        let mut variants = Vec::new();
        let mut invalid = base.clone();
        invalid.implementation.name.clear();
        variants.push((invalid, ErrorCode::ConstraintViolation));
        let mut invalid = base.clone();
        invalid.implementation.version.clear();
        variants.push((invalid, ErrorCode::ConstraintViolation));
        let mut invalid = base.clone();
        invalid.protocol_versions.clear();
        variants.push((invalid, ErrorCode::ConstraintViolation));
        let mut invalid = base.clone();
        invalid.protocol_versions.push(String::new());
        variants.push((invalid, ErrorCode::ConstraintViolation));
        let mut invalid = base.clone();
        invalid.protocol_versions.push(String::from(DEFAULT_PROTOCOL_VERSION));
        variants.push((invalid, ErrorCode::DuplicateObjectKey));
        let mut invalid = base.clone();
        invalid.capabilities = ServerCapabilities { tools: false, tools_list_changed: true };
        variants.push((invalid, ErrorCode::ConstraintViolation));
        for (config, code) in variants {
            for limit in [0, 1024] {
                assert_local(initialize_result(&config, DEFAULT_PROTOCOL_VERSION, limit).unwrap_err(), code);
            }
        }
        for selected in ["", "2025-03-26", "CUSTOM", "custom "] {
            assert_local(initialize_result(&base, selected, 1024).unwrap_err(), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn tool_list_vectors_preserve_order_schemas_and_metadata() {
        let mut tools = vec![tool("Zoo.z", b" \n{\"type\":\"object\"}\t"), tool("Alpha.a", b"{}")];
        tools[0].description = String::from("é\n\"\\\0");
        tools[0].endpoint_code = 0;
        tools[1].description.clear();
        let expected = "{\"tools\":[{\"name\":\"Zoo.z\",\"description\":\"é\\n\\\"\\\\\\u0000\",\"inputSchema\": \n{\"type\":\"object\"}\t},{\"name\":\"Alpha.a\",\"description\":\"\",\"inputSchema\":{}}]}";
        assert_budget(expected.as_bytes(), |limit| tools_list_result(&tools, limit));
        assert_budget(b"{\"tools\":[]}", |limit| tools_list_result(&[], limit));
        // Endpoint codes are host-only metadata, including repeated codes here.
        tools[1].endpoint_code = 0;
        assert_eq!(tools_list_result(&tools, 1024).unwrap().as_slice(), expected.as_bytes());
    }

    #[test]
    fn tool_names_match_frozen_root_convention_without_normalization() {
        for name in ["", "s", ".e", "s.", "s.e.x", "s..e", "0s.e", "s.0e", "s-e.x", "s.e-x", " s.e", "s.e ", "é.e", "s.界", "s.e\0"] {
            assert_local(tools_list_result(&[tool(name, b"{}")], 1024).unwrap_err(), ErrorCode::ConstraintViolation);
        }
        for service in ["_", "A", "z", "_09AZaz"] {
            for endpoint in ["_", "A", "e9", "_09AZaz"] {
                let name = tool_name(service, endpoint).unwrap();
                assert_json(&tools_list_result(&[tool(&name, b"{}")], 1024).unwrap());
            }
        }
        let tools = [tool("s.e", b"{}"), tool("s.e", b"{}")];
        assert_local(tools_list_result(&tools, 1024).unwrap_err(), ErrorCode::DuplicateObjectKey);
        assert!(tools_list_result(&[tool("s.e", b"{}"), tool("s.E", b"{}")], 1024).is_ok());
    }

    #[test]
    fn schemas_and_structured_content_require_an_object() {
        for (raw, offset) in [
            (&b"null"[..], 0), (b"true", 0), (b"1", 0), (b"\"object\"", 0),
            (b"[]", 0), (b"[{}]", 0), (b" \t\r\n[]", 4),
        ] {
            let encoder = Raw(raw);
            for error in [
                tools_list_result(&[tool("s.e", raw)], 1024).unwrap_err(),
                tool_result_payload(&ToolResult { content: &[], structured_content: Some(&encoder), is_error: false }, 1024).unwrap_err(),
            ] {
                assert_eq!(error.byte_offset, offset);
                assert_local(error, ErrorCode::TypeMismatch);
            }
        }
    }

    #[test]
    fn later_invalid_tool_definition_discards_all_owned_output() {
        let tools = [tool("s.e", b"{}"), tool("s.f", b"{\"x\":1,\"x\":2}")];
        assert_local(tools_list_result(&tools, 4096).unwrap_err(), ErrorCode::DuplicateObjectKey);
        let tools = [tool("s.e", b"{}"), tool("bad", b"{}")];
        assert_local(tools_list_result(&tools, 0).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_local(tools_list_result(&tools, 4096).unwrap_err(), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn text_only_and_empty_execution_results_have_exact_wire_shapes() {
        assert_budget(b"{\"content\":[],\"isError\":false}", |limit| tool_result_payload(
            &ToolResult { content: &[], structured_content: None, is_error: false }, limit));
        assert_budget(b"{\"content\":[],\"isError\":true}", |limit| tool_result_payload(
            &ToolResult { content: &[], structured_content: None, is_error: true }, limit));
        let content = [TextContent { text: "" }, TextContent { text: "é\n\0\"\\🚀" }];
        let expected = "{\"content\":[{\"type\":\"text\",\"text\":\"\"},{\"type\":\"text\",\"text\":\"é\\n\\u0000\\\"\\\\🚀\"}],\"isError\":true}";
        assert_budget(expected.as_bytes(), |limit| tool_result_payload(
            &ToolResult { content: &content, structured_content: None, is_error: true }, limit));
    }

    #[test]
    fn structured_execution_output_is_encoded_once_before_inclusion() {
        let encoder = Encoder { bytes: b" \n{\"answer\":42}\t", calls: Cell::new(0), error: None };
        let content = [TextContent { text: "42" }];
        let result = ToolResult { content: &content, structured_content: Some(&encoder), is_error: false };
        let expected = b"{\"content\":[{\"type\":\"text\",\"text\":\"42\"}],\"structuredContent\": \n{\"answer\":42}\t,\"isError\":false}";
        let payload = tool_result_payload(&result, expected.len() as u32).unwrap();
        assert_eq!(payload.as_slice(), expected);
        assert_eq!(encoder.calls.get(), 1);
        assert_json(&payload);
        assert_local(tool_result_payload(&result, expected.len() as u32 - 1).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(encoder.calls.get(), 2);
        let empty = Raw(b"{}");
        assert_budget(b"{\"content\":[],\"structuredContent\":{},\"isError\":true}", |limit| tool_result_payload(
            &ToolResult { content: &[], structured_content: Some(&empty), is_error: true }, limit));
    }

    #[test]
    fn response_encoders_are_invoked_once_and_owned_results_outlive_inputs() {
        let payload = {
            let id = McpId::String(String::from("request"));
            let encoder = Encoder { bytes: b"{\"owned\":true}", calls: Cell::new(0), error: None };
            let payload = response_payload(&McpResponse::Success { id: &id, result: &encoder }, 1024).unwrap();
            assert_eq!(encoder.calls.get(), 1);
            payload
        };
        assert_eq!(payload.as_slice(), b"{\"jsonrpc\":\"2.0\",\"id\":\"request\",\"result\":{\"owned\":true}}");
        let payload = {
            let tools = [tool("s.e", b"{}")];
            tools_list_result(&tools, 1024).unwrap()
        };
        assert_json(&payload);
        let payload = {
            let text = String::from("owned text");
            let content = [TextContent { text: &text }];
            tool_result_payload(&ToolResult { content: &content, structured_content: None, is_error: false }, 1024).unwrap()
        };
        assert_eq!(payload.as_slice(), b"{\"content\":[{\"type\":\"text\",\"text\":\"owned text\"}],\"isError\":false}");
    }

    #[test]
    fn output_validation_uses_explicit_byte_budget_instead_of_request_defaults() {
        let depth = usize::from(EnvelopeLimits::default().max_depth) + 1;
        let raw = format!("{{\"x\":{}0{}}}", "[".repeat(depth), "]".repeat(depth));
        let encoder = Raw(raw.as_bytes());
        assert!(tool_result_payload(&ToolResult { content: &[], structured_content: Some(&encoder), is_error: false }, (raw.len() + 128) as u32).is_ok());
        let mut raw = String::from("{");
        for index in 0..=EnvelopeLimits::default().max_fields {
            if index != 0 { raw.push(','); }
            raw.push_str(&format!("\"k{index}\":null"));
        }
        raw.push('}');
        assert!(tools_list_result(&[tool("s.e", raw.as_bytes())], (raw.len() + 128) as u32).is_ok());
    }

    #[test]
    fn full_abi_depth_boundary_is_enforced_for_raw_values() {
        for (depth, succeeds) in [(usize::from(u16::MAX), true), (usize::from(u16::MAX) + 1, false)] {
            let raw = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            let payload = Payload::from_slice(raw.as_bytes());
            let result = validate_value(&payload, false);
            if succeeds {
                result.unwrap();
            } else {
                assert_local(result.unwrap_err(), ErrorCode::DepthLimitExceeded);
            }
        }
    }

    #[test]
    fn initialization_and_execution_results_integrate_with_response_envelopes() {
        let config = server();
        let request = Payload::from_slice(br#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"client","version":"1"}}}"#);
        let message = envelope::decode_message(request, EnvelopeLimits::default()).unwrap();
        let id = message.id.unwrap();
        let result = initialize_result(&config, DEFAULT_PROTOCOL_VERSION, 1024).unwrap();
        let raw = Raw(result.as_slice());
        let response = response_payload(&McpResponse::Success { id: &id, result: &raw }, 2048).unwrap();
        assert_json(&response);
        assert_eq!(envelope::object_field(&response, "result", EnvelopeLimits::default()).unwrap().unwrap().as_slice(), result.as_slice());
        let tools = [tool("s.e", br#"{"type":"object","additionalProperties":false}"#)];
        let listing = tools_list_result(&tools, 1024).unwrap();
        let raw = Raw(listing.as_slice());
        let response = response_payload(&McpResponse::Success { id: &id, result: &raw }, 2048).unwrap();
        assert_json(&response);
        assert_eq!(envelope::object_field(&response, "result", EnvelopeLimits::default()).unwrap().unwrap().as_slice(), listing.as_slice());
        let structured = Raw(br#"{"reason":"not available"}"#);
        let content = [TextContent { text: "execution failed" }];
        let execution = tool_result_payload(&ToolResult { content: &content, structured_content: Some(&structured), is_error: true }, 1024).unwrap();
        let raw = Raw(execution.as_slice());
        let response = response_payload(&McpResponse::Success { id: &id, result: &raw }, 2048).unwrap();
        assert!(envelope::object_field(&response, "error", EnvelopeLimits::default()).unwrap().is_none());
        let execution = envelope::object_field(&response, "result", EnvelopeLimits::default()).unwrap().unwrap();
        assert_eq!(envelope::object_field(&execution, "isError", EnvelopeLimits::default()).unwrap().unwrap().as_slice(), b"true");
        let failure = failure(None, PARSE_ERROR, "Invalid JSON");
        let response = response_payload(&McpResponse::Error { id: &failure.id, code: failure.code, message: &failure.message, data: None }, 1024).unwrap();
        assert_eq!(response.as_slice(), b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Invalid JSON\"}}");
    }
}
