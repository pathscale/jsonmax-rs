//! Bounded serialization, string escaping, and numeric formatting benchmarks.

use alloc::vec::Vec;
use core::mem::size_of;

use jmax_abi::{
    error::{Error, ErrorCode, ErrorSource},
    node::{Container32, NodeWord},
    span::{Span32, SpanSource, StrRef},
};

use crate::{
    bench_error, corpus, digest_bytes, measure, mix_digest, BenchmarkConfig,
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
    prepare(workload, config)?;
    let documents = prepare_documents(workload)?;
    for document in &documents { verify_serialized(document)?; }
    let mut operation = PreparedOperation::new("jsonmax.serialize", BenchmarkMode::Serialize, documents);
    measure(workload, &mut operation, config, clock)
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
    let mut operation = PreparedOperation::new("jsonmax.escape", BenchmarkMode::Escape, documents);
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
    let mut operation = PreparedOperation::new("jsonmax.format", BenchmarkMode::Format, documents);
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
}

impl PreparedOperation {
    fn new(name: &'static str, mode: BenchmarkMode, documents: Vec<jsonmax::Document>) -> Self {
        Self { name, mode, documents, next_document: 0, sink: Vec::new() }
    }

    fn execute_document(&mut self, document: &jsonmax::Document) -> Result<Observation> {
        self.sink.clear();
        match self.mode {
            BenchmarkMode::Serialize => emit_document(document, &mut self.sink)?,
            BenchmarkMode::Escape => emit_escaped(document, &mut self.sink)?,
            BenchmarkMode::Format => emit_formatted(document, &mut self.sink)?,
            _ => return Err(bench_error(ErrorCode::InternalError, "invalid prepared output mode")),
        }
        let output_bytes = u64::try_from(self.sink.len())
            .map_err(|_| encode_error(ErrorCode::PayloadTooLarge, "output length exceeds u64"))?;
        let digest = mix_digest(mode_tag(self.mode), digest_bytes(&self.sink));
        let retained_bytes = retained_documents_bytes(&self.documents)?
            .checked_add(u64::try_from(self.sink.capacity()).map_err(|_| {
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
        let observation = self.execute_document(&self.documents[index])?;
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
    emit_node(document, 0, out)?;
    Ok(())
}

fn emit_node(document: &jsonmax::Document, index: usize, out: &mut Vec<u8>) -> Result<usize> {
    let node = *document.parts().nodes.get(index).ok_or_else(|| {
        encode_error(ErrorCode::InternalError, "DOM node index is outside prepared tables")
    })?;
    let metadata = *document.metadata().get(index).ok_or_else(|| {
        encode_error(ErrorCode::InternalError, "DOM metadata index is outside prepared tables")
    })?;
    match node.kind() {
        jmax_dom::TAG_NULL => append(out, b"null")?,
        jmax_dom::TAG_FALSE => append(out, b"false")?,
        jmax_dom::TAG_TRUE => append(out, b"true")?,
        jmax_dom::TAG_NUMBER => {
            let span = *document.parts().spans.get(index).ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "number node has no span")
            })?;
            let number = span.slice(document.payload().as_slice()).ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "number span is outside payload")
            })?;
            append(out, number)?;
        }
        jmax_dom::TAG_STRING => {
            let text = resolve_string(document, metadata.string.ok_or_else(|| {
                encode_error(ErrorCode::InternalError, "string node has no decoded reference")
            })?)?;
            append_quoted(out, text.as_bytes())?;
        }
        jmax_dom::TAG_ARRAY | jmax_dom::TAG_OBJECT => {
            let container = container_for(document, node)?;
            let object = node.kind() == jmax_dom::TAG_OBJECT;
            append(out, if object { b"{" } else { b"[" })?;
            let mut child = if container.child_count == 0 { None } else { Some(container.first_child as usize) };
            for child_index in 0..container.child_count {
                let at = child.ok_or_else(|| encode_error(ErrorCode::InternalError, "container child link ended early"))?;
                if child_index != 0 { append(out, b",")?; }
                let child_meta = *document.metadata().get(at).ok_or_else(|| encode_error(ErrorCode::InternalError, "child metadata is missing"))?;
                if object {
                    let key = resolve_string(document, child_meta.key.ok_or_else(|| encode_error(ErrorCode::InternalError, "object member has no key"))?)?;
                    append_quoted(out, key.as_bytes())?;
                    append(out, b":")?;
                }
                let end = emit_node(document, at, out)?;
                child = document.metadata().get(at).and_then(|meta| meta.next_sibling).map(|id| id.0 as usize);
                if end <= at { return Err(encode_error(ErrorCode::InternalError, "child subtree has invalid extent")); }
            }
            if child.is_some() { return Err(encode_error(ErrorCode::InternalError, "container has excess child links")); }
            append(out, if object { b"}" } else { b"]" })?;
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
    append_quoted(out, text.as_bytes())
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

fn append_quoted(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    append(out, b"\"")?;
    for &byte in bytes {
        match byte {
            b'"' => append(out, b"\\\"")?,
            b'\\' => append(out, b"\\\\")?,
            0x08 => append(out, b"\\b")?,
            0x0c => append(out, b"\\f")?,
            b'\n' => append(out, b"\\n")?,
            b'\r' => append(out, b"\\r")?,
            b'\t' => append(out, b"\\t")?,
            0..=0x1f => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                append(out, b"\\u00")?;
                append(out, &[HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]])?;
            }
            _ => append(out, &[byte])?,
        }
    }
    append(out, b"\"")
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
    add_capacity::<jmax_dom::NodeMeta>(&mut total, document.metadata().len())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use crate::{CorpusConfig, WorkloadKind};

    #[derive(Default)]
    struct TickClock { now: u64, step: u64 }
    impl Clock for TickClock {
        fn now_ns(&mut self) -> u64 { let value = self.now; self.now += self.step; value }
    }

    fn config() -> BenchmarkConfig {
        BenchmarkConfig { warmup_iterations: 1, measured_iterations: 2, max_total_input_bytes: 32 * 1024 }
    }

    fn workload(kind: WorkloadKind) -> Workload {
        corpus::make_workload(kind, CorpusConfig { seed: 9, documents: 2, items_per_document: 8, max_bytes: 32 * 1024 }).unwrap()
    }

    #[test]
    fn serialization_rebuilds_compact_json_from_prepared_dom() {
        let input = Payload::from_slice(br#" { "a" : [true, 12, "x\n"], "b": {} } "#);
        let doc = jsonmax::parse::parse_payload(input, jsonmax::ParseOptions::default()).unwrap();
        let mut out = Vec::new();
        emit_document(&doc, &mut out).unwrap();
        assert_eq!(out, br#"{"a":[true,12,"x\n"],"b":{}}"#);
        assert_ne!(out, doc.payload().as_slice());
        verify_serialized(&doc).unwrap();
    }

    #[test]
    fn escape_covers_names_values_controls_unicode_and_empty_strings() {
        let doc = jsonmax::parse::parse_payload(Payload::from_slice(br#"{"\u0001":"a\tb","é":"😀","":""}"#), jsonmax::ParseOptions::default()).unwrap();
        let mut out = Vec::new();
        emit_escaped(&doc, &mut out).unwrap();
        assert_eq!(out, b"\"\\u0001\"\"a\\tb\"\"é\"\"😀\"\"\"\"\"");
        verify_escaped(&doc).unwrap();
        let empty = jsonmax::parse::parse_payload(Payload::from_slice(b"[]"), jsonmax::ParseOptions::default()).unwrap();
        assert_eq!(emit_escaped(&empty, &mut Vec::new()).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn format_preserves_large_and_fixed_point_lexemes_without_float_conversion() {
        let doc = jsonmax::parse::parse_payload(Payload::from_slice(b"[18446744073709551615,-0.125,1.2300e+0042]"), jsonmax::ParseOptions::default()).unwrap();
        let mut out = Vec::new();
        emit_formatted(&doc, &mut out).unwrap();
        assert_eq!(out, b"18446744073709551615\n-0.125\n1.2300e+0042");
        verify_formatted(&doc).unwrap();
        let no_numbers = jsonmax::parse::parse_payload(Payload::from_slice(b"{\"s\":\"v\"}"), jsonmax::ParseOptions::default()).unwrap();
        assert_eq!(emit_formatted(&no_numbers, &mut Vec::new()).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn output_limit_is_inclusive_and_encode_errors_keep_source() {
        let mut bytes = Vec::new();
        assert!(append(&mut bytes, &vec![b'x'; MAX_OUTPUT],).is_ok());
        let error = append(&mut bytes, b"x").unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Encode);
    }

    #[test]
    fn runners_prepare_once_then_measure_and_reject_missing_microbench_values() {
        let text = workload(WorkloadKind::EscapedStrings);
        let mut clock = TickClock { now: 1, step: 4 };
        let escaped = run_escape(&text, config(), &mut clock).unwrap();
        assert_eq!(escaped.operation, "jsonmax.escape");
        assert_eq!(escaped.mode, BenchmarkMode::Escape);
        assert_eq!(escaped.samples.len(), 2);
        assert!(escaped.samples[0].output_bytes > 0);

        let numbers = workload(WorkloadKind::NumberArray);
        let formatted = run_format(&numbers, config(), &mut clock).unwrap();
        assert_eq!(formatted.operation, "jsonmax.format");
        assert!(formatted.samples[0].output_bytes > 0);
        assert_eq!(run_format(&text, config(), &mut clock).unwrap_err().code, ErrorCode::ConstraintViolation);
        assert_eq!(run_escape(&numbers, config(), &mut clock).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn malformed_inputs_and_budget_errors_are_not_hidden() {
        let mut bad = workload(WorkloadKind::SmallObject);
        bad.payloads[0] = Payload::from_slice(b"{\"x\":]");
        let mut clock = TickClock::default();
        assert!(run_serialize(&bad, config(), &mut clock).is_err());
        let too_small = BenchmarkConfig { max_total_input_bytes: 1, ..config() };
        assert_eq!(run_serialize(&workload(WorkloadKind::SmallObject), too_small, &mut clock).unwrap_err().code, ErrorCode::BudgetExhausted);
    }
}
