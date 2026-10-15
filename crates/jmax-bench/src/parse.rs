//! Fair parse, schema-validation, and selected-value extraction benchmarks.

use core::mem::size_of;

use jmax_abi::{
    error::ErrorCode,
    number::ConvertedNumber,
    schema::SlotValue,
    span::Span32,
};

use crate::{
    bench_error, corpus, digest_bytes, measure, mix_digest, BenchmarkConfig,
    BenchmarkMode, BenchmarkOperation, BenchmarkReport, Clock, Observation,
    Payload, Result, Workload,
};

const ACCEPTED_SCHEMA_MARKER: u64 = 0x7661_6c69_6461_7465;

/// Measure strict native parsing of every payload in the workload.
pub fn run_parse(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let mut verification = 0;
    for input in &workload.payloads {
        let document = jsonmax::parse::parse_payload(input.clone(), jsonmax::ParseOptions::default())?;
        verification = mix_digest(verification, digest_document(&document));
    }
    let mut operation = ParseOperation;
    let report = measure(workload, &mut operation, config, clock)?;
    Ok(with_verification(report, verification))
}

/// Measure complete schema validation of every payload in the workload.
pub fn run_validate(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    run_schema(workload, config, clock, false)
}

/// Measure full schema validation followed by resolving the selected slots.
pub fn run_extract(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    run_schema(workload, config, clock, true)
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
        let parts = document.parts();
        let digest = mix_digest(
            mix_digest(parts.nodes.len() as u64, parts.containers.len() as u64),
            mix_digest(parts.decoded.len() as u64, document.proof().schema_fingerprint()),
        );
        core::hint::black_box(&document);
        let retained_bytes = document_retained_bytes(&document)?;
        Ok(Observation { digest, output_bytes: 0, retained_bytes })
    }
}

struct SchemaOperation<'a> {
    workload: &'a Workload,
    extract: bool,
}

impl SchemaOperation<'_> {
    fn execute_slots(&self, input: &Payload) -> Result<jmax_abi::schema::ValidatedSlots> {
        let options = jsonmax::ParseOptions::default();
        if self.extract {
            jsonmax::endpoint::extract_payload(
                input.clone(), &self.workload.compiled_schema, &self.workload.selection, options,
            )
        } else {
            jsonmax::endpoint::validate_payload(input.clone(), &self.workload.compiled_schema, options)
        }
    }
}

impl BenchmarkOperation for SchemaOperation<'_> {
    fn name(&self) -> &str {
        if self.extract { "jsonmax.extract" } else { "jsonmax.validate" }
    }
    fn mode(&self) -> BenchmarkMode {
        if self.extract { BenchmarkMode::Extract } else { BenchmarkMode::Validate }
    }
    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let slots = self.execute_slots(input)?;
        let digest = mix_digest(mix_digest(ACCEPTED_SCHEMA_MARKER,
            slots.proof.schema_fingerprint()), slots.slots.len() as u64);
        let retained_bytes = slots_retained_bytes(&slots, &self.workload.compiled_schema)?;
        // Consume the actual result without scanning its payload or tables.
        core::hint::black_box(&slots);
        let output_bytes = if self.extract {
            u64::try_from(slots.slots.len()).ok()
                .and_then(|n| n.checked_mul(size_of::<SlotValue>() as u64))
                .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "slot output size overflow"))?
        } else { 0 };
        Ok(Observation { digest, output_bytes, retained_bytes })
    }
}

fn run_schema(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
    extract: bool,
) -> Result<BenchmarkReport> {
    let mut operation = SchemaOperation { workload, extract };
    let mut verification = 0;
    for input in &workload.payloads {
        let slots = operation.execute_slots(input)?;
        verification = mix_digest(verification, digest_all_slots(&slots)?);
    }
    let report = measure(workload, &mut operation, config, clock)?;
    Ok(with_verification(report, verification))
}

fn with_verification(mut report: BenchmarkReport, verification: u64) -> BenchmarkReport {
    for sample in &mut report.samples {
        sample.digest = mix_digest(sample.digest, verification);
    }
    report
}

fn document_retained_bytes(document: &jsonmax::Document) -> Result<u64> {
    let mut total = u64::try_from(document.payload().len())
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "input retention size overflow"))?;
    add_capacity::<jmax_abi::NodeWord>(&mut total, document.parts().nodes.capacity())?;
    add_capacity::<Span32>(&mut total, document.parts().spans.capacity())?;
    add_capacity::<jmax_abi::Container32>(&mut total, document.parts().containers.capacity())?;
    add_capacity::<ConvertedNumber>(&mut total, document.parts().numbers.capacity())?;
    add_u64(&mut total, document.parts().decoded.capacity())?;
    add_capacity::<jmax_dom::NodeMeta>(&mut total, document.parts().node_metadata.capacity())?;
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
    let parts = document.parts();
    let mut digest = mix_digest(0x7061_7273_652d_7631, digest_bytes(document.payload().as_slice()));
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
