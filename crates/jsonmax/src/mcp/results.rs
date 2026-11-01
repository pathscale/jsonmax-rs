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
