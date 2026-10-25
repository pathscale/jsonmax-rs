//! Fair parse, schema-validation, and selected-value extraction benchmarks.

use core::mem::size_of;

use jmax_abi::{error::ErrorCode, schema::SlotValue};

use crate::{
    BenchmarkConfig, BenchmarkMode, BenchmarkOperation, BenchmarkReport, Clock, Observation,
    Payload, Result, Workload, bench_error, corpus, digest_bytes, measure, mix_digest,
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
        let document =
            jsonmax::parse::parse_payload(input.clone(), jsonmax::ParseOptions::default())?;
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

/// Measure a zero-copy borrowed DOM over each payload's existing byte slice.
pub fn run_parse_borrowed(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    run_borrowed(workload, config, clock, BenchmarkMode::Parse)
}
/// Measure strict borrowed syntax/schema validation and extraction variants.
pub fn run_validate_borrowed(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    run_borrowed(workload, config, clock, BenchmarkMode::Validate)
}
pub fn run_extract_borrowed(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    run_borrowed(workload, config, clock, BenchmarkMode::Extract)
}

fn run_borrowed(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
    mode: BenchmarkMode,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let mut operation = BorrowedOperation {
        workload,
        mode,
        dom: Default::default(),
        schema: Default::default(),
    };
    let mut verification = 0;
    for input in &workload.payloads {
        let observation = operation.execute(input)?;
        verification = mix_digest(verification, observation.digest);
    }
    Ok(with_verification(
        measure(workload, &mut operation, config, clock)?,
        verification,
    ))
}

struct BorrowedOperation<'a> {
    workload: &'a Workload,
    mode: BenchmarkMode,
    dom: jsonmax::DomScratch,
    schema: jsonmax::endpoint::ValidationScratch,
}
impl BenchmarkOperation for BorrowedOperation<'_> {
    fn name(&self) -> &str {
        "jsonmax.Borrowed (Zero-Copy Input)"
    }
    fn mode(&self) -> BenchmarkMode {
        self.mode
    }
    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let raw = input.as_slice();
        let options = jsonmax::ParseOptions::default();
        match self.mode {
            BenchmarkMode::Parse => {
                let document =
                    jsonmax::parse::parse_borrowed_with_scratch(raw, options, &mut self.dom)?;
                let digest = mix_digest(digest_bytes(document.raw()), document.node_count() as u64);
                let retained_bytes = u64::try_from(
                    document.tape().len() * size_of::<jmax_abi::node::TapeWord>()
                        + document.decoded().len(),
                )
                .map_err(|_| {
                    bench_error(ErrorCode::BudgetExhausted, "borrowed tape size overflow")
                })?;
                document.recycle(&mut self.dom);
                Ok(Observation {
                    digest,
                    output_bytes: 0,
                    retained_bytes,
                })
            }
            BenchmarkMode::Validate => {
                let proof = jsonmax::endpoint::validate_borrowed(
                    &self.workload.compiled_schema,
                    raw,
                    options,
                    &mut self.schema,
                )?;
                Ok(Observation {
                    digest: mix_digest(ACCEPTED_SCHEMA_MARKER, proof.schema_fingerprint()),
                    output_bytes: 0,
                    retained_bytes: schema_retained_bytes(&self.workload.compiled_schema)?,
                })
            }
            BenchmarkMode::Extract => {
                let slots = jsonmax::endpoint::extract_borrowed_with_scratch(
                    &self.workload.compiled_schema,
                    &self.workload.selection,
                    raw,
                    options,
                    &mut self.schema,
                )?;
                let digest = mix_digest(
                    mix_digest(digest_bytes(raw), slots.proof.schema_fingerprint()),
                    slots.slots.len() as u64,
                );
                Ok(Observation {
                    digest,
                    output_bytes: (slots.slots.len() * size_of::<SlotValue>()) as u64,
                    retained_bytes: (raw.len()
                        + slots.decoded.len()
                        + slots.slots.len() * size_of::<SlotValue>())
                        as u64,
                })
            }
            _ => Err(bench_error(
                ErrorCode::ConstraintViolation,
                "borrowed parse operation requires parse, validate, or extract mode",
            )),
        }
    }
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
        return Err(bench_error(
            ErrorCode::ConstraintViolation,
            "empty benchmark corpus",
        ));
    }
    let mode = left.mode();
    if mode != right.mode()
        || !matches!(
            mode,
            BenchmarkMode::Parse | BenchmarkMode::Validate | BenchmarkMode::Extract
        )
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
    fn name(&self) -> &str {
        "jsonmax.parse"
    }

    fn mode(&self) -> BenchmarkMode {
        BenchmarkMode::Parse
    }

    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let document =
            jsonmax::parse::parse_payload(input.clone(), jsonmax::ParseOptions::default())?;
        let digest = digest_document(&document);
        core::hint::black_box(&document);
        let retained_bytes = document_retained_bytes(&document)?;
        Ok(Observation {
            digest,
            output_bytes: 0,
            retained_bytes,
        })
    }
}

struct SchemaOperation<'a> {
    workload: &'a Workload,
    extract: bool,
}

impl SchemaOperation<'_> {
    fn execute_result(&self, input: &Payload) -> Result<SchemaResult> {
        let options = jsonmax::ParseOptions::default();
        if self.extract {
            jsonmax::endpoint::extract_payload(
                input.clone(),
                &self.workload.compiled_schema,
                &self.workload.selection,
                options,
            )
            .map(SchemaResult::Slots)
        } else {
            jsonmax::endpoint::validate_payload(
                input.clone(),
                &self.workload.compiled_schema,
                options,
            )
            .map(SchemaResult::Proof)
        }
    }
}

enum SchemaResult {
    Proof(jsonmax::ValidationProof),
    Slots(jmax_abi::schema::ValidatedSlots),
}

impl BenchmarkOperation for SchemaOperation<'_> {
    fn name(&self) -> &str {
        if self.extract {
            "jsonmax.extract"
        } else {
            "jsonmax.validate"
        }
    }
    fn mode(&self) -> BenchmarkMode {
        if self.extract {
            BenchmarkMode::Extract
        } else {
            BenchmarkMode::Validate
        }
    }
    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let result = self.execute_result(input)?;
        let (digest, retained_bytes, output_bytes) = match &result {
            SchemaResult::Proof(proof) => (
                mix_digest(ACCEPTED_SCHEMA_MARKER, proof.schema_fingerprint()),
                schema_retained_bytes(&self.workload.compiled_schema)?,
                0,
            ),
            SchemaResult::Slots(slots) => (
                mix_digest(
                    mix_digest(ACCEPTED_SCHEMA_MARKER, slots.proof.schema_fingerprint()),
                    slots.slots.len() as u64,
                ),
                slots_retained_bytes(slots, &self.workload.compiled_schema)?,
                u64::try_from(slots.slots.len())
                    .ok()
                    .and_then(|n| n.checked_mul(size_of::<SlotValue>() as u64))
                    .ok_or_else(|| {
                        bench_error(ErrorCode::BudgetExhausted, "slot output size overflow")
                    })?,
            ),
        };
        core::hint::black_box(&result);
        Ok(Observation {
            digest,
            output_bytes,
            retained_bytes,
        })
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
        match operation.execute_result(input)? {
            SchemaResult::Proof(proof) => {
                verification = mix_digest(verification, proof.schema_fingerprint())
            }
            SchemaResult::Slots(slots) => {
                verification = mix_digest(verification, digest_all_slots(&slots)?)
            }
        }
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
    add_capacity::<jmax_abi::node::TapeWord>(&mut total, document.tape().len())?;
    add_u64(&mut total, document.decoded().len())?;
    Ok(total)
}

fn schema_retained_bytes(schema: &jsonmax::CompiledSchema) -> Result<u64> {
    let image = schema.image();
    let mut total = 0;
    add_capacity::<jmax_abi::schema::InsnWord>(&mut total, image.insns.capacity())?;
    add_capacity::<jmax_abi::schema::KeyEntry>(&mut total, image.keys.capacity())?;
    add_u64(&mut total, image.key_bytes.capacity())?;
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
    total = total
        .checked_add(schema_retained_bytes(schema)?)
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    Ok(total)
}

fn add_capacity<T>(total: &mut u64, capacity: usize) -> Result<()> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    add_u64(total, bytes)
}

fn add_u64(total: &mut u64, amount: usize) -> Result<()> {
    let amount = u64::try_from(amount)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    *total = total
        .checked_add(amount)
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "retained storage size overflow"))?;
    Ok(())
}

fn digest_document(document: &jsonmax::Document) -> u64 {
    let mut digest = mix_digest(
        0x7061_7273_652d_7631,
        digest_bytes(document.payload().as_slice()),
    );
    digest = mix_digest(digest, document.node_count() as u64);
    digest = mix_digest(digest, document.container_count() as u64);
    digest = mix_digest(digest, u64::from(document.max_depth()));
    for word in document.tape() {
        digest = mix_digest(digest, word.0);
    }
    mix_digest(digest, digest_bytes(document.decoded()))
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
            let len = u64::try_from(text.len()).map_err(|_| {
                bench_error(ErrorCode::BudgetExhausted, "selected string size overflow")
            })?;
            digest = mix_digest(digest, 6);
            digest = mix_digest(digest, len);
            digest = mix_digest(digest, digest_bytes(text.as_bytes()));
            9u64.checked_add(len).ok_or_else(|| {
                bench_error(
                    ErrorCode::BudgetExhausted,
                    "extraction output size overflow",
                )
            })?
        }
        SlotValue::RawSpan(span) => {
            let bytes = span.slice(payload.as_slice()).ok_or_else(|| {
                bench_error(
                    ErrorCode::SchemaImageInvalid,
                    "selected raw span is outside payload",
                )
            })?;
            let len = u64::try_from(bytes.len()).map_err(|_| {
                bench_error(
                    ErrorCode::BudgetExhausted,
                    "selected raw span size overflow",
                )
            })?;
            digest = mix_digest(digest, 7);
            digest = mix_digest(digest, len);
            digest = mix_digest(digest, digest_bytes(bytes));
            9u64.checked_add(len).ok_or_else(|| {
                bench_error(
                    ErrorCode::BudgetExhausted,
                    "extraction output size overflow",
                )
            })?
        }
    };
    Ok((digest, encoded_len))
}

/// Prewarm every payload even when the requested warmup count is zero.
/// The measured path retains result tables and all cursor/VM scratch.
pub fn run_parse_warm(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let mut operation = WarmOperation::new(workload, BenchmarkMode::Parse);
    let mut verification = 0;
    for input in &workload.payloads {
        let doc = jsonmax::parse::parse_payload_with_scratch(
            input.clone(),
            jsonmax::ParseOptions::default(),
            &mut operation.dom,
        )?;
        let cold = jsonmax::parse::parse_payload(input.clone(), jsonmax::ParseOptions::default())?;
        let digest = digest_document(&doc);
        if digest != digest_document(&cold) {
            return Err(bench_error(
                ErrorCode::InternalError,
                "warm DOM differs from cold parse",
            ));
        }
        verification = mix_digest(verification, digest);
        doc.recycle(&mut operation.dom);
    }
    Ok(with_verification(
        measure(workload, &mut operation, config, clock)?,
        verification,
    ))
}
pub fn run_validate_warm(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    run_schema_warm(workload, config, clock, BenchmarkMode::Validate)
}
pub fn run_extract_warm(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
) -> Result<BenchmarkReport> {
    run_schema_warm(workload, config, clock, BenchmarkMode::Extract)
}
fn run_schema_warm(
    workload: &Workload,
    config: BenchmarkConfig,
    clock: &mut dyn Clock,
    mode: BenchmarkMode,
) -> Result<BenchmarkReport> {
    config.validate()?;
    corpus::validate_workload(workload, config.max_total_input_bytes)?;
    let mut operation = WarmOperation::new(workload, mode);
    let cold = SchemaOperation {
        workload,
        extract: mode == BenchmarkMode::Extract,
    };
    let mut verification = 0;
    // Both reorder buffers must reach the maximum size before measurement.
    for _ in 0..2 {
        for input in &workload.payloads {
            operation.execute(input)?;
            match (mode, cold.execute_result(input)?) {
                (BenchmarkMode::Validate, SchemaResult::Proof(expected)) => {
                    let actual = jsonmax::endpoint::validate_payload_with_scratch(
                        input.clone(),
                        &workload.compiled_schema,
                        jsonmax::ParseOptions::default(),
                        &mut operation.schema,
                    )?;
                    if actual != expected {
                        return Err(bench_error(
                            ErrorCode::InternalError,
                            "warm proof differs from cold validation",
                        ));
                    }
                    verification = mix_digest(verification, actual.schema_fingerprint());
                }
                (BenchmarkMode::Extract, SchemaResult::Slots(expected)) => {
                    let actual = operation.schema.slots().ok_or_else(|| {
                        bench_error(ErrorCode::InternalError, "warm schema result is missing")
                    })?;
                    if actual.slots != expected.slots
                        || actual.decoded != expected.decoded
                        || actual.proof != expected.proof
                        || actual.payload.as_slice() != expected.payload.as_slice()
                    {
                        return Err(bench_error(
                            ErrorCode::InternalError,
                            "warm slots differ from cold validation",
                        ));
                    }
                    verification = mix_digest(verification, digest_all_slots(actual)?);
                }
                _ => {
                    return Err(bench_error(
                        ErrorCode::InternalError,
                        "warm schema result kind differs from mode",
                    ));
                }
            }
        }
    }
    Ok(with_verification(
        measure(workload, &mut operation, config, clock)?,
        verification,
    ))
}
struct WarmOperation<'a> {
    workload: &'a Workload,
    mode: BenchmarkMode,
    dom: jsonmax::DomScratch,
    schema: jsonmax::endpoint::ValidationScratch,
}
impl<'a> WarmOperation<'a> {
    fn new(workload: &'a Workload, mode: BenchmarkMode) -> Self {
        Self {
            workload,
            mode,
            dom: Default::default(),
            schema: Default::default(),
        }
    }
}
impl BenchmarkOperation for WarmOperation<'_> {
    fn name(&self) -> &str {
        "jsonmax.Warm-Scratch (Zero-Alloc Reuse)"
    }
    fn mode(&self) -> BenchmarkMode {
        self.mode
    }
    fn execute(&mut self, input: &Payload) -> Result<Observation> {
        let options = jsonmax::ParseOptions::default();
        if self.mode == BenchmarkMode::Parse {
            let document =
                jsonmax::parse::parse_payload_with_scratch(input.clone(), options, &mut self.dom)?;
            let digest = digest_document(&document);
            let retained_bytes = document_retained_bytes(&document)?;
            core::hint::black_box(&document);
            document.recycle(&mut self.dom);
            return Ok(Observation {
                digest,
                output_bytes: 0,
                retained_bytes,
            });
        }
        let slots = if self.mode == BenchmarkMode::Extract {
            jsonmax::endpoint::extract_payload_with_scratch(
                input,
                &self.workload.compiled_schema,
                &self.workload.selection,
                options,
                &mut self.schema,
            )?
        } else {
            let proof = jsonmax::endpoint::validate_payload_with_scratch(
                input.clone(),
                &self.workload.compiled_schema,
                options,
                &mut self.schema,
            )?;
            core::hint::black_box(proof);
            return Ok(Observation {
                digest: mix_digest(ACCEPTED_SCHEMA_MARKER, proof.schema_fingerprint()),
                output_bytes: 0,
                retained_bytes: schema_retained_bytes(&self.workload.compiled_schema)?,
            });
        };
        core::hint::black_box(slots);
        Ok(Observation {
            digest: mix_digest(
                mix_digest(ACCEPTED_SCHEMA_MARKER, slots.proof.schema_fingerprint()),
                slots.slots.len() as u64,
            ),
            output_bytes: if self.mode == BenchmarkMode::Extract {
                (slots.slots.len() * size_of::<SlotValue>()) as u64
            } else {
                0
            },
            retained_bytes: slots_retained_bytes(slots, &self.workload.compiled_schema)?,
        })
    }
}
