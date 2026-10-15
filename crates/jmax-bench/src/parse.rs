//! Fair parse, schema-validation, and selected-value extraction benchmarks.

use core::mem::size_of;

use jmax_abi::{
    error::ErrorCode,
    number::ConvertedNumber,
    schema::{FieldId, SlotValue},
    span::Span32,
};

use crate::{
    bench_error, corpus, digest_bytes, measure, mix_digest, BenchmarkConfig,
    BenchmarkMode, BenchmarkOperation, BenchmarkReport, Clock, Observation,
    Payload, Result, Workload,
};

const EMIT_SLOT_OPCODE: u8 = 13;
const ACCEPTED_SCHEMA_MARKER: u64 = 0x7661_6c69_6461_7465;

/// Measure strict native parsing of every payload in the workload.
pub fn run_parse(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let mut operation = ParseOperation;
    measure(workload, &mut operation, config, clock)
}

/// Measure complete schema validation of every payload in the workload.
pub fn run_validate(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let schema = compile_workload_schema(workload)?;
    let mut operation = SchemaOperation { schema, selection: None };
    measure(workload, &mut operation, config, clock)
}

/// Measure full schema validation followed by resolving the selected slots.
pub fn run_extract(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    if workload.selection.field_ids.is_empty() || !workload.selection.strict_validate_unselected {
        return Err(bench_error(
            ErrorCode::ConstraintViolation,
            "extraction requires a nonempty selection and strict validation",
        ));
    }
    let schema = compile_workload_schema(workload)?;
    let mut operation = SchemaOperation {
        schema,
        selection: Some(workload.selection.clone()),
    };
    measure(workload, &mut operation, config, clock)
}

/// Compare two operations after checking their per-input semantic results.
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
        || !matches!(mode, BenchmarkMode::Parse | BenchmarkMode::Validate | BenchmarkMode::Extract)
    {
        return Err(bench_error(
            ErrorCode::ConstraintViolation,
            "comparison requires matching parse, validate, or extract modes",
        ));
    }
    corpus::validate_workload(workload, config.max_total_input_bytes)?;

    // The preflight is deliberately outside timed samples. It establishes that
    // both implementations agree for every document before either report exists.
    for input in &workload.payloads {
        let left_result = left.execute(input)?;
        let right_result = right.execute(input)?;
        if left_result.digest != right_result.digest
            || left_result.output_bytes != right_result.output_bytes
        {
            return Err(bench_error(
                ErrorCode::ConstraintViolation,
                "comparison operations produced different semantic results",
            ));
        }
    }

    let left_report = measure(workload, left, config, clock)?;
    let right_report = measure(workload, right, config, clock)?;
    Ok((left_report, right_report))
}

struct ParseOperation;

impl BenchmarkOperation for ParseOperation {
    fn name(&self) -> &str { "jsonmax.parse" }

    fn mode(&self) -> BenchmarkMode { BenchmarkMode::Parse }

    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let document = jsonmax::parse::parse_payload(
            input.clone(),
            jsonmax::ParseOptions::default(),
        )?;
        let digest = digest_document(&document);
        let retained_bytes = document_retained_bytes(&document)?;
        Ok(Observation { digest, output_bytes: 0, retained_bytes })
    }
}

struct SchemaOperation {
    schema: jsonmax::CompiledSchema,
    selection: Option<jmax_abi::schema::Selection>,
}

impl BenchmarkOperation for SchemaOperation {
    fn name(&self) -> &str {
        if self.selection.is_some() { "jsonmax.extract" } else { "jsonmax.validate" }
    }

    fn mode(&self) -> BenchmarkMode {
        if self.selection.is_some() { BenchmarkMode::Extract } else { BenchmarkMode::Validate }
    }

    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let slots = jsonmax::endpoint::validate_payload(
            input.clone(),
            &self.schema,
            jsonmax::ParseOptions::default(),
        )?;
        let retained_bytes = slots_retained_bytes(&slots, &self.schema)?;
        let (digest, output_bytes) = match &self.selection {
            None => (digest_all_slots(&slots)?, 0),
            Some(selection) => digest_selected_slots(&slots, &self.schema, selection)?,
        };
        Ok(Observation { digest, output_bytes, retained_bytes })
    }
}

fn compile_workload_schema(workload: &Workload) -> Result<jsonmax::CompiledSchema> {
    let image = workload.schema.as_ref().ok_or_else(|| {
        bench_error(ErrorCode::ConstraintViolation, "schema benchmark requires an attached schema")
    })?;
    jsonmax::endpoint::load_schema(image.clone())
}

fn document_retained_bytes(document: &jsonmax::Document) -> Result<u64> {
    let mut total = u64::try_from(document.payload.len())
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "input retention size overflow"))?;
    add_capacity::<jmax_abi::NodeWord>(&mut total, document.parts.nodes.capacity())?;
    add_capacity::<Span32>(&mut total, document.parts.spans.capacity())?;
    add_capacity::<jmax_abi::Container32>(&mut total, document.parts.containers.capacity())?;
    add_capacity::<ConvertedNumber>(&mut total, document.parts.numbers.capacity())?;
    add_u64(&mut total, document.parts.decoded.capacity())?;
    Ok(total)
}

fn slots_retained_bytes(
    slots: &jmax_abi::schema::ValidatedSlots,
    schema: &jsonmax::CompiledSchema,
) -> Result<u64> {
    let mut total = u64::try_from(slots.payload.len())
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "input retention size overflow"))?;
    add_capacity::<SlotValue>(&mut total, slots.slots.capacity())?;
    add_u64(&mut total, slots.decoded.len())?;
    // The compiled schema stays live in the operation across invocations and is
    // therefore part of the per-invocation retained working set.
    let image = schema.image();
    add_capacity::<jmax_abi::schema::InsnWord>(&mut total, image.insns.capacity())?;
    add_capacity::<jmax_abi::schema::KeyEntry>(&mut total, image.keys.capacity())?;
    add_u64(&mut total, image.key_bytes.capacity())?;
    Ok(total)
}

fn add_capacity<T>(total: &mut u64, capacity: usize) -> Result<()> {
    let bytes = capacity.checked_mul(size_of::<T>())
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    add_u64(total, bytes)
}

fn add_u64(total: &mut u64, amount: usize) -> Result<()> {
    let amount = u64::try_from(amount)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    *total = total.checked_add(amount)
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    Ok(())
}

fn digest_document(document: &jsonmax::Document) -> u64 {
    let parts = &document.parts;
    let mut digest = mix_digest(0x7061_7273_652d_7631, digest_bytes(document.payload.as_slice()));
    for node in &parts.nodes {
        digest = mix_digest(digest, node.0);
    }
    for span in &parts.spans {
        digest = mix_digest(digest, u64::from(span.offset));
        digest = mix_digest(digest, u64::from(span.len));
    }
    for container in &parts.containers {
        digest = mix_digest(digest, u64::from(container.first_child));
        digest = mix_digest(digest, u64::from(container.child_count));
        digest = mix_digest(digest, u64::from(container.end_node));
        digest = mix_digest(digest, u64::from(container.reserved));
    }
    for number in &parts.numbers {
        digest = match number {
            ConvertedNumber::U64(value) => mix_digest(mix_digest(digest, 1), *value),
            ConvertedNumber::I64(value) => mix_digest(mix_digest(digest, 2), *value as u64),
            ConvertedNumber::F64Bits(value) => mix_digest(mix_digest(digest, 3), *value),
            ConvertedNumber::Decimal(value) => mix_digest(
                mix_digest(mix_digest(digest, 4), value.coefficient as u128 as u64),
                (value.coefficient as u128 >> 64) as u64 ^ u64::from(value.scale),
            ),
        };
    }
    mix_digest(digest, digest_bytes(&parts.decoded))
}

fn digest_all_slots(slots: &jmax_abi::schema::ValidatedSlots) -> Result<u64> {
    let mut digest = ACCEPTED_SCHEMA_MARKER;
    for (index, value) in slots.slots.iter().enumerate() {
        digest = mix_digest(digest, index as u64);
        digest = mix_slot_value(digest, value, &slots.payload, &slots.decoded)?.0;
    }
    Ok(digest)
}

fn digest_selected_slots(
    slots: &jmax_abi::schema::ValidatedSlots,
    schema: &jsonmax::CompiledSchema,
    selection: &jmax_abi::schema::Selection,
) -> Result<(u64, u64)> {
    if selection.field_ids.is_empty() || !selection.strict_validate_unselected {
        return Err(bench_error(
            ErrorCode::ConstraintViolation,
            "extraction requires a nonempty selection and strict validation",
        ));
    }
    let image = schema.image();
    let mut digest = mix_digest(0x6578_7472_6163_742d, ACCEPTED_SCHEMA_MARKER);
    let mut output_bytes = 0u64;
    for field in &selection.field_ids {
        let slot_index = slot_index_for_field(image, *field).ok_or_else(|| {
            bench_error(ErrorCode::SchemaImageInvalid, "selection field has no emitted slot")
        })?;
        let value = slots.slots.get(slot_index).ok_or_else(|| {
            bench_error(ErrorCode::SchemaImageInvalid, "emitted slot is outside validated results")
        })?;
        digest = mix_digest(digest, u64::from(field.0));
        let (next, encoded_len) = mix_slot_value(digest, value, &slots.payload, &slots.decoded)?;
        digest = next;
        output_bytes = output_bytes.checked_add(encoded_len)
            .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "extraction output size overflow"))?;
    }
    Ok((digest, output_bytes))
}

fn slot_index_for_field(image: &jmax_abi::schema::SchemaImage, field: FieldId) -> Option<usize> {
    image.insns.iter().find(|insn| {
        insn.opcode == EMIT_SLOT_OPCODE && u32::from(insn.field_id) == field.0
    }).and_then(|insn| usize::try_from(insn.arg0).ok())
}

/// Hash a slot's type and resolved content. The returned byte count uses a
/// stable tagged encoding: one type byte, then an eight-byte length for strings
/// and raw spans, an eight-byte scalar payload for bool/integer/float, and a
/// seventeen-byte payload for decimals.
fn mix_slot_value(
    mut digest: u64,
    value: &SlotValue,
    payload: &Payload,
    decoded: &[u8],
) -> Result<(u64, u64)> {
    let encoded_len = match value {
        SlotValue::Empty => {
            digest = mix_digest(digest, 0);
            1
        }
        SlotValue::Bool(value) => {
            digest = mix_digest(mix_digest(digest, 1), u64::from(*value));
            2
        }
        SlotValue::U64(value) => {
            digest = mix_digest(mix_digest(digest, 2), *value);
            9
        }
        SlotValue::I64(value) => {
            digest = mix_digest(mix_digest(digest, 3), *value as u64);
            9
        }
        SlotValue::F64Bits(value) => {
            digest = mix_digest(mix_digest(digest, 4), *value);
            9
        }
        SlotValue::Decimal(value) => {
            digest = mix_digest(digest, 5);
            digest = mix_digest(digest, value.coefficient as u128 as u64);
            digest = mix_digest(digest, (value.coefficient as u128 >> 64) as u64);
            digest = mix_digest(digest, u64::from(value.scale));
            18
        }
        SlotValue::Str(value) => {
            let text = jmax_schema_vm::resolve_str(payload.as_slice(), decoded, *value)?;
            let len = u64::try_from(text.len())
                .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "selected string size overflow"))?;
            digest = mix_digest(digest, 6);
            digest = mix_digest(digest, len);
            digest = mix_digest(digest, digest_bytes(text.as_bytes()));
            9u64.checked_add(len)
                .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "extraction output size overflow"))?
        }
        SlotValue::RawSpan(span) => {
            let bytes = span.slice(payload.as_slice()).ok_or_else(|| {
                bench_error(ErrorCode::SchemaImageInvalid, "selected raw span is outside payload")
            })?;
            let len = u64::try_from(bytes.len())
                .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "selected raw span size overflow"))?;
            digest = mix_digest(digest, 7);
            digest = mix_digest(digest, len);
            digest = mix_digest(digest, digest_bytes(bytes));
            9u64.checked_add(len)
                .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "extraction output size overflow"))?
        }
    };
    Ok((digest, encoded_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[derive(Default)]
    struct StepClock { now: u64, step: u64 }
    impl Clock for StepClock {
        fn now_ns(&mut self) -> u64 {
            let result = self.now;
            self.now = self.now.saturating_add(self.step);
            result
        }
    }

    fn config() -> BenchmarkConfig {
        BenchmarkConfig { warmup_iterations: 1, measured_iterations: 2, max_total_input_bytes: 4096 }
    }

    fn small_workload() -> Workload {
        corpus::make_workload(crate::WorkloadKind::SmallObject, crate::CorpusConfig {
            seed: 7, documents: 2, items_per_document: 2, max_bytes: 4096,
        }).expect("deterministic small workload")
    }

    #[test]
    fn parse_runner_reports_owned_dom_digest_and_timing() {
        let workload = small_workload();
        let mut clock = StepClock { now: 10, step: 3 };
        let report = run_parse(&workload, config(), &mut clock).unwrap();
        assert_eq!(report.operation, "jsonmax.parse");
        assert_eq!(report.mode, BenchmarkMode::Parse);
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.samples[0].elapsed_ns, 3);
        assert_eq!(report.samples[0].documents, 2);
        assert_eq!(report.samples[0].output_bytes, 0);
        assert!(report.samples[0].peak_retained_bytes >= workload.payloads[0].len() as u64);
        assert_eq!(report.samples[0].digest, report.samples[1].digest);
    }

    #[test]
    fn validate_and_extract_execute_schema_and_resolve_values() {
        let workload = small_workload();
        let mut validate_clock = StepClock { now: 0, step: 2 };
        let validated = run_validate(&workload, config(), &mut validate_clock).unwrap();
        assert_eq!(validated.operation, "jsonmax.validate");
        assert_eq!(validated.mode, BenchmarkMode::Validate);
        assert_ne!(validated.samples[0].digest, 0);
        assert_eq!(validated.samples[0].output_bytes, 0);

        let mut extract_clock = StepClock { now: 0, step: 2 };
        let extracted = run_extract(&workload, config(), &mut extract_clock).unwrap();
        assert_eq!(extracted.operation, "jsonmax.extract");
        assert_eq!(extracted.mode, BenchmarkMode::Extract);
        assert!(extracted.samples[0].output_bytes > 0);
        assert_ne!(extracted.samples[0].digest, validated.samples[0].digest);
    }

    #[test]
    fn schema_requirements_and_malformed_payload_errors_are_preserved() {
        let mut workload = small_workload();
        workload.schema = None;
        workload.selection.field_ids.clear();
        let mut clock = StepClock::default();
        assert_eq!(run_validate(&workload, config(), &mut clock).unwrap_err().code,
            ErrorCode::ConstraintViolation);
        assert_eq!(run_extract(&workload, config(), &mut clock).unwrap_err().code,
            ErrorCode::ConstraintViolation);

        let mut non_strict = small_workload();
        non_strict.selection.strict_validate_unselected = false;
        assert_eq!(run_extract(&non_strict, config(), &mut clock).unwrap_err().code,
            ErrorCode::ConstraintViolation);

        let too_small_budget = BenchmarkConfig {
            max_total_input_bytes: 1,
            ..config()
        };
        assert_eq!(run_parse(&small_workload(), too_small_budget, &mut clock).unwrap_err().code,
            ErrorCode::BudgetExhausted);

        let mut malformed = small_workload();
        malformed.payloads[0] = Payload::from_slice(b"{\"f0\":]");
        assert!(run_parse(&malformed, config(), &mut clock).is_err());
        assert!(run_validate(&malformed, config(), &mut clock).is_err());
    }

    struct FixedOperation {
        name: String,
        mode: BenchmarkMode,
        digest: u64,
    }
    impl BenchmarkOperation for FixedOperation {
        fn name(&self) -> &str { &self.name }
        fn mode(&self) -> BenchmarkMode { self.mode }
        fn execute(&mut self, input: &Payload) -> Result<Observation> {
            Ok(Observation { digest: self.digest ^ digest_bytes(input.as_slice()), ..Observation::default() })
        }
    }

    #[test]
    fn compare_preflights_semantics_and_rejects_mode_or_result_mismatch() {
        let workload = small_workload();
        let mut left = FixedOperation { name: String::from("left"), mode: BenchmarkMode::Parse, digest: 5 };
        let mut right = FixedOperation { name: String::from("right"), mode: BenchmarkMode::Parse, digest: 5 };
        let mut clock = StepClock { now: 0, step: 1 };
        let (a, b) = compare(&workload, &mut left, &mut right, config(), &mut clock).unwrap();
        assert_eq!(a.operation, "left");
        assert_eq!(b.operation, "right");
        assert_eq!(a.samples.len(), b.samples.len());

        let mut mismatch = FixedOperation { name: String::from("bad"), mode: BenchmarkMode::Parse, digest: 6 };
        assert_eq!(compare(&workload, &mut left, &mut mismatch, config(), &mut clock)
            .unwrap_err().code, ErrorCode::ConstraintViolation);
        mismatch.mode = BenchmarkMode::Validate;
        assert_eq!(compare(&workload, &mut left, &mut mismatch, config(), &mut clock)
            .unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn selected_slot_encoding_counts_lengths_and_hashes_value_types() {
        let payload = Payload::from_slice(b"value");
        let decoded = [];
        let string = SlotValue::Str(jmax_abi::span::StrRef {
            span: Span32::new(0, 5),
            source: jmax_abi::span::SpanSource::RawPayload,
            scalar_len: 5,
        });
        let (str_digest, str_len) = mix_slot_value(0, &string, &payload, &decoded).unwrap();
        let (bool_digest, bool_len) = mix_slot_value(0, &SlotValue::Bool(true), &payload, &decoded).unwrap();
        assert_eq!(str_len, 14);
        assert_eq!(bool_len, 2);
        assert_ne!(str_digest, bool_digest);
        assert_eq!(Span32::new(u32::MAX, 1).slice(payload.as_slice()), None);
    }

}
