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

/// Encodes an immutable tape into one contiguous buffer. Validated number
/// lexemes and unescaped string tokens are copied directly from the input.
pub fn encode_tape_document_to_vec(
    document: &jmax_dom::TapeDocument,
    limits: EncodeLimits,
) -> Result<Vec<u8>> {
    encode_tape_view_to_vec(document.as_tape(), limits)
}

/// Encodes a borrowed immutable tape without copying its input allocation.
pub fn encode_borrowed_document_to_vec(
    document: &jmax_dom::BorrowedDocument<'_>,
    limits: EncodeLimits,
) -> Result<Vec<u8>> {
    encode_tape_view_to_vec(document.as_tape(), limits)
}

fn encode_tape_view_to_vec(
    document: jmax_dom::TapeView<'_>,
    limits: EncodeLimits,
) -> Result<Vec<u8>> {
    let mut sink = VecSink::new(limits.max_output_bytes);
    write_tape_view(document, &mut sink, limits, &mut WriterScratch::default())?;
    Ok(sink.into_vec())
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
        .and_then(|()| {
            // When source length fits, no output-size check can fail: whitespace
            // is dropped and escaped tokens canonicalize without growing.
            if document.raw().len() <= writer.limits.max_output_bytes {
                let result = emit_tape_flat(document, &mut writer);
                if result.is_err() {
                    writer.poisoned = true;
                }
                result
            } else {
                emit_tape_flat_checked(document, &mut writer)
            }
        });
    // Always reclaim frames, preserving the original emission/sink error.
    let finished = writer.finish_with_scratch(scratch);
    emitted?;
    finished
}

fn emit_tape_flat_checked(
    document: jmax_dom::TapeView<'_>,
    writer: &mut JsonWriter<'_>,
) -> Result<()> {
    let tape = document.tape();
    let raw = document.raw();
    let decoded = document.decoded();
    let mut ends: Vec<u32> = Vec::new();
    ends.try_reserve(usize::from(document.max_depth()))
        .map_err(|_| encode_error(ErrorCode::BudgetExhausted, "writer frame allocation failed"))?;
    let mut cursor = 0usize;
    while cursor < tape.len() {
        let header = *tape
            .get(cursor)
            .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated tape record"))?;
        let width = header.record_words();
        let next = cursor
            .checked_add(width)
            .filter(|end| width != 0 && *end <= tape.len())
            .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated tape record"))?;
        if header.tag() == jmax_dom::TAPE_TAG_KEY {
            let token_len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
                tape.get(cursor + 2)
                    .map(|word| word.high32())
                    .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated key record"))?
            } else {
                header.compact_len() + 2
            };
            if !header.has_flag(jmax_dom::TAPE_FLAG_ESCAPED) {
                let token = Span32::new(header.raw_offset(), token_len)
                    .slice(raw)
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document key span is outside its owner",
                        )
                    })?;
                writer.trusted_quoted_key(token)?;
            } else {
                let reference = tape.get(cursor + 1).ok_or_else(|| {
                    encode_error(ErrorCode::InternalError, "truncated key record")
                })?;
                let bytes = Span32::new(reference.low32(), reference.high32())
                    .slice(decoded)
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document key is outside its owner",
                        )
                    })?;
                let key = core::str::from_utf8(bytes).map_err(|_| {
                    encode_error(ErrorCode::ConstraintViolation, "document key is not UTF-8")
                })?;
                writer.key(key)?;
            }
            cursor = next;
            continue;
        }
        let subtree_end = tape
            .get(cursor + 1)
            .map(|word| word.high32())
            .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated value record"))?;
        match header.tag() {
            jmax_dom::TAG_NULL => writer.null()?,
            jmax_dom::TAG_FALSE => writer.boolean(false)?,
            jmax_dom::TAG_TRUE => writer.boolean(true)?,
            jmax_dom::TAG_NUMBER => {
                let len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
                    tape.get(cursor + 2)
                        .map(|word| word.low32())
                        .ok_or_else(|| {
                            encode_error(ErrorCode::InternalError, "truncated number record")
                        })?
                } else {
                    header.compact_len()
                };
                let bytes = Span32::new(header.raw_offset(), len)
                    .slice(raw)
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document number is outside its owner",
                        )
                    })?;
                writer.trusted_number_lexeme(bytes)?;
            }
            jmax_dom::TAG_STRING => {
                if !header.has_flag(jmax_dom::TAPE_FLAG_ESCAPED) {
                    let len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
                        tape.get(cursor + 3)
                            .map(|word| word.high32())
                            .ok_or_else(|| {
                                encode_error(ErrorCode::InternalError, "truncated string record")
                            })?
                    } else {
                        header.compact_len() + 2
                    };
                    let token = Span32::new(header.raw_offset(), len)
                        .slice(raw)
                        .ok_or_else(|| {
                            encode_error(
                                ErrorCode::ConstraintViolation,
                                "document string span is outside its owner",
                            )
                        })?;
                    writer.trusted_quoted_string(token)?;
                } else {
                    let reference = tape.get(cursor + 2).ok_or_else(|| {
                        encode_error(ErrorCode::InternalError, "truncated string record")
                    })?;
                    let bytes = Span32::new(reference.low32(), reference.high32())
                        .slice(decoded)
                        .ok_or_else(|| {
                            encode_error(
                                ErrorCode::ConstraintViolation,
                                "document string is outside its owner",
                            )
                        })?;
                    let value = core::str::from_utf8(bytes).map_err(|_| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document string is not UTF-8",
                        )
                    })?;
                    writer.string(value)?;
                }
            }
            jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT => {
                if header.tag() == jmax_dom::TAG_OBJECT {
                    writer.begin_object()?;
                } else {
                    writer.begin_array()?;
                }
                ends.try_reserve(1).map_err(|_| {
                    encode_error(ErrorCode::BudgetExhausted, "writer frame allocation failed")
                })?;
                ends.push(subtree_end);
            }
            _ => {
                return Err(encode_error(
                    ErrorCode::InternalError,
                    "unknown native tape tag",
                ));
            }
        }
        if matches!(header.tag(), jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT)
            && subtree_end as usize != next
        {
            cursor = next;
            continue;
        }
        while ends.last().is_some_and(|end| *end == subtree_end) {
            let kind = writer
                .frames
                .last()
                .map(|frame| frame.kind)
                .ok_or_else(|| {
                    encode_error(ErrorCode::InternalError, "container frame disappeared")
                })?;
            match kind {
                WriteContainer::Object => writer.end_object()?,
                WriteContainer::Array => writer.end_array()?,
            }
            ends.pop();
        }
        cursor = next;
    }
    if !ends.is_empty() || !writer.frames.is_empty() {
        return Err(encode_error(
            ErrorCode::InternalError,
            "native tape has unfinished containers",
        ));
    }
    Ok(())
}

/// Bytes staged on the stack between delegated sink writes.
const BATCH_BYTES: usize = 16 * 1024;

/// Stack staging for the batched tape emitter. Flushes go through the writer's
/// `LimitedSink`, so its limit, poisoning and length-contract checks still
/// govern every byte that reaches the destination.
struct Batch {
    bytes: [u8; BATCH_BYTES],
    len: usize,
}

impl Batch {
    #[inline(always)]
    fn push(&mut self, sink: &mut LimitedSink<'_>, data: &[u8]) -> Result<()> {
        if data.len() > BATCH_BYTES - self.len {
            self.flush(sink)?;
            if data.len() > BATCH_BYTES {
                return sink.write_bytes(data);
            }
        }
        self.bytes[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
        Ok(())
    }
    #[inline(always)]
    fn push_byte(&mut self, sink: &mut LimitedSink<'_>, byte: u8) -> Result<()> {
        if self.len == BATCH_BYTES {
            self.flush(sink)?;
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        Ok(())
    }
    #[inline(always)]
    fn push_number_pair(
        &mut self,
        sink: &mut LimitedSink<'_>,
        first: &[u8],
        second: &[u8],
    ) -> Result<()> {
        let Some(total) = first
            .len()
            .checked_add(second.len())
            .and_then(|len| len.checked_add(1))
        else {
            self.push(sink, first)?;
            self.push_byte(sink, b',')?;
            return self.push(sink, second);
        };
        if total > BATCH_BYTES {
            self.flush(sink)?;
            self.push(sink, first)?;
            self.push_byte(sink, b',')?;
            return self.push(sink, second);
        }
        if total > BATCH_BYTES - self.len {
            self.flush(sink)?;
        }
        let start = self.len;
        let split = start + first.len();
        let end = split + 1 + second.len();
        self.bytes[start..split].copy_from_slice(first);
        self.bytes[split] = b',';
        self.bytes[split + 1..end].copy_from_slice(second);
        self.len = end;
        Ok(())
    }
    #[inline(always)]
    fn flush(&mut self, sink: &mut LimitedSink<'_>) -> Result<()> {
        if self.len != 0 {
            let len = core::mem::replace(&mut self.len, 0);
            sink.write_bytes(&self.bytes[..len])?;
        }
        Ok(())
    }
    /// Same canonical minimal escaping as `JsonWriter::write_string`.
    fn push_escaped(
        &mut self,
        sink: &mut LimitedSink<'_>,
        text: &str,
        masker: jmax_string::EscapeMasker,
    ) -> Result<()> {
        self.push_byte(sink, b'"')?;
        let bytes = text.as_bytes();
        let mut start = 0;
        for index in EscapePositions::new(bytes, masker) {
            if start != index {
                self.push(sink, &bytes[start..index])?;
            }
            let byte = bytes[index];
            let escape: &[u8] = match byte {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                b'\x08' => b"\\b",
                b'\x0c' => b"\\f",
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                _ => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    self.push(
                        sink,
                        &[
                            b'\\',
                            b'u',
                            b'0',
                            b'0',
                            HEX[usize::from(byte >> 4)],
                            HEX[usize::from(byte & 15)],
                        ],
                    )?;
                    start = index + 1;
                    continue;
                }
            };
            self.push(sink, escape)?;
            start = index + 1;
        }
        if start != bytes.len() {
            self.push(sink, &bytes[start..])?;
        }
        self.push_byte(sink, b'"')
    }
}

/// `emit_tape` with the writer's per-token state machine inlined and output
/// staged in stack batches instead of several dynamic sink calls per token.
///
/// Every check `JsonWriter` performs for these operations is repeated in the
/// same order with the same error: key count, decoded string length, number
/// length, array item count, depth and frame allocation. Output size checks are
/// omitted only because the caller established `raw.len() <= max_output_bytes`,
/// which bounds every reservation the writer would make. Frame state is kept
/// in the writer's own frames, so `finish` observes the usual completion state.
fn emit_tape_flat(document: jmax_dom::TapeView<'_>, writer: &mut JsonWriter<'_>) -> Result<()> {
    let limits = writer.limits;
    let masker = writer.escape_masker;
    let mut batch = Batch {
        bytes: [0; BATCH_BYTES],
        len: 0,
    };
    let tape = document.tape();
    let raw = document.raw();
    let decoded = document.decoded();
    let mut cursor = 0usize;
    while cursor < tape.len() {
        let header = *tape
            .get(cursor)
            .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated tape record"))?;
        let width = header.record_words();
        if width == 0 {
            return Err(encode_error(
                ErrorCode::InternalError,
                "unknown native tape tag",
            ));
        }
        let next = cursor
            .checked_add(width)
            .filter(|end| *end <= tape.len())
            .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated tape record"))?;
        if header.tag() == jmax_dom::TAPE_TAG_KEY {
            // key_comma
            let frame = writer
                .frames
                .last_mut()
                .ok_or_else(|| encode_error(ErrorCode::InvalidSyntax, "key outside an object"))?;
            if frame.kind != WriteContainer::Object || !frame.expecting_key {
                return Err(encode_error(
                    ErrorCode::InvalidSyntax,
                    "object is not expecting a key",
                ));
            }
            if frame.count >= limits.max_container_items {
                return Err(encode_error(
                    ErrorCode::BudgetExhausted,
                    "object property limit exceeded",
                ));
            }
            let comma = frame.count != 0;
            let key_offset = header.raw_offset();
            let raw_len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
                tape.get(cursor + 2)
                    .map(|word| word.high32())
                    .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated key record"))?
            } else {
                header.compact_len() + 2
            } as usize;
            let token = jmax_abi::span::Span32::new(key_offset, raw_len as u32)
                .slice(raw)
                .ok_or_else(|| {
                    encode_error(
                        ErrorCode::ConstraintViolation,
                        "document key span is outside its owner",
                    )
                })?;
            if !header.has_flag(jmax_dom::TAPE_FLAG_ESCAPED) {
                check_batched_string_len(token.len() - 2, limits)?;
                if comma {
                    batch.push_byte(&mut writer.sink, b',')?;
                }
                batch.push(&mut writer.sink, token)?;
            } else {
                let refword = tape.get(cursor + 1).ok_or_else(|| {
                    encode_error(ErrorCode::InternalError, "truncated key record")
                })?;
                let key_bytes = jmax_abi::span::Span32::new(refword.low32(), refword.high32())
                    .slice(decoded)
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document key is outside its owner",
                        )
                    })?;
                // SAFETY: immutable tape strings were validated by the DOM builder;
                // the complete decoded owner span is checked immediately above.
                let key = unsafe { core::str::from_utf8_unchecked(key_bytes) };
                check_batched_string_len(key.len(), limits)?;
                if comma {
                    batch.push_byte(&mut writer.sink, b',')?;
                }
                batch.push_escaped(&mut writer.sink, key, masker)?;
            }
            batch.push_byte(&mut writer.sink, b':')?;
            // accept_key
            frame.count += 1;
            frame.expecting_key = false;
            cursor = next;
            continue;
        }
        let subtree_end = tape
            .get(cursor + 1)
            .map(|word| word.high32())
            .ok_or_else(|| encode_error(ErrorCode::InternalError, "truncated value record"))?;
        match header.tag() {
            jmax_dom::TAG_NULL => {
                let comma = batched_value_comma(writer, limits)?;
                batched_token(writer, &mut batch, comma, b"null")?;
            }
            jmax_dom::TAG_FALSE => {
                let comma = batched_value_comma(writer, limits)?;
                batched_token(writer, &mut batch, comma, b"false")?;
            }
            jmax_dom::TAG_TRUE => {
                let comma = batched_value_comma(writer, limits)?;
                batched_token(writer, &mut batch, comma, b"true")?;
            }
            jmax_dom::TAG_NUMBER => {
                let number_len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
                    tape.get(cursor + 2)
                        .map(|word| word.low32())
                        .ok_or_else(|| {
                            encode_error(ErrorCode::InternalError, "truncated number record")
                        })? as usize
                } else {
                    header.compact_len() as usize
                };
                let bytes = jmax_abi::span::Span32::new(header.raw_offset(), number_len as u32)
                    .slice(raw)
                    .ok_or_else(|| {
                        encode_error(
                            ErrorCode::ConstraintViolation,
                            "document number is outside its owner",
                        )
                    })?;
                if bytes.len() > limits.max_number_bytes as usize {
                    return Err(encode_error(
                        ErrorCode::BudgetExhausted,
                        "number text limit exceeded",
                    ));
                }
                let comma = batched_value_comma(writer, limits)?;
                batched_token(writer, &mut batch, comma, bytes)?;
            }
            jmax_dom::TAG_STRING => {
                if !header.has_flag(jmax_dom::TAPE_FLAG_ESCAPED) {
                    let token_len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
                        tape.get(cursor + 3)
                            .map(|word| word.high32())
                            .ok_or_else(|| {
                                encode_error(ErrorCode::InternalError, "truncated string record")
                            })?
                    } else {
                        header.compact_len() + 2
                    };
                    let token = jmax_abi::span::Span32::new(header.raw_offset(), token_len)
                        .slice(raw)
                        .ok_or_else(|| {
                            encode_error(
                                ErrorCode::ConstraintViolation,
                                "document string span is outside its owner",
                            )
                        })?;
                    check_batched_string_len(token.len() - 2, limits)?;
                    let comma = batched_value_comma(writer, limits)?;
                    batched_token(writer, &mut batch, comma, token)?;
                } else {
                    let refword = tape.get(cursor + 2).ok_or_else(|| {
                        encode_error(ErrorCode::InternalError, "truncated string record")
                    })?;
                    let value_bytes =
                        jmax_abi::span::Span32::new(refword.low32(), refword.high32())
                            .slice(decoded)
                            .ok_or_else(|| {
                                encode_error(
                                    ErrorCode::ConstraintViolation,
                                    "document string is outside its owner",
                                )
                            })?;
                    // SAFETY: the DOM validated this complete decoded string and
                    // neither its arena nor its tape can be mutated through this view.
                    let value = unsafe { core::str::from_utf8_unchecked(value_bytes) };
                    check_batched_string_len(value.len(), limits)?;
                    let comma = batched_value_comma(writer, limits)?;
                    if comma {
                        batch.push_byte(&mut writer.sink, b',')?;
                    }
                    batch.push_escaped(&mut writer.sink, value, masker)?;
                    writer.accept_value();
                }
            }
            jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT => {
                let kind = if header.tag() == jmax_dom::TAG_OBJECT {
                    WriteContainer::Object
                } else {
                    WriteContainer::Array
                };
                // A two-number array is a common tape shape in flat datasets.
                // Its two validated number lexemes can be staged together,
                // without pushing and then popping a temporary writer frame.
                let number_pair = if kind == WriteContainer::Array {
                    flat_number_pair(tape, next, subtree_end).filter(|(_, first, _, second, _)| {
                        limits.max_container_items >= 2
                            && *first <= limits.max_number_bytes
                            && *second <= limits.max_number_bytes
                    })
                } else {
                    None
                };
                // begin
                let comma = batched_value_comma(writer, limits)?;
                // write_tape_view checked the immutable document's maximum
                // depth and reserved every frame before publishing any bytes.
                if let Some((first_offset, first_len, second_offset, second_len, after)) =
                    number_pair
                {
                    // Commit the array as a parent value, then apply the same
                    // child length and item-limit checks as the scalar path.
                    let first =
                        Span32::new(first_offset, first_len)
                            .slice(raw)
                            .ok_or_else(|| {
                                encode_error(
                                    ErrorCode::ConstraintViolation,
                                    "document number is outside its owner",
                                )
                            })?;
                    check_batched_number_len(first.len(), limits)?;
                    if limits.max_container_items == 0 {
                        return Err(encode_error(
                            ErrorCode::BudgetExhausted,
                            "array item limit exceeded",
                        ));
                    }
                    let second = Span32::new(second_offset, second_len)
                        .slice(raw)
                        .ok_or_else(|| {
                            encode_error(
                                ErrorCode::ConstraintViolation,
                                "document number is outside its owner",
                            )
                        })?;
                    check_batched_number_len(second.len(), limits)?;
                    if limits.max_container_items < 2 {
                        return Err(encode_error(
                            ErrorCode::BudgetExhausted,
                            "array item limit exceeded",
                        ));
                    }
                    let row_end = tape[cursor + 2].high32();
                    if header.raw_offset().checked_add(1) == Some(first_offset)
                        && first_offset
                            .checked_add(first_len)
                            .and_then(|n| n.checked_add(1))
                            == Some(second_offset)
                        && second_offset
                            .checked_add(second_len)
                            .and_then(|n| n.checked_add(1))
                            == Some(row_end)
                    {
                        // Exact adjacency proves the validated row already has
                        // compact punctuation. Copy its original numeric lexemes
                        // and delimiters together without staging each piece.
                        if comma {
                            batch.push_byte(&mut writer.sink, b',')?;
                        }
                        let row = Span32::new(header.raw_offset(), row_end - header.raw_offset())
                            .slice(raw)
                            .ok_or_else(|| {
                                encode_error(
                                    ErrorCode::InternalError,
                                    "invalid coordinate row span",
                                )
                            })?;
                        batch.push(&mut writer.sink, row)?;
                        writer.accept_value();
                    } else {
                        batched_token(writer, &mut batch, comma, b"[")?;
                        batch.push_number_pair(&mut writer.sink, first, second)?;
                        batch.push_byte(&mut writer.sink, b']')?;
                    }
                    cursor = after;
                    while writer
                        .frames
                        .last()
                        .is_some_and(|frame| frame.tape_end == subtree_end)
                    {
                        let parent_kind =
                            writer
                                .frames
                                .last()
                                .map(|frame| frame.kind)
                                .ok_or_else(|| {
                                    encode_error(
                                        ErrorCode::InternalError,
                                        "container frame disappeared",
                                    )
                                })?;
                        batched_end(writer, &mut batch, parent_kind)?;
                    }
                    continue;
                }
                batched_token(
                    writer,
                    &mut batch,
                    comma,
                    if kind == WriteContainer::Object {
                        b"{"
                    } else {
                        b"["
                    },
                )?;
                writer.frames.push(WriteFrame {
                    tape_end: subtree_end,
                    kind,
                    count: 0,
                    expecting_key: kind == WriteContainer::Object,
                });
                // A nonempty container is closed by its final descendant.
                if subtree_end as usize != next {
                    cursor = next;
                    continue;
                }
                batched_end(writer, &mut batch, kind)?;
            }
            _ => {
                return Err(encode_error(
                    ErrorCode::InternalError,
                    "unknown native tape tag",
                ));
            }
        }
        while writer
            .frames
            .last()
            .is_some_and(|frame| frame.tape_end == subtree_end)
        {
            let kind = writer
                .frames
                .last()
                .map(|frame| frame.kind)
                .ok_or_else(|| {
                    encode_error(ErrorCode::InternalError, "container frame disappeared")
                })?;
            batched_end(writer, &mut batch, kind)?;
        }
        cursor = next;
    }
    if !writer.frames.is_empty() {
        return Err(encode_error(
            ErrorCode::InternalError,
            "native tape has unfinished containers",
        ));
    }
    batch.flush(&mut writer.sink)
}

/// Returns raw number spans for an array whose complete subtree is exactly
/// two adjacent number records. The tape proves each lexeme is valid JSON;
/// this helper checks record and owner bounds before selecting the fast path.
#[inline(always)]
fn flat_number_pair<'a>(
    tape: &[jmax_dom::TapeWord],
    first_cursor: usize,
    subtree_end: u32,
) -> Option<(u32, u32, u32, u32, usize)> {
    // Compact numeric rows occupy exactly four words after their header.
    if first_cursor.checked_add(4)? == subtree_end as usize {
        let pair = tape.get(first_cursor..subtree_end as usize)?;
        let first = pair[0];
        let second = pair[2];
        if first.tag() == jmax_dom::TAG_NUMBER
            && second.tag() == jmax_dom::TAG_NUMBER
            && !first.has_flag(jmax_dom::TAPE_FLAG_EXTENDED)
            && !second.has_flag(jmax_dom::TAPE_FLAG_EXTENDED)
        {
            return Some((
                first.raw_offset(),
                first.compact_len(),
                second.raw_offset(),
                second.compact_len(),
                subtree_end as usize,
            ));
        }
    }
    let Some(first_header) = tape.get(first_cursor).copied() else {
        return None;
    };
    if first_header.tag() != jmax_dom::TAG_NUMBER {
        return None;
    }
    let first_width = first_header.record_words();
    let Some(second_cursor) = first_cursor
        .checked_add(first_width)
        .filter(|end| first_width != 0 && *end <= tape.len())
    else {
        return None;
    };
    let Some(second_header) = tape.get(second_cursor).copied() else {
        return None;
    };
    if second_header.tag() != jmax_dom::TAG_NUMBER {
        return None;
    }
    let second_width = second_header.record_words();
    let Some(after) = second_cursor
        .checked_add(second_width)
        .filter(|end| second_width != 0 && *end <= tape.len())
    else {
        return None;
    };
    if after != subtree_end as usize {
        return None;
    }
    let span = |cursor: usize, header: jmax_dom::TapeWord| -> Option<(u32, u32)> {
        let len = if header.has_flag(jmax_dom::TAPE_FLAG_EXTENDED) {
            tape.get(cursor + 2)?.low32()
        } else {
            header.compact_len()
        };
        Some((header.raw_offset(), len))
    };
    let (first_offset, first_len) = span(first_cursor, first_header)?;
    let (second_offset, second_len) = span(second_cursor, second_header)?;
    Some((first_offset, first_len, second_offset, second_len, after))
}

#[inline(always)]
fn check_batched_number_len(len: usize, limits: EncodeLimits) -> Result<()> {
    if len > limits.max_number_bytes as usize {
        return Err(encode_error(
            ErrorCode::BudgetExhausted,
            "number text limit exceeded",
        ));
    }
    Ok(())
}

#[inline(always)]
fn check_batched_string_len(len: usize, limits: EncodeLimits) -> Result<()> {
    if len > limits.max_string_bytes as usize {
        return Err(encode_error(
            ErrorCode::BudgetExhausted,
            "decoded string limit exceeded",
        ));
    }
    Ok(())
}

/// `JsonWriter::value_comma` over the writer's frames.
#[inline(always)]
fn batched_value_comma(writer: &JsonWriter<'_>, limits: EncodeLimits) -> Result<bool> {
    match writer.frames.last() {
        Some(frame) if frame.kind == WriteContainer::Object => {
            if frame.expecting_key {
                return Err(encode_error(
                    ErrorCode::InvalidSyntax,
                    "object value requires a key",
                ));
            }
            Ok(false)
        }
        Some(frame) => {
            if frame.count >= limits.max_container_items {
                return Err(encode_error(
                    ErrorCode::BudgetExhausted,
                    "array item limit exceeded",
                ));
            }
            Ok(frame.count != 0)
        }
        None if writer.root_written => {
            Err(encode_error(ErrorCode::InvalidSyntax, "second JSON root"))
        }
        None => Ok(false),
    }
}

#[inline(always)]
fn batched_token(
    writer: &mut JsonWriter<'_>,
    batch: &mut Batch,
    comma: bool,
    bytes: &[u8],
) -> Result<()> {
    if comma {
        batch.push_byte(&mut writer.sink, b',')?;
    }
    batch.push(&mut writer.sink, bytes)?;
    writer.accept_value();
    Ok(())
}

/// `JsonWriter::end` over the writer's frames.
fn batched_end(writer: &mut JsonWriter<'_>, batch: &mut Batch, kind: WriteContainer) -> Result<()> {
    let frame = writer.frames.last().ok_or_else(|| {
        encode_error(
            ErrorCode::InvalidSyntax,
            "closing delimiter without an open container",
        )
    })?;
    if frame.kind != kind {
        return Err(encode_error(
            ErrorCode::InvalidSyntax,
            "mismatched closing delimiter",
        ));
    }
    if kind == WriteContainer::Object && !frame.expecting_key {
        return Err(encode_error(
            ErrorCode::InvalidSyntax,
            "object key has no value",
        ));
    }
    batch.push_byte(
        &mut writer.sink,
        if kind == WriteContainer::Object {
            b'}'
        } else {
            b']'
        },
    )?;
    writer.frames.pop();
    Ok(())
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

/// Encoding of absent optional fields; existing entry points omit them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OptionalFieldPolicy {
    #[default]
    Omit,
    Null,
}

/// Encode schema slots with explicit control over absent optional properties.
pub fn encode_slots_with_optional_policy(
    slots: &ValidatedSlots,
    schema: &SchemaIr,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
    optional: OptionalFieldPolicy,
) -> Result<()> {
    encode_values_with_optional_policy(
        slots.payload.as_slice(),
        &slots.decoded,
        &slots.slots,
        schema,
        sink,
        limits,
        optional,
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

fn write_empty_optional(writer: &mut JsonWriter<'_>, wire_name: &str) -> Result<()> {
    writer.key(wire_name)?;
    writer.null()
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
    encode_values_with_optional_policy(
        payload,
        decoded,
        values,
        schema,
        sink,
        limits,
        OptionalFieldPolicy::Omit,
    )
}

fn encode_values_with_optional_policy(
    payload: &[u8],
    decoded: &[u8],
    values: &[SlotValue],
    schema: &SchemaIr,
    sink: &mut dyn ByteSink,
    limits: EncodeLimits,
    optional: OptionalFieldPolicy,
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
            if optional == OptionalFieldPolicy::Null {
                write_empty_optional(&mut writer, &field.name).map_err(context)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_optional_emits_a_null_property() {
        let mut sink = VecSink::new(64);
        let mut writer = JsonWriter::new(&mut sink, EncodeLimits::default());
        writer.begin_object().unwrap();
        write_empty_optional(&mut writer, "nickname").unwrap();
        writer.end_object().unwrap();
        writer.finish().unwrap();
        assert_eq!(sink.as_slice(), br#"{"nickname":null}"#);
    }

    #[test]
    fn optional_null_emission_is_explicit_and_required_fields_stay_required() {
        let mut schema = SchemaIr {
            fields: alloc::vec![SchemaFieldIr {
                field_id: jmax_abi::schema::FieldId(3),
                name: "nickname".into(),
                required: false,
                scalar_kind: Some(ScalarKind::String),
                format: None,
                min_len: None,
                max_len: None,
                min_i64: None,
                max_i64: None,
            }],
            ..SchemaIr::default()
        };
        for (policy, expected) in [
            (OptionalFieldPolicy::Omit, b"{}".as_slice()),
            (
                OptionalFieldPolicy::Null,
                br#"{"nickname":null}"#.as_slice(),
            ),
        ] {
            let mut sink = VecSink::new(64);
            encode_values_with_optional_policy(
                &[],
                &[],
                &[SlotValue::Empty],
                &schema,
                &mut sink,
                EncodeLimits::default(),
                policy,
            )
            .unwrap();
            assert_eq!(sink.as_slice(), expected);
        }
        schema.fields[0].required = true;
        schema.required_count = 1;
        for policy in [OptionalFieldPolicy::Omit, OptionalFieldPolicy::Null] {
            let mut sink = VecSink::new(64);
            let error = encode_values_with_optional_policy(
                &[],
                &[],
                &[SlotValue::Empty],
                &schema,
                &mut sink,
                EncodeLimits::default(),
                policy,
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::MissingRequiredField);
            assert_eq!(error.field_id, 3);
            assert!(sink.as_slice().is_empty());
        }
    }
}
