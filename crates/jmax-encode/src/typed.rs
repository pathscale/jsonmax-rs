//! Staged typed encoding, direct DOM streaming and checked schema slot output.
//!
//! `encode_*` APIs stage complete messages before publishing. `write_*` APIs
//! stream directly and may leave a prefix on any error; discard that message.

use super::*;
use core::cmp::Ordering;
use jmax_abi::span::{Span32, SpanSource, StrRef};
use jmax_abi::syntax::ScalarKind;

/// Runs the ABI encoder, validates its entire output, then publishes it.
///
/// A custom encoder is responsible for its own computation and termination.
/// Its output storage and the subsequently accepted JSON are bounded here.
pub fn encode<T: JsonEncode + ?Sized>(
    value: &T,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    let bytes = encode_to_vec(value, limits)?;
    publish(&bytes, sink, limits)
}

/// Returns the encoder's validated owned buffer without copying its contents.
pub fn encode_to_vec<T: JsonEncode + ?Sized>(value: &T, limits: EncodeLimits) -> Result<Vec<u8>> {
    let mut temporary = VecSink::new(limits.max_output_bytes);
    value.encode_json(&mut temporary)?;
    raw::validate(temporary.as_slice(), limits)?;
    Ok(temporary.into_vec())
}

/// Emits the shared DOM/ABI representation through the bounded JSON writer.
///
/// Public tables are checked before any navigation. Strings use their cooked
/// owner-relative references; numbers retain their validated original spelling.
/// Emission is iterative and destination writes start only after completion.
/// Existing syntax or schema provenance is preserved without issuing a proof.
pub fn encode_document(
    document: &ValidatedDocument,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    let bytes = encode_document_to_vec(document, limits)?;
    publish(&bytes, sink, limits)
}

/// Returns compact JSON for the legacy public ABI document.
pub fn encode_document_to_vec(
    document: &ValidatedDocument,
    limits: EncodeLimits,
) -> Result<Vec<u8>> {
    let mut temporary = VecSink::new(limits.max_output_bytes);
    write_document(document, &mut temporary, limits)?;
    Ok(temporary.into_vec())
}

/// Streams the legacy public ABI document after checking its current tables.
/// No proof on this mutable representation authorizes trusted string access.
/// A failure may leave a prefix in `sink`; existing sink bytes are excluded
/// from the returned byte count and this message's output allowance.
pub fn write_document(
    document: &ValidatedDocument,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<usize> {
    document.validate_layout().map_err(|mut error| {
        error.source = ErrorSource::Encode;
        error
    })?;
    if document.metadata().max_depth > limits.max_depth {
        return Err(encode_error(
            ErrorCode::DepthLimitExceeded,
            "document exceeds encoder depth limit",
        ));
    }
    let mut writer = JsonWriter::new(sink, limits);
    // Exclusive subtree ends allow a single preorder pass without recursion or
    // allocating a work item for every child of a wide container.
    let mut open: Vec<(u32, bool)> = Vec::new();
    for (index, word) in document.parts().nodes.iter().copied().enumerate() {
        let id = index as u32;
        while open.last().is_some_and(|entry| entry.0 == id) {
            if let Some((_, object)) = open.pop() {
                if object {
                    writer.end_object()?;
                } else {
                    writer.end_array()?;
                }
            }
        }
        let meta = document.parts().node_metadata[index];
        if let Some(key) = meta.key {
            let text = document.resolve_str(key).ok_or_else(|| {
                encode_error(
                    ErrorCode::ConstraintViolation,
                    "document key is outside its owner",
                )
            })?;
            writer.key(text)?;
        }
        match word.kind() {
            jmax_dom::TAG_NULL => writer.null()?,
            jmax_dom::TAG_FALSE => writer.boolean(false)?,
            jmax_dom::TAG_TRUE => writer.boolean(true)?,
            jmax_dom::TAG_STRING => {
                let text = meta
                    .string
                    .and_then(|reference| document.resolve_str(reference))
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document string is outside its owner",
                        )
                    })?;
                writer.string(text)?;
            }
            jmax_dom::TAG_NUMBER => {
                let bytes = document.parts().spans[index]
                    .slice(document.payload().as_slice())
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document number is outside its owner",
                        )
                    })?;
                let fragment = raw::validate(bytes, limits)?;
                if fragment.kind() != FragmentKind::Number {
                    return Err(encode_error(
                        ErrorCode::TypeMismatch,
                        "document number span is not a JSON number",
                    ));
                }
                writer.raw(fragment)?;
            }
            jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT => {
                let object = word.kind() == jmax_dom::TAG_OBJECT;
                open.try_reserve(1).map_err(|_| {
                    encode_error(
                        ErrorCode::BudgetExhausted,
                        "document encoder allocation failed",
                    )
                })?;
                if object {
                    writer.begin_object()?;
                } else {
                    writer.begin_array()?;
                }
                let record = document.parts().containers[word.payload_index() as usize];
                open.push((record.end_node, object));
            }
            _ => {
                return Err(encode_error(
                    ErrorCode::ConstraintViolation,
                    "document contains an unknown node tag",
                ));
            }
        }
    }
    while let Some((_, object)) = open.pop() {
        if object {
            writer.end_object()?;
        } else {
            writer.end_array()?;
        }
    }
    writer.finish()
}

/// Streams compact JSON directly from the immutable owned packed tape.
/// Cooked keys/strings use trusted node handles; numbers keep their spelling.
/// Errors may leave a partial message in the destination.
pub fn write_tape_document(
    document: &jmax_dom::TapeDocument,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<usize> {
    write_tape_document_with_scratch(document, sink, limits, &mut WriterScratch::default())
}

/// Retains writer frames on success and failure for subsequent messages.
pub fn write_tape_document_with_scratch(
    document: &jmax_dom::TapeDocument,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
    scratch: &mut WriterScratch,
) -> Result<usize> {
    write_tape_view(document.as_tape(), sink, limits, scratch)
}

/// Streams the borrowed packed tape without acquiring or copying its input.
/// Errors may leave a partial message in the destination.
pub fn write_borrowed_document(
    document: &jmax_dom::BorrowedDocument<'_>,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<usize> {
    write_borrowed_document_with_scratch(document, sink, limits, &mut WriterScratch::default())
}

pub fn write_borrowed_document_with_scratch(
    document: &jmax_dom::BorrowedDocument<'_>,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
    scratch: &mut WriterScratch,
) -> Result<usize> {
    write_tape_view(document.as_tape(), sink, limits, scratch)
}

fn write_tape_view(
    document: jmax_dom::TapeView<'_>,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
    scratch: &mut WriterScratch,
) -> Result<usize> {
    if document.max_depth() > limits.max_depth {
        return Err(encode_error(
            ErrorCode::DepthLimitExceeded,
            "document exceeds encoder depth limit",
        ));
    }
    let mut writer = JsonWriter::new_with_scratch(sink, limits, scratch);
    let emitted = writer
        .reserve_document(document.raw().len(), document.max_depth())
        .and_then(|()| emit_tape(document, &mut writer));
    // Always reclaim frames, preserving the original emission/sink error.
    let finished = writer.finish_with_scratch(scratch);
    emitted?;
    finished
}

fn emit_tape(document: jmax_dom::TapeView<'_>, writer: &mut JsonWriter<'_>) -> Result<()> {
    // Nodes advances across actual TapeWord records, skipping extension/key
    // words in O(1). No legacy tables, ID-membership scans or work stack.
    for node in document.nodes() {
        if let Some(token) = node.unescaped_key_token() {
            writer.trusted_quoted_key(token)?;
        } else if let Some(key) = node.key() {
            writer.key(key)?;
        }
        let header = node.header();
        match header.tag() {
            jmax_dom::TAG_NULL => writer.null()?,
            jmax_dom::TAG_FALSE => writer.boolean(false)?,
            jmax_dom::TAG_TRUE => writer.boolean(true)?,
            jmax_dom::TAG_NUMBER => writer.trusted_number_lexeme(node.raw()?)?,
            jmax_dom::TAG_STRING => {
                if let Some(token) = node.unescaped_string_token() {
                    writer.trusted_quoted_string(token)?;
                } else {
                    writer.string(node.string()?)?;
                }
            }
            jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT => {
                if header.tag() == jmax_dom::TAG_OBJECT {
                    writer.begin_object()?;
                } else {
                    writer.begin_array()?;
                }
                // A nonempty container is closed by its final descendant.
                if node.subtree_end() as usize != node.id().0 as usize + header.record_words() {
                    continue;
                }
                close_tape_container(node, writer)?;
            }
            _ => {
                return Err(encode_error(
                    ErrorCode::InternalError,
                    "unknown native tape tag",
                ));
            }
        }
        let end = node.subtree_end();
        let mut ancestor = node.parent();
        while let Some(parent) = ancestor {
            if parent.subtree_end() != end {
                break;
            }
            close_tape_container(parent, writer)?;
            ancestor = parent.parent();
        }
    }
    Ok(())
}

fn close_tape_container(node: jmax_dom::Node<'_>, writer: &mut JsonWriter<'_>) -> Result<()> {
    if node.header().tag() == jmax_dom::TAG_OBJECT {
        writer.end_object()
    } else {
        writer.end_array()
    }
}

/// Emits present fields in schema order, rechecking current values and owners.
///
/// Field position, rather than FieldId, indexes the slots. The proof is neither
/// trusted as a schema match nor replaced with newly manufactured evidence.
/// Named formats are unsupported by this version of the ABI adapter.
pub fn encode_slots(
    slots: &ValidatedSlots,
    schema: &SchemaIr,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    encode_values(
        slots.payload.as_slice(),
        &slots.decoded,
        &slots.slots,
        schema,
        sink,
        limits,
    )
}

fn publish(bytes: &[u8], sink: &mut dyn ByteSink, limits: EncodeLimits) -> Result<()> {
    let mut bounded = LimitedSink::new(sink, limits.max_output_bytes);
    sink::append(&mut bounded, bytes, limits.max_output_bytes)
}

fn field_error(
    field: &SchemaFieldIr,
    schema: &SchemaIr,
    code: ErrorCode,
    detail: &'static str,
) -> Error {
    encode_error(code, detail).with_schema(schema.schema_id.0, field.field_id.0)
}

/// Kept independent of proof construction so every owner/value path can also
/// be exercised by safe tests. This is the complete implementation used above.
fn encode_values(
    payload: &[u8],
    decoded: &[u8],
    values: &[SlotValue],
    schema: &SchemaIr,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
) -> Result<()> {
    preflight(schema)?;
    if values.len() != schema.fields.len() {
        return Err(encode_error(
            ErrorCode::ConstraintViolation,
            "slot count does not match schema",
        )
        .with_schema(schema.schema_id.0, 0));
    }
    let mut temporary = VecSink::new(limits.max_output_bytes);
    let mut scratch = VecSink::new(limits.max_output_bytes);
    let mut writer = JsonWriter::new(&mut temporary, limits);
    writer
        .begin_object()
        .map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    // begin_object established that a depth allowance of one exists.
    let value_limits = EncodeLimits {
        max_depth: limits.max_depth - 1,
        ..limits
    };
    for (field, &value) in schema.fields.iter().zip(values) {
        let context = |error: Error| error.with_schema(schema.schema_id.0, field.field_id.0);
        if value == SlotValue::Empty {
            if field.required {
                return Err(field_error(
                    field,
                    schema,
                    ErrorCode::MissingRequiredField,
                    "required output field is absent",
                ));
            }
            continue;
        }
        let fragment =
            render_slot(value, payload, decoded, &mut scratch, value_limits).map_err(context)?;
        check_field(fragment, field).map_err(context)?;
        writer.key(&field.name).map_err(context)?;
        writer.raw(fragment).map_err(context)?;
    }
    writer
        .end_object()
        .map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    writer
        .finish()
        .map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    raw::validate(temporary.as_slice(), limits)
        .map_err(|error| error.with_schema(schema.schema_id.0, 0))?;
    // Preserve all external error metadata, including its original source.
    publish(temporary.as_slice(), sink, limits)
}

fn preflight(schema: &SchemaIr) -> Result<()> {
    // Formats are unsupported regardless of requiredness or other metadata.
    for field in &schema.fields {
        if field.format.is_some() {
            return Err(field_error(
                field,
                schema,
                ErrorCode::UnsupportedSchemaFeature,
                "named formats are not supported by typed encoding",
            ));
        }
    }
    let mut required = 0u16;
    for field in &schema.fields {
        let invalid = |detail| field_error(field, schema, ErrorCode::ConstraintViolation, detail);
        if field.required {
            required = required
                .checked_add(1)
                .ok_or_else(|| invalid("required field count exceeds the ABI range"))?;
        }
        let length = field.min_len.is_some() || field.max_len.is_some();
        let integer = field.min_i64.is_some() || field.max_i64.is_some();
        if (length && field.scalar_kind != Some(ScalarKind::String))
            || (integer && field.scalar_kind != Some(ScalarKind::Number))
        {
            return Err(invalid("constraint family does not match the scalar kind"));
        }
        if matches!((field.min_len, field.max_len), (Some(min), Some(max)) if min > max)
            || matches!((field.min_i64, field.max_i64), (Some(min), Some(max)) if min > max)
        {
            return Err(invalid("schema bounds are reversed"));
        }
    }
    if required != schema.required_count {
        return Err(encode_error(
            ErrorCode::ConstraintViolation,
            "schema required count is inaccurate",
        )
        .with_schema(schema.schema_id.0, 0));
    }
    // Sort references, leaving field order intact. Unstable sorting needs no
    // additional allocation and avoids quadratic duplicate scans.
    let mut fields: Vec<&SchemaFieldIr> = Vec::new();
    fields.try_reserve(schema.fields.len()).map_err(|_| {
        encode_error(
            ErrorCode::BudgetExhausted,
            "schema preflight allocation failed",
        )
        .with_schema(schema.schema_id.0, 0)
    })?;
    fields.extend(schema.fields.iter());
    fields.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    for pair in fields.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(field_error(
                pair[1],
                schema,
                ErrorCode::ConstraintViolation,
                "schema field names are not unique",
            ));
        }
    }
    fields.sort_unstable_by_key(|field| field.field_id.0);
    for pair in fields.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(field_error(
                pair[1],
                schema,
                ErrorCode::ConstraintViolation,
                "schema field identifiers are not unique",
            ));
        }
    }
    Ok(())
}

fn resolve_string<'a>(reference: StrRef, payload: &'a [u8], decoded: &'a [u8]) -> Result<&'a str> {
    let owner = match reference.source {
        SpanSource::RawPayload => payload,
        SpanSource::DecodedArena => decoded,
    };
    let bytes = reference.span.slice(owner).ok_or_else(|| {
        encode_error(
            ErrorCode::ConstraintViolation,
            "string span is outside its owner",
        )
    })?;
    let text = core::str::from_utf8(bytes).map_err(|error| {
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Encode,
            u32::try_from(error.valid_up_to()).unwrap_or(u32::MAX),
            "invalid slot string UTF-8",
        )
    })?;
    if text.chars().count() != reference.scalar_len as usize {
        return Err(encode_error(
            ErrorCode::ConstraintViolation,
            "slot string scalar count is inaccurate",
        ));
    }
    Ok(text)
}

/// Render through the canonical writer rather than independently formatting
/// numbers or decimals. Raw spans retain their exact spelling and whitespace.
fn render_slot<'a>(
    value: SlotValue,
    payload: &'a [u8],
    decoded: &[u8],
    scratch: &'a mut VecSink,
    limits: EncodeLimits,
) -> Result<RawFragment<'a>> {
    if let SlotValue::RawSpan(span) = value {
        let bytes = span.slice(payload).ok_or_else(|| {
            encode_error(
                ErrorCode::ConstraintViolation,
                "raw span is outside the payload",
            )
        })?;
        return raw::validate(bytes, limits);
    }
    scratch.bytes.clear();
    let mut writer = JsonWriter::new(scratch, limits);
    match value {
        SlotValue::Bool(value) => writer.boolean(value)?,
        SlotValue::U64(value) => writer.u64(value)?,
        SlotValue::I64(value) => writer.i64(value)?,
        SlotValue::F64Bits(bits) => writer.f64(f64::from_bits(bits))?,
        SlotValue::Decimal(value) => writer.decimal(value)?,
        SlotValue::Str(reference) => writer.string(resolve_string(reference, payload, decoded)?)?,
        SlotValue::Empty | SlotValue::RawSpan(_) => {
            return Err(encode_error(
                ErrorCode::InternalError,
                "slot has no standalone writer representation",
            ));
        }
    }
    writer.finish()?;
    raw::validate(scratch.as_slice(), limits)
}

fn check_field(fragment: RawFragment<'_>, field: &SchemaFieldIr) -> Result<()> {
    let expected = match field.scalar_kind {
        None => return Ok(()),
        Some(ScalarKind::String) => FragmentKind::String,
        Some(ScalarKind::Number) => FragmentKind::Number,
        Some(ScalarKind::True | ScalarKind::False) => FragmentKind::Boolean,
        Some(ScalarKind::Null) => FragmentKind::Null,
    };
    if fragment.kind() != expected {
        return Err(encode_error(
            ErrorCode::TypeMismatch,
            "output scalar category does not match schema",
        ));
    }
    let token = trim_json_whitespace(fragment.as_bytes());
    if field.min_len.is_some() || field.max_len.is_some() {
        // String fragments have already been strictly validated, including
        // quotes and surrogate pairing. Measurement counts decoded scalars.
        let content_len = u32::try_from(token.len() - 2).map_err(|_| {
            encode_error(
                ErrorCode::ConstraintViolation,
                "string length exceeds the schema counter range",
            )
        })?;
        let measured = jmax_string::length::decoded_length(token, Span32::new(1, content_len))
            .map_err(|mut error| {
                error.source = ErrorSource::Encode;
                error
            })?;
        if field.min_len.is_some_and(|min| measured.scalar_len < min)
            || field.max_len.is_some_and(|max| measured.scalar_len > max)
        {
            return Err(encode_error(
                ErrorCode::ConstraintViolation,
                "string length is outside schema bounds",
            ));
        }
    }
    if field.min_i64.is_some() || field.max_i64.is_some() {
        let digits = token.strip_prefix(b"-").unwrap_or(token);
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(encode_error(
                ErrorCode::TypeMismatch,
                "integer schema requires integer JSON grammar",
            ));
        }
        if field
            .min_i64
            .is_some_and(|min| compare_integer(token, min) == Ordering::Less)
            || field
                .max_i64
                .is_some_and(|max| compare_integer(token, max) == Ordering::Greater)
        {
            return Err(encode_error(
                ErrorCode::ConstraintViolation,
                "integer is outside schema bounds",
            ));
        }
    }
    Ok(())
}

fn trim_json_whitespace(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Compare a validated integer token to i64 without narrowing or rounding.
/// Arbitrarily long raw integers can satisfy a one-sided bound. Negative zero
/// compares equal to zero, while its original JSON spelling remains intact.
fn compare_integer(token: &[u8], bound: i64) -> Ordering {
    let digits = token.strip_prefix(b"-").unwrap_or(token);
    let negative = token.first() == Some(&b'-') && digits != b"0";
    let bound_negative = bound < 0;
    if negative != bound_negative {
        return if negative {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let mut magnitude = bound.unsigned_abs();
    let mut buffer = [0u8; 20];
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    let other = &buffer[start..];
    let order = digits
        .len()
        .cmp(&other.len())
        .then_with(|| digits.cmp(other));
    if negative { order.reverse() } else { order }
}
