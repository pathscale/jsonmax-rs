//! Trusted native JSON output with bounded staging.

use alloc::{string::String, vec::Vec};

use super::{
    facade_error, ByteSink, Document, EncodeOptions, Error, ErrorCode, ErrorSource,
    JsonEncode, OutputBuffer, Result,
};

/// Encodes one value under the JsonEncode contract into bounded owned storage.
pub fn to_vec<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut staged = OutputBuffer::new(options.max_output_bytes);
    value.encode_json(&mut staged)?;
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

/// Stages a complete value before making the first destination
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

/// Emits a validated native DOM through the syntax-aware writer.
/// Callers modifying public ABI tables must preserve their validation invariants.
pub fn document_to_vec(document: &Document, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = OutputBuffer::new(options.max_output_bytes);
    let limits = writer_limits(options.max_output_bytes);
    {
        let mut writer = jmax_encode::JsonWriter::new(&mut output, limits);
        encode_dom_node(document, jmax_dom::navigate::root(document), &mut writer, 0)?;
        writer.finish()?;
    }
    Ok(output.into_vec())
}

/// Emits one quoted JSON string using the native writer's canonical escaping.
pub fn escape_string(value: &str, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = OutputBuffer::new(options.max_output_bytes);
    let limits = writer_limits(options.max_output_bytes);
    {
        let mut writer = jmax_encode::JsonWriter::new(&mut output, limits);
        writer.string(value)?;
        writer.finish()?;
    }
    Ok(output.into_vec())
}

fn encode_dom_node(
    document: &jmax_dom::Document,
    node: jmax_dom::Node<'_>,
    writer: &mut jmax_encode::JsonWriter<'_>,
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

/// Serialize into retained output and writer frames. Preparation parses the
/// document separately; this operation times only native DOM emission.
pub fn document_to_buffer(document: &Document, output: &mut OutputBuffer,
    scratch: &mut jmax_encode::WriterScratch) -> Result<()> {
    output.clear();
    let limits = writer_limits(output.max_bytes());
    let mut writer = jmax_encode::JsonWriter::new_with_scratch(output, limits, scratch);
    let result = encode_dom_node(document, jmax_dom::navigate::root(document), &mut writer, 0);
    let finished = writer.finish_with_scratch(scratch);
    result?;
    finished.map(|_| ())
}
