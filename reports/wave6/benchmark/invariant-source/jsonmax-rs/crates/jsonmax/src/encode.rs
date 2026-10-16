//! Native tape/borrowed output, with separate staged and streaming sink APIs.

use alloc::{string::String, vec::Vec};

use super::{
    BorrowedDocument, ByteSink, Document, EncodeOptions, Error, ErrorCode, ErrorSource, JsonEncode,
    OutputBuffer, Result,
};

/// Encodes one value under the JsonEncode contract into bounded owned storage.
pub fn to_vec<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut staged = OutputBuffer::new(options.max_output_bytes);
    value.encode_json(&mut staged)?;
    Ok(staged.into_vec())
}

/// Encodes one typed value and checks its output UTF-8.
pub fn to_string<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<String> {
    let bytes = to_vec(value, options)?;
    String::from_utf8(bytes).map_err(|error| {
        let offset = u32::try_from(error.utf8_error().valid_up_to()).unwrap_or(u32::MAX);
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Encode,
            offset,
            "encoder output was not UTF-8",
        )
    })
}

/// Stages a complete value before the first destination call. Encoding failure
/// leaves the destination untouched; a destination write error may leave a prefix.
pub fn to_sink<T: JsonEncode + ?Sized>(
    value: &T,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    publish(&to_vec(value, options)?, sink)
}

/// Direct bounded output under the JsonEncode contract, without staging. Any
/// error may leave a partial message; the caller must discard that message.
/// The byte allowance excludes an existing destination prefix.
pub fn to_sink_streaming<T: JsonEncode + ?Sized>(
    value: &T,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    value.encode_json(&mut jmax_encode::LimitedSink::new(
        sink,
        options.max_output_bytes,
    ))
}

/// Emits a native packed tape directly into its final owned output buffer.
pub fn encode_document(document: &Document, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = OutputBuffer::new(options.max_output_bytes);
    encode_document_streaming(document, &mut output, options)?;
    Ok(output.into_vec())
}

/// Compatibility spelling for owned native document output.
pub fn document_to_vec(document: &Document, options: EncodeOptions) -> Result<Vec<u8>> {
    encode_document(document, options)
}

/// Stages native tape output before the first destination call.
pub fn encode_document_to_sink(
    document: &Document,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    publish(&encode_document(document, options)?, sink)
}

/// Writes a native tape directly. Errors may leave a partial message.
pub fn encode_document_streaming(
    document: &Document,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    jmax_encode::typed::write_tape_document(document, sink, writer_limits(options.max_output_bytes))
        .map(|_| ())
}

/// Emits a borrowed native tape without copying input or building legacy tables.
pub fn encode_borrowed_document(
    document: &BorrowedDocument<'_>,
    options: EncodeOptions,
) -> Result<Vec<u8>> {
    let mut output = OutputBuffer::new(options.max_output_bytes);
    encode_borrowed_document_streaming(document, &mut output, options)?;
    Ok(output.into_vec())
}

/// Stages borrowed document output before touching the destination.
pub fn encode_borrowed_document_to_sink(
    document: &BorrowedDocument<'_>,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    publish(&encode_borrowed_document(document, options)?, sink)
}

/// Direct borrowed document output. Errors may leave a partial message.
pub fn encode_borrowed_document_streaming(
    document: &BorrowedDocument<'_>,
    sink: &mut dyn ByteSink,
    options: EncodeOptions,
) -> Result<()> {
    jmax_encode::typed::write_borrowed_document(
        document,
        sink,
        writer_limits(options.max_output_bytes),
    )
    .map(|_| ())
}

/// Emits one quoted JSON string using the native writer's canonical escaping.
pub fn escape_string(value: &str, options: EncodeOptions) -> Result<Vec<u8>> {
    let mut output = OutputBuffer::new(options.max_output_bytes);
    let mut writer =
        jmax_encode::JsonWriter::new(&mut output, writer_limits(options.max_output_bytes));
    writer.string(value)?;
    writer.finish()?;
    Ok(output.into_vec())
}

/// Serialize into retained output and writer frames. This clears the logical
/// output first; an error may leave a prefix in the retained buffer.
pub fn document_to_buffer(
    document: &Document,
    output: &mut OutputBuffer,
    scratch: &mut jmax_encode::WriterScratch,
) -> Result<()> {
    output.clear();
    let limits = writer_limits(output.max_bytes());
    jmax_encode::typed::write_tape_document_with_scratch(document, output, limits, scratch)
        .map(|_| ())
}

/// Borrowed counterpart using the same retained writer-frame storage.
pub fn borrowed_document_to_buffer(
    document: &BorrowedDocument<'_>,
    output: &mut OutputBuffer,
    scratch: &mut jmax_encode::WriterScratch,
) -> Result<()> {
    output.clear();
    let limits = writer_limits(output.max_bytes());
    jmax_encode::typed::write_borrowed_document_with_scratch(document, output, limits, scratch)
        .map(|_| ())
}

fn publish(bytes: &[u8], sink: &mut dyn ByteSink) -> Result<()> {
    sink.reserve(bytes.len())?;
    sink.write_bytes(bytes)
}

fn writer_limits(max_output_bytes: usize) -> jmax_encode::EncodeLimits {
    jmax_encode::EncodeLimits {
        max_output_bytes,
        max_string_bytes: u32::MAX,
        max_number_bytes: u32::MAX,
        ..jmax_encode::EncodeLimits::default()
    }
}
