//! Bounded serialization, string escaping, and numeric formatting benchmarks.

use alloc::vec::Vec;
use core::mem::size_of;

use jmax_abi::{
    error::{Error, ErrorCode, ErrorSource},
    node::{Container32, NodeWord},
    span::{Span32, SpanSource, StrRef},
};
use jmax_encode::{EncodeLimits, JsonWriter};

use crate::{
    bench_error, corpus, measure, mix_digest, BenchmarkConfig,
    BenchmarkMode, BenchmarkOperation, BenchmarkReport, Clock, Observation,
    Payload, Result, Workload,
};

const MAX_OUTPUT: usize = jsonmax::DEFAULT_MAX_OUTPUT_BYTES;
const SERIALIZE_TAG: u64 = 0x7365_7269_616c_697a;
const ESCAPE_TAG: u64 = 0x6573_6361_7065_2d31;
const FORMAT_TAG: u64 = 0x666f_726d_6174_2d31;

/// Measure compact JSON serialization of each strictly parsed input.
pub fn run_serialize(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    run_serialize_regime(workload, config, clock, true)
}

/// Measure quoted JSON escaping of every string and object key in source order.
pub fn run_escape(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    prepare(workload, config)?;
    let documents = prepare_documents(workload)?;
    for document in &documents { verify_escaped(document)?; }
    let mut operation = PreparedOperation::new("jsonmax.escape", BenchmarkMode::Escape, documents)?;
    measure(workload, &mut operation, config, clock)
}

/// Measure native numeric lexeme formatting in source order, one per LF line.
pub fn run_format(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    prepare(workload, config)?;
    let documents = prepare_documents(workload)?;
    for document in &documents { verify_formatted(document)?; }
    let mut operation = PreparedOperation::new("jsonmax.format", BenchmarkMode::Format, documents)?;
    measure(workload, &mut operation, config, clock)
}

/// Compare matching output operations after an untimed semantic preflight.
pub fn compare(
    workload: &Workload,
    left: &mut dyn BenchmarkOperation,
    right: &mut dyn BenchmarkOperation,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<(BenchmarkReport, BenchmarkReport)> {
    config.validate()?;
    if workload.payloads.is_empty() {
        return Err(bench_error(ErrorCode::ConstraintViolation, "empty benchmark corpus"));
    }
    let mode = left.mode();
    if mode != right.mode()
        || !matches!(mode, BenchmarkMode::Serialize | BenchmarkMode::Escape | BenchmarkMode::Format)
    {
        return Err(bench_error(ErrorCode::ConstraintViolation,
            "comparison requires matching serialize, escape, or format modes"));
    }
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let documents = prepare_documents(workload)?;
    for document in &documents {
        match mode {
            BenchmarkMode::Serialize => verify_serialized(document)?,
            BenchmarkMode::Escape => verify_escaped(document)?,
            BenchmarkMode::Format => verify_formatted(document)?,
            _ => return Err(bench_error(ErrorCode::ConstraintViolation, "unsupported output comparison mode")),
        }
    }
    // Operations receive the same immutable source corpus and are checked in
    // payload order before either timed report is produced.
    for input in &workload.payloads {
        let a = left.execute(input)?;
        let b = right.execute(input)?;
        if a.digest != b.digest || a.output_bytes != b.output_bytes {
            return Err(bench_error(ErrorCode::ConstraintViolation,
                "comparison operations produced different output"));
        }
    }
    let a = measure(workload, left, config, clock)?;
    let b = measure(workload, right, config, clock)?;
    Ok((a, b))
}

fn prepare(workload: &Workload, config: BenchmarkConfig) -> Result<()> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)
}

fn prepare_documents(workload: &Workload) -> Result<Vec<jsonmax::Document>> {
    let mut documents = Vec::new();
    documents.try_reserve_exact(workload.payloads.len())
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "prepared document allocation failed"))?;
    for payload in &workload.payloads {
        documents.push(jsonmax::parse::parse_payload(payload.clone(), jsonmax::ParseOptions::default())?);
    }
    Ok(documents)
}

struct PreparedOperation {
    name: &'static str,
    mode: BenchmarkMode,
    documents: Vec<jsonmax::Document>,
    next_document: usize,
    sink: Vec<u8>,
    retained_documents_bytes: u64,
}

impl PreparedOperation {
    fn new(name: &'static str, mode: BenchmarkMode, documents: Vec<jsonmax::Document>) -> Result<Self> {
        let retained_documents_bytes = retained_documents_bytes(&documents)?;
        Ok(Self { name, mode, documents, next_document: 0, sink: Vec::new(), retained_documents_bytes })
    }

    fn execute_document(
        mode: BenchmarkMode,
        document: &jsonmax::Document,
        sink: &mut Vec<u8>,
        retained_documents_bytes: u64,
    ) -> Result<Observation> {
        sink.clear();
        match mode {
            BenchmarkMode::Serialize => emit_document(document, sink)?,
            BenchmarkMode::Escape => emit_escaped(document, sink)?,
            BenchmarkMode::Format => emit_formatted(document, sink)?,
            _ => return Err(bench_error(ErrorCode::InternalError, "invalid prepared output mode")),
        }
        let output_bytes = u64::try_from(sink.len())
            .map_err(|_| encode_error(ErrorCode::PayloadTooLarge, "output length exceeds u64"))?;
        // Full output equality is verified before measurement. Keep timed
        // consumption constant-size so hashing does not dominate emission.
        core::hint::black_box(sink.as_slice());
        let boundary = u64::from(sink.first().copied().unwrap_or(0))
            | (u64::from(sink.last().copied().unwrap_or(0)) << 8);
        let digest = mix_digest(mix_digest(mode_tag(mode), output_bytes), boundary);
        let retained_bytes = retained_documents_bytes
            .checked_add(u64::try_from(sink.capacity()).map_err(|_| {
                bench_error(ErrorCode::BudgetExhausted, "output capacity exceeds u64")
            })?)
            .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained byte count overflow"))?;
        Ok(Observation { digest, output_bytes, retained_bytes })
    }
}

impl BenchmarkOperation for PreparedOperation {
    fn name(&self) -> &str { self.name }
    fn mode(&self) -> BenchmarkMode { self.mode }

    fn execute(&mut self, _input: &Payload) -> Result<Observation> {
        if self.documents.is_empty() {
            return Err(bench_error(ErrorCode::InternalError, "prepared corpus is empty"));
        }
        let index = self.next_document;
        let observation = Self::execute_document(
            self.mode, &self.documents[index], &mut self.sink, self.retained_documents_bytes,
        )?;
        self.next_document += 1;
        if self.next_document == self.documents.len() { self.next_document = 0; }
        Ok(observation)
    }
}

fn mode_tag(mode: BenchmarkMode) -> u64 {
    match mode {
        BenchmarkMode::Serialize => SERIALIZE_TAG,
        BenchmarkMode::Escape => ESCAPE_TAG,
        BenchmarkMode::Format => FORMAT_TAG,
        _ => 0,
    }
}

fn emit_document(document: &jsonmax::Document, out: &mut Vec<u8>) -> Result<()> {
    let nodes = &document.parts().nodes;
    let metadata = document.metadata();
    if nodes.is_empty() || metadata.len() != nodes.len() {
        return Err(encode_error(ErrorCode::InternalError, "prepared DOM tables are inconsistent"));
    }
    let limits = writer_limits(out)?;
    let mut writer = JsonWriter::new(out, limits);
    emit_node(document, 0, &mut writer)?;
    writer.finish().map(|_| ())
}

fn emit_node(document: &jsonmax::Document, index: usize, writer: &mut JsonWriter<'_>) -> Result<usize> {
    let node = *document.parts().nodes.get(index).ok_or_else(|| {
        encode_error(ErrorCode::InternalError, "DOM node index is outside prepared tables")
    })?;
    let metadata = *document.metadata().get(index).ok_or_else(|| {
        encode_error(ErrorCode::InternalError, "DOM metadata index is outside prepared tables")
    })?;
    match node.kind() {
        jmax_dom::TAG_NULL => writer.null()?,
        jmax_dom::TAG_FALSE => writer.boolean(false)?,
        jmax_dom::TAG_TRUE => writer.boolean(true)?,
        jmax_dom::TAG_NUMBER => {
            let span = *document.parts().spans.get(index).ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "number node has no span")
            })?;
            let number = span.slice(document.payload().as_slice()).ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "number span is outside payload")
            })?;
            writer.number_lexeme(number)?;
        }
        jmax_dom::TAG_STRING => {
            let text = resolve_string(document, metadata.string.ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "string node has no decoded reference")
            })?)?;
            writer.string(text)?;
        }
        jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT => {
            let container = container_for(document, node)?;
            let object = node.kind() == jmax_dom::TAG_OBJECT;
            if object { writer.begin_object()?; } else { writer.begin_array()?; }
            let mut child = if container.child_count == 0 { None } else { Some(container.first_child as usize) };
            for _ in 0..container.child_count {
                let at = child.ok_or_else(|| encode_error(ErrorCode::InternalError, "container child link ended early"))?;
                let child_meta = *document.metadata().get(at).ok_or_else(|| encode_error(ErrorCode::InternalError, "child metadata is missing"))?;
                if object {
                    let key = resolve_string(document, child_meta.key.ok_or_else(|| encode_error(ErrorCode::InternalError, "object member has no key"))?)?;
                    writer.key(key)?;
                }
                let end = emit_node(document, at, writer)?;
                child = document.metadata().get(at).and_then(|meta| meta.next_sibling).map(|id| id.0 as usize);
                if end <= at { return Err(encode_error(ErrorCode::InternalError, "child subtree has invalid extent")); }
            }
            if child.is_some() { return Err(encode_error(ErrorCode::InternalError, "container has excess child links")); }
            if object { writer.end_object()?; } else { writer.end_array()?; }
            return Ok(container.end_node as usize);
        }
        _ => return Err(encode_error(ErrorCode::InternalError, "unknown prepared DOM node kind")),
    }
    Ok(index + 1)
}

fn emit_escaped(document: &jsonmax::Document, out: &mut Vec<u8>) -> Result<()> {
    let mut seen = false;
    for (index, node) in document.parts().nodes.iter().enumerate() {
        let meta = document.metadata().get(index).ok_or_else(|| encode_error(ErrorCode::InternalError, "DOM metadata is missing"))?;
        if let Some(key) = meta.key {
            emit_ref_quoted(document, key, out)?;
            seen = true;
        }
        if node.kind() == jmax_dom::TAG_STRING {
            let value = meta.string.ok_or_else(|| encode_error(ErrorCode::InternalError, "string node has no decoded reference"))?;
            emit_ref_quoted(document, value, out)?;
            seen = true;
        }
    }
    if !seen { return Err(bench_error(ErrorCode::ConstraintViolation, "escape workload contains no strings")); }
    Ok(())
}

fn emit_formatted(document: &jsonmax::Document, out: &mut Vec<u8>) -> Result<()> {
    let mut count = 0usize;
    for (index, node) in document.parts().nodes.iter().enumerate() {
        if node.kind() != jmax_dom::TAG_NUMBER { continue; }
        let span = *document.parts().spans.get(index).ok_or_else(|| encode_error(ErrorCode::InternalError, "number node has no span"))?;
        let lexeme = span.slice(document.payload().as_slice()).ok_or_else(|| encode_error(ErrorCode::InternalError, "number span is outside payload"))?;
        if count != 0 { append(out, b"\n")?; }
        append(out, lexeme)?;
        count += 1;
    }
    if count == 0 { return Err(bench_error(ErrorCode::ConstraintViolation, "format workload contains no numbers")); }
    Ok(())
}

fn verify_serialized(document: &jsonmax::Document) -> Result<()> {
    let mut first = Vec::new();
    emit_document(document, &mut first)?;
    let reparsed = jsonmax::parse::parse_payload(Payload::from_slice(&first), jsonmax::ParseOptions::default())?;
    let mut second = Vec::new();
    emit_document(&reparsed, &mut second)?;
    if first != second { return Err(encode_error(ErrorCode::InternalError, "serialized output failed semantic round trip")); }
    Ok(())
}

fn verify_escaped(document: &jsonmax::Document) -> Result<()> {
    let mut found = false;
    for (index, node) in document.parts().nodes.iter().enumerate() {
        let meta = document.metadata().get(index).ok_or_else(|| encode_error(ErrorCode::InternalError, "DOM metadata is missing"))?;
        if let Some(key) = meta.key { verify_one_string(document, key)?; found = true; }
        if node.kind() == jmax_dom::TAG_STRING {
            verify_one_string(document, meta.string.ok_or_else(|| encode_error(ErrorCode::InternalError, "string reference is missing"))?)?;
            found = true;
        }
    }
    if !found { return Err(bench_error(ErrorCode::ConstraintViolation, "escape workload contains no strings")); }
    Ok(())
}

fn verify_one_string(document: &jsonmax::Document, reference: StrRef) -> Result<()> {
    let expected = resolve_string(document, reference)?;
    let mut bytes = Vec::new();
    emit_ref_quoted(document, reference, &mut bytes)?;
    let parsed = jsonmax::parse::parse_payload(Payload::from_slice(&bytes), jsonmax::ParseOptions::default())?;
    let decoded = parsed.metadata().first().and_then(|meta| meta.string)
        .and_then(|string| parsed.resolve_str(string))
        .ok_or_else(|| encode_error(ErrorCode::InternalError, "escaped string failed to decode"))?;
    if decoded != expected { return Err(encode_error(ErrorCode::InternalError, "escaped string round trip changed its value")); }
    Ok(())
}

fn verify_formatted(document: &jsonmax::Document) -> Result<()> {
    let mut count = 0usize;
    for (index, node) in document.parts().nodes.iter().enumerate() {
        if node.kind() != jmax_dom::TAG_NUMBER { continue; }
        let span = *document.parts().spans.get(index).ok_or_else(|| encode_error(ErrorCode::InternalError, "number span is missing"))?;
        let lexeme = span.slice(document.payload().as_slice()).ok_or_else(|| encode_error(ErrorCode::InternalError, "number span is invalid"))?;
        let parsed = jsonmax::parse::parse_payload(Payload::from_slice(lexeme), jsonmax::ParseOptions::default())?;
        let reparsed_span = *parsed.parts().spans.first().ok_or_else(|| encode_error(ErrorCode::InternalError, "formatted number has no parsed span"))?;
        if reparsed_span.slice(parsed.payload().as_slice()) != Some(lexeme) {
            return Err(encode_error(ErrorCode::InternalError, "formatted number round trip changed its representation"));
        }
        count += 1;
    }
    if count == 0 { return Err(bench_error(ErrorCode::ConstraintViolation, "format workload contains no numbers")); }
    Ok(())
}

fn emit_ref_quoted(document: &jsonmax::Document, reference: StrRef, out: &mut Vec<u8>) -> Result<()> {
    let text = resolve_string(document, reference)?;
    let limits = writer_limits(out)?;
    let mut writer = JsonWriter::new(out, limits);
    writer.string(text)?;
    writer.finish().map(|_| ())
}

fn resolve_string<'a>(document: &'a jsonmax::Document, reference: StrRef) -> Result<&'a str> {
    let value = match reference.source {
        SpanSource::RawPayload => reference.span.slice(document.payload().as_slice()),
        SpanSource::DecodedArena => reference.span.slice(&document.parts().decoded),
    }.ok_or_else(|| encode_error(ErrorCode::InternalError, "string reference is outside its backing storage"))?;
    core::str::from_utf8(value).map_err(|_| encode_error(ErrorCode::InternalError, "prepared string is not UTF-8"))
}

fn container_for(document: &jsonmax::Document, node: NodeWord) -> Result<Container32> {
    document.parts().containers.get(node.payload_index() as usize).copied()
        .ok_or_else(|| encode_error(ErrorCode::InternalError, "container record is missing"))
}

fn writer_limits(out: &[u8]) -> Result<EncodeLimits> {
    let max_output_bytes = MAX_OUTPUT.checked_sub(out.len())
        .ok_or_else(|| encode_error(ErrorCode::PayloadTooLarge, "output byte limit exceeded"))?;
    // Prepared documents have already passed the parser's resource limits.
    // Preserve the benchmark's single cumulative output ceiling, including
    // strings emitted as separate roots by the escape operation.
    Ok(EncodeLimits {
        max_output_bytes,
        max_depth: u16::MAX,
        max_container_items: u32::MAX,
        max_string_bytes: u32::MAX,
        max_number_bytes: u32::MAX,
    })
}

fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let end = out.len().checked_add(bytes.len())
        .ok_or_else(|| encode_error(ErrorCode::PayloadTooLarge, "output length overflow"))?;
    if end > MAX_OUTPUT { return Err(encode_error(ErrorCode::PayloadTooLarge, "output byte limit exceeded")); }
    out.try_reserve(bytes.len()).map_err(|_| encode_error(ErrorCode::BudgetExhausted, "output allocation failed"))?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn retained_documents_bytes(documents: &[jsonmax::Document]) -> Result<u64> {
    let mut total = 0u64;
    for document in documents {
        let bytes = retained_document_bytes(document)?;
        total = total.checked_add(bytes)
            .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage overflow"))?;
    }
    Ok(total)
}

fn retained_document_bytes(document: &jsonmax::Document) -> Result<u64> {
    let mut total = u64::try_from(document.payload().len())
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "input retention size overflow"))?;
    add_capacity::<NodeWord>(&mut total, document.parts().nodes.capacity())?;
    add_capacity::<Span32>(&mut total, document.parts().spans.capacity())?;
    add_capacity::<Container32>(&mut total, document.parts().containers.capacity())?;
    add_usize(&mut total, document.parts().numbers.capacity().checked_mul(size_of::<jmax_abi::number::ConvertedNumber>())
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage overflow"))?)?;
    add_usize(&mut total, document.parts().decoded.capacity())?;
    add_capacity::<jmax_dom::NodeMeta>(&mut total, document.parts().node_metadata.capacity())?;
    Ok(total)
}

fn add_capacity<T>(total: &mut u64, capacity: usize) -> Result<()> {
    let size = capacity.checked_mul(size_of::<T>())
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage overflow"))?;
    add_usize(total, size)
}

fn add_usize(total: &mut u64, amount: usize) -> Result<()> {
    let amount = u64::try_from(amount)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "retained storage exceeds u64"))?;
    *total = total.checked_add(amount)
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage overflow"))?;
    Ok(())
}

fn encode_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}

pub fn run_serialize_cold(workload: &Workload, config: BenchmarkConfig,
    clock: &mut dyn Clock) -> Result<BenchmarkReport> {
    run_serialize_regime(workload, config, clock, false)
}
fn run_serialize_regime(workload: &Workload, config: BenchmarkConfig,
    clock: &mut dyn Clock, warm: bool) -> Result<BenchmarkReport> {
    prepare(workload, config)?;
    let documents = prepare_documents(workload)?;
    let retained = retained_documents_bytes(&documents)?;
    let mut operation = ReuseSerialize { documents, retained, next: 0, warm,
        output: jsonmax::OutputBuffer::new(MAX_OUTPUT), frames: Default::default() };
    for input in &workload.payloads {
        operation.execute(input)?;
        let index = (operation.next + workload.payloads.len() - 1) % workload.payloads.len();
        verify_serialized(&operation.documents[index])?;
        if warm {
            let expected = jsonmax::encode::document_to_vec(&operation.documents[index], Default::default())?;
            if expected != operation.output.as_bytes() {
                return Err(encode_error(ErrorCode::InternalError, "warm serialization differs from cold"));
            }
        }
    }
    measure(workload, &mut operation, config, clock)
}
struct ReuseSerialize {
    documents: Vec<jsonmax::Document>, retained: u64, next: usize, warm: bool,
    output: jsonmax::OutputBuffer, frames: jmax_encode::WriterScratch,
}
impl BenchmarkOperation for ReuseSerialize {
    fn name(&self) -> &str { if self.warm { "jsonmax.serialize.Warm-Scratch" } else { "jsonmax.serialize.Cold-Alloc" } }
    fn mode(&self) -> BenchmarkMode { BenchmarkMode::Serialize }
    fn execute(&mut self, _: &Payload) -> Result<Observation> {
        let document = &self.documents[self.next];
        let mut retained_bytes = self.retained;
        let len = if self.warm {
            jsonmax::encode::document_to_buffer(document, &mut self.output, &mut self.frames)?;
            add_usize(&mut retained_bytes, self.output.capacity())?;
            add_usize(&mut retained_bytes, self.frames.retained_bytes())?;
            core::hint::black_box(self.output.as_bytes()).len()
        } else {
            let bytes = jsonmax::encode::document_to_vec(document, Default::default())?;
            add_usize(&mut retained_bytes, bytes.capacity())?;
            core::hint::black_box(bytes).len()
        };
        self.next = (self.next + 1) % self.documents.len();
        Ok(Observation { digest: mix_digest(SERIALIZE_TAG, len as u64), output_bytes: len as u64, retained_bytes })
    }
}
