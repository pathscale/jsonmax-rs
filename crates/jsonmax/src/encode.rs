//! Typed JSON output with bounded staging and complete syntax validation.

use alloc::{string::String, vec::Vec};

use super::{
    facade_error, ByteSink, Document, EncodeOptions, Error, ErrorCode, ErrorSource,
    JsonEncode, OutputBuffer, Result,
};

/// Encodes one typed value into an owned vector after validating the complete
/// output as a single JSON value.
pub fn to_vec<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut staged = OutputBuffer::new(options.max_output_bytes);
    value.encode_json(&mut staged)?;
    validate_encoded(staged.as_bytes())?;
    Ok(staged.into_vec())
}

/// Encodes one typed value and converts its validated UTF-8 bytes to a string.
pub fn to_string<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<String> {
    let bytes = to_vec(value, options)?;
    String::from_utf8(bytes).map_err(|error| {
        let offset = u32::try_from(error.utf8_error().valid_up_to()).unwrap_or(u32::MAX);
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Encode,
            offset,
            "validated encoder output was not UTF-8",
        )
    })
}

/// Stages and validates a complete value before making the first destination
/// call. Destination errors are propagated as supplied by that sink.
pub fn to_sink<T: JsonEncode + ?Sized>(
    value: &T,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    let bytes = to_vec(value, options)?;
    sink.reserve(bytes.len())?;
    sink.write_bytes(&bytes)
}

/// Revalidates a caller-supplied ABI document, then emits its values through
/// the native syntax-aware writer. The supplied tables must match a fresh
/// parse of the retained payload exactly.
pub fn document_to_vec(document: &Document, options: EncodeOptions) -> Result<Vec<u8>> {
    if document.proof.payload_len() as usize != document.payload.len()
        || document.proof.decoded_len() as usize != document.parts.decoded.len()
    {
        return Err(document_error("document proof lengths do not match its buffers"));
    }

    let rebuilt = jmax_dom::build::parse(
        document.payload.clone(),
        super::Limits::permissive_default(),
    )
    .map_err(|error| match error.code {
        ErrorCode::BudgetExhausted => Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Encode,
            error.byte_offset,
            "document validation allocation failed",
        ),
        ErrorCode::PayloadTooLarge => Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Encode,
            error.byte_offset,
            "document exceeds the native addressable length",
        ),
        _ => document_error("document payload is not a complete valid JSON value"),
    })?;
    if rebuilt.parts().nodes != document.parts.nodes
        || rebuilt.parts().spans != document.parts.spans
        || rebuilt.parts().containers != document.parts.containers
        || rebuilt.parts().numbers != document.parts.numbers
        || rebuilt.parts().decoded != document.parts.decoded
    {
        return Err(document_error("document tables do not match its payload"));
    }

    let mut output = OutputBuffer::new(options.max_output_bytes);
    let limits = writer_limits(options.max_output_bytes);
    {
        let mut writer = jmax_encode::writer::JsonWriter::new(&mut output, limits);
        encode_dom_node(&rebuilt, jmax_dom::navigate::root(&rebuilt), &mut writer, 0)?;
        writer.finish()?;
    }
    validate_encoded(output.as_bytes())?;
    Ok(output.into_vec())
}

/// Emits one quoted JSON string using the native writer's canonical escaping.
pub fn escape_string(value: &str, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = OutputBuffer::new(options.max_output_bytes);
    let limits = writer_limits(options.max_output_bytes);
    {
        let mut writer = jmax_encode::writer::JsonWriter::new(&mut output, limits);
        writer.string(value)?;
        writer.finish()?;
    }
    validate_encoded(output.as_bytes())?;
    Ok(output.into_vec())
}

fn validate_encoded(bytes: &[u8]) -> Result<()> {
    if u32::try_from(bytes.len()).is_err() {
        return Err(facade_error(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Encode,
            "encoded value exceeds the 32-bit validator length limit",
        ));
    }
    let mut cursor = jmax_cursor::Cursor::new(
        super::Limits::permissive_default(),
        jmax_cursor::SkipMode::Strict,
    );
    jmax_cursor::walk::push(&mut cursor, bytes).map_err(as_encode_error)?;
    jmax_cursor::walk::end_input(&mut cursor).map_err(as_encode_error)?;
    loop {
        match jmax_cursor::walk::next(&mut cursor).map_err(as_encode_error)? {
            jmax_abi::syntax::ScanStep::Emit(_) => {}
            jmax_abi::syntax::ScanStep::Finished => return Ok(()),
            jmax_abi::syntax::ScanStep::NeedMore => {
                return Err(Error::new(
                    ErrorCode::InternalError,
                    ErrorSource::Encode,
                    cursor.state().byte_offset,
                    "complete staged output requested additional bytes",
                ));
            }
        }
    }
}

fn as_encode_error(mut error: Error) -> Error {
    error.source = ErrorSource::Encode;
    error
}

fn encode_dom_node(
    document: &jmax_dom::Document,
    node: jmax_dom::Node<'_>,
    writer: &mut jmax_encode::writer::JsonWriter<'_>,
    depth: usize,
) -> Result<()> {
    if depth > usize::from(jmax_encode::DEFAULT_MAX_DEPTH) {
        return Err(Error::new(
            ErrorCode::DepthLimitExceeded,
            ErrorSource::Encode,
            jmax_dom::navigate::raw_span(node).offset,
            "document nesting exceeds the encoder depth limit",
        ));
    }
    match jmax_dom::navigate::kind(node) {
        jmax_dom::NodeKind::Null => writer.null(),
        jmax_dom::NodeKind::Bool => {
            let tag = document.parts().nodes[node.id().0 as usize].kind();
            writer.boolean(tag == jmax_dom::TAG_TRUE)
        }
        jmax_dom::NodeKind::String => {
            let reference = document.metadata()
                .get(node.id().0 as usize)
                .and_then(|meta| meta.string)
                .ok_or_else(|| document_error("string node has no validated string reference"))?;
            let text = document.resolve_str(reference)
                .ok_or_else(|| document_error("string reference is outside its decoded owner"))?;
            writer.string(text)
        }
        jmax_dom::NodeKind::Number => {
            let span = jmax_dom::navigate::raw_span(node);
            let raw = span.slice(document.payload().as_slice())
                .ok_or_else(|| document_error("number span is outside its payload"))?;
            let fragment = jmax_encode::raw::validate(raw, writer_limits(usize::MAX))?;
            writer.raw(fragment)
        }
        jmax_dom::NodeKind::Array => {
            writer.begin_array()?;
            for child in jmax_dom::navigate::array(node)?.iter() {
                encode_dom_node(document, child, writer, depth + 1)?;
            }
            writer.end_array()
        }
        jmax_dom::NodeKind::Object => {
            writer.begin_object()?;
            for member in jmax_dom::navigate::object(node)?.iter() {
                writer.key(member.key)?;
                encode_dom_node(document, member.value, writer, depth + 1)?;
            }
            writer.end_object()
        }
    }
}

fn writer_limits(max_output_bytes: usize) -> jmax_encode::EncodeLimits {
    jmax_encode::EncodeLimits {
        max_output_bytes,
        max_string_bytes: u32::MAX,
        max_number_bytes: u32::MAX,
        ..jmax_encode::EncodeLimits::default()
    }
}

fn document_error(detail: &'static str) -> Error {
    facade_error(ErrorCode::InternalError, ErrorSource::Encode, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Literal(&'static [u8]);
    impl JsonEncode for Literal {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(self.0)
        }
    }

    struct Failing;
    impl JsonEncode for Failing {
        fn encode_json(&self, _sink: &mut dyn ByteSink) -> Result<()> {
            Err(Error::new(ErrorCode::Cancelled, ErrorSource::Decode, 7, "encoder stopped"))
        }
    }

    #[test]
    fn typed_output_accepts_exact_json_scalars_and_nested_values() {
        for (input, expected) in [
            (b"null".as_slice(), b"null".as_slice()),
            (b"true", b"true"),
            (br#"{"a":[1,-2.50e+3],"b":"x"}"#, br#"{"a":[1,-2.50e+3],"b":"x"}"#),
        ] {
            assert_eq!(to_vec(&Literal(input), EncodeOptions::default()).unwrap(), expected);
        }
        assert_eq!(to_string(&Literal(b"0"), EncodeOptions::default()).unwrap(), "0");
    }

    #[test]
    fn malformed_encoder_output_has_precise_encode_error_and_offset() {
        for (bytes, code, offset) in [
            (b"[1,]".as_slice(), ErrorCode::InvalidSyntax, 3),
            (b"null false", ErrorCode::TrailingContent, 5),
            (b"\xff", ErrorCode::InvalidUtf8, 0),
            (b"", ErrorCode::UnexpectedEof, 0),
        ] {
            let error = to_vec(&Literal(bytes), EncodeOptions::default()).unwrap_err();
            assert_eq!((error.code, error.source, error.byte_offset), (code, ErrorSource::Encode, offset));
        }
    }

    #[test]
    fn cap_includes_every_output_byte_and_zero_rejects_empty_impossible_value() {
        let error = to_vec(&Literal(b"null"), EncodeOptions { max_output_bytes: 3 }).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Encode);
        let error = escape_string("", EncodeOptions { max_output_bytes: 1 }).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(escape_string("", EncodeOptions { max_output_bytes: 2 }).unwrap(), b"\"\"");
    }

    #[test]
    fn sink_is_untouched_until_staging_and_validation_succeed() {
        let mut destination = b"prefix".to_vec();
        let error = to_sink(&Literal(b"["), &mut destination, EncodeOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnexpectedEof);
        assert_eq!(destination, b"prefix");
        let error = to_sink(&Failing, &mut destination, EncodeOptions::default()).unwrap_err();
        assert_eq!(error, Error::new(ErrorCode::Cancelled, ErrorSource::Decode, 7, "encoder stopped"));
        assert_eq!(destination, b"prefix");
        to_sink(&Literal(b"true"), &mut destination, EncodeOptions::default()).unwrap();
        assert_eq!(destination, b"prefixtrue");
    }

    #[test]
    fn minimal_string_escaping_preserves_unicode_and_escapes_controls() {
        let encoded = escape_string("a\"\\/\n\u{1}\u{e9}😀", EncodeOptions::default()).unwrap();
        assert_eq!(encoded, "\"a\\\"\\\\/\\n\\u0001é😀\"".as_bytes());
        assert_eq!(to_vec(&Literal(&encoded), EncodeOptions::default()).unwrap(), encoded);
        assert_eq!(escape_string("\u{7f}", EncodeOptions::default()).unwrap(), b"\"\x7f\"");
    }

    #[test]
    fn document_rebuild_verifies_tables_and_emits_decoded_strings() {
        let input = br#" {"a":"x\u00e9","n":0,"v":[true,null]} "#;
        let mut document = super::super::parse::parse_payload(
            super::super::Payload::from_slice(input),
            super::super::ParseOptions::default(),
        ).unwrap();
        assert_eq!(document_to_vec(&document, EncodeOptions::default()).unwrap(),
            br#"{"a":"xé","n":0,"v":[true,null]}"#);

        document.parts.nodes.clear();
        let error = document_to_vec(&document, EncodeOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InternalError);
        assert_eq!(error.source, ErrorSource::Encode);
    }
}
