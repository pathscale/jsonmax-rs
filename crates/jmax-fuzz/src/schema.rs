//! Deterministic differential harness for compiled schema constraints.

use alloc::{string::String, vec::Vec};

use crate::{
    compare_outcomes, harness_error, Error, ErrorCode, ErrorSource, Failure, FailureKind,
    FieldId, FuzzConfig, FuzzReport, Limits, Outcome, Payload, Result, RejectionComparison,
    SchemaCase, SchemaOracle,
};
use jmax_abi::schema::{SchemaFieldIr, SchemaId, SchemaImage};
use jmax_abi::syntax::ScalarKind;

/// Build a deterministic set of real compiler-produced schema images and test payloads.
///
/// The emitted cases cover the supported flat scalar constraint surface. Every image is
/// produced by the schema compiler facade and checked again through the AOT image
/// projection validator before it is returned. Cases that do not fit the configured
/// payload budget are omitted intact; payloads are never truncated into new JSON.
pub fn constraint_corpus(config: FuzzConfig) -> Result<Vec<SchemaCase>> {
    config.validate()?;
    let mut cases = Vec::new();

    // Required and optional members, including a required/optional pair in one object.
    add_case(&mut cases, config, "required-present", b"{\"n\":1}",
        vec![field(1, "n", true, Some(ScalarKind::Number), None, None, None, None, None)],
        true, &[1])?;
    add_case(&mut cases, config, "required-missing", b"{}",
        vec![field(1, "n", true, Some(ScalarKind::Number), None, None, None, None, None)],
        true, &[1])?;
    add_case(&mut cases, config, "optional-absent", b"{}",
        vec![field(2, "s", false, Some(ScalarKind::String), None, None, None, None, None)],
        true, &[2])?;
    add_case(&mut cases, config, "required-and-optional", b"{\"n\":1,\"s\":\"ok\"}",
        vec![
            field(1, "n", true, Some(ScalarKind::Number), None, None, None, None, None),
            field(2, "s", false, Some(ScalarKind::String), None, None, None, None, None),
        ], true, &[1, 2])?;

    // Duplicate and unknown object properties have distinct reject paths.
    add_case(&mut cases, config, "duplicate-key", b"{\"n\":1,\"n\":2}",
        vec![field(1, "n", false, Some(ScalarKind::Number), None, None, None, None, None)],
        true, &[1])?;
    add_case(&mut cases, config, "unknown-key", b"{\"n\":1,\"x\":2}",
        vec![field(1, "n", false, Some(ScalarKind::Number), None, None, None, None, None)],
        true, &[1])?;

    // Scalar kind and both sides of inclusive integer bounds, with exact and
    // one-past-boundary values. The schema itself uses representable i64 limits.
    let integer = || field(3, "n", true, Some(ScalarKind::Number), None, None,
        Some(-10), Some(10), None);
    for (name, payload) in [
        ("integer-lower-bound", &b"{\"n\":-10}"[..]),
        ("integer-lower-one-past", &b"{\"n\":-11}"[..]),
        ("integer-upper-bound", &b"{\"n\":10}"[..]),
        ("integer-upper-one-past", &b"{\"n\":11}"[..]),
        ("integer-type-mismatch", &b"{\"n\":\"1\"}"[..]),
    ] {
        add_case(&mut cases, config, name, payload, vec![integer()], true, &[3])?;
    }

    // String bounds count decoded Unicode scalar values. Two and three are the
    // exact accepted endpoints; one and four exercise the adjacent failures.
    let bounded_string = || field(4, "s", true, Some(ScalarKind::String),
        Some(2), Some(3), None, None, None);
    for (name, payload) in [
        ("string-below-minimum", &b"{\"s\":\"a\"}"[..]),
        ("string-at-minimum", &b"{\"s\":\"ab\"}"[..]),
        ("string-at-maximum", &b"{\"s\":\"abc\"}"[..]),
        ("string-above-maximum", &b"{\"s\":\"abcd\"}"[..]),
        ("string-unicode-scalar-length", &b"{\"s\":\"\xc3\xa9\xf0\x9f\x99\x82\"}"[..]),
    ] {
        add_case(&mut cases, config, name, payload, vec![bounded_string()], true, &[4])?;
    }

    // These registry formats are supported by the compiler. Each format includes
    // a valid spelling and a nearby invalid spelling for differential checking.
    for (id, key, format, valid, invalid) in [
        (5, "uuid", "uuid", b"{\"uuid\":\"550e8400-e29b-41d4-a716-446655440000\"}".as_slice(), b"{\"uuid\":\"not-a-uuid\"}".as_slice()),
        (6, "ip", "ip", b"{\"ip\":\"192.0.2.1\"}".as_slice(), b"{\"ip\":\"999.0.2.1\"}".as_slice()),
        (7, "hex", "hex", b"{\"hex\":\"a0ff\"}".as_slice(), b"{\"hex\":\"agff\"}".as_slice()),
        (8, "b64", "base64", b"{\"b64\":\"YWJj\"}".as_slice(), b"{\"b64\":\"%%%\"}".as_slice()),
    ] {
        let mut format_field = field(id, key, true, Some(ScalarKind::String), None, None, None, None, None);
        format_field.format = Some(String::from(format));
        add_case(&mut cases, config, "supported-format-valid", valid,
            vec![format_field.clone()], true, &[id])?;
        add_case(&mut cases, config, "supported-format-invalid", invalid,
            vec![format_field], true, &[id])?;
    }

    // A selected field does not exempt an invalid unselected field from strict
    // validation. The selection deliberately contains only `n`.
    add_case(&mut cases, config, "strict-unselected-invalid", b"{\"n\":1,\"s\":1}",
        vec![
            field(9, "n", true, Some(ScalarKind::Number), None, None, None, None, None),
            field(10, "s", false, Some(ScalarKind::String), Some(2), Some(4), None, None, None),
        ], true, &[9])?;

    // Limits are applied after building the ordered candidate list, preserving
    // deterministic prefix semantics. A tiny byte budget can therefore produce
    // fewer vectors, but can never turn an oversized vector into a different one.
    let mut bounded = Vec::new();
    bounded.try_reserve_exact(cases.len().min(config.max_cases as usize)).map_err(|_| {
        harness_error(ErrorCode::BudgetExhausted, "schema corpus allocation failed")
    })?;
    for case in cases {
        if bounded.len() >= config.max_cases as usize { break; }
        if case.payload.len() <= config.max_input_bytes {
            bounded.push(case);
        }
    }
    if bounded.is_empty() {
        return Err(harness_error(ErrorCode::ConstraintViolation,
            "schema corpus budget cannot represent a JSON case"));
    }
    Ok(bounded)
}

/// Compare AOT and VM behavior over validated immutable schema cases.
pub fn run_schema(
    cases: &[SchemaCase],
    aot: &mut dyn SchemaOracle,
    vm: &mut dyn SchemaOracle,
    limits: Limits,
    config: FuzzConfig,
) -> Result<FuzzReport> {
    config.validate()?;
    if cases.is_empty() {
        return Err(harness_error(ErrorCode::ConstraintViolation, "schema run requires cases"));
    }

    // Validate the entire caller-provided list before any oracle interaction,
    // including cases beyond this run's max_cases execution budget.
    for case in cases {
        if case.payload.len() > config.max_input_bytes {
            return Err(harness_error(ErrorCode::PayloadTooLarge, "schema payload exceeds byte cap"));
        }
        validate_image_and_selection(&case.schema, &case.selection)?;
    }

    let target = cases.len().min(config.max_cases as usize);
    let mut report = FuzzReport::default();
    for (index, case) in cases.iter().take(target).enumerate() {
        if report.failures.len() >= config.max_failures as usize {
            report.truncated = true;
            break;
        }
        let left = aot.observe(&case.payload, &case.schema, &case.selection, limits)?;
        let right = vm.observe(&case.payload, &case.schema, &case.selection, limits)?;
        report.attempted += 1;
        match compare_outcomes(&left, &right, config.rejection_comparison) {
            None => report.agreed += 1,
            Some(kind) => {
                let mut input = Vec::new();
                input.try_reserve_exact(case.payload.len()).map_err(|_| {
                    harness_error(ErrorCode::BudgetExhausted, "schema failure input allocation failed")
                })?;
                input.extend_from_slice(case.payload.as_slice());
                let mut detail = String::new();
                detail.try_reserve_exact(512).map_err(|_| {
                    harness_error(ErrorCode::BudgetExhausted, "schema diagnostic allocation failed")
                })?;
                detail.push_str("schema oracle mismatch: ");
                for ch in aot.name().chars().take(48) { detail.push(ch); }
                detail.push_str(" vs ");
                for ch in vm.name().chars().take(48) { detail.push(ch); }
                report.failures.try_reserve(1).map_err(|_| {
                    harness_error(ErrorCode::BudgetExhausted, "schema report allocation failed")
                })?;
                let recorded = report.record_failure(Failure {
                    case_index: index as u64,
                    seed: config.seed,
                    kind,
                    input,
                    detail,
                }, config.max_failures);
                if !recorded { break; }
            }
        }
    }
    if report.attempted == 0 {
        return Err(harness_error(ErrorCode::ConstraintViolation, "schema run attempted no cases"));
    }
    Ok(report)
}

fn validate_image_and_selection(image: &SchemaImage, selection: &jmax_abi::schema::Selection) -> Result<()> {
    // to_owned is the public AOT projection validator: it checks image metadata,
    // counts, spans and keys without issuing a proof. Compiler-produced corpus
    // images retain the fingerprint from their ValidatedProgram unchanged.
    jmax_schema_aot::to_owned(jmax_schema_aot::StaticSchemaImage::from(image))
        .map_err(|_| schema_image_error(image.header.schema_id, "schema image failed AOT validation"))?;

    if selection.field_ids.len() > image.header.slot_count as usize {
        return Err(schema_image_error(image.header.schema_id, "selection exceeds schema slot count"));
    }
    for (index, field_id) in selection.field_ids.iter().enumerate() {
        if selection.field_ids[..index].contains(field_id) {
            return Err(schema_image_error(image.header.schema_id, "selection contains duplicate field ids"));
        }
        let found = image.keys.iter().any(|key| u32::from(key.field_id) == field_id.0);
        if !found {
            return Err(schema_image_error(image.header.schema_id, "selection field id is outside the schema"));
        }
    }
    // strict_validate_unselected is intentionally preserved as supplied. It is
    // a requested validation mode, not permission for the harness to strengthen it.
    Ok(())
}

fn schema_image_error(schema_id: u32, detail: &'static str) -> Error {
    Error::new(ErrorCode::SchemaImageInvalid, ErrorSource::Decode, 0, detail)
        .with_schema(schema_id, 0)
}

fn field(
    field_id: u32,
    name: &str,
    required: bool,
    scalar_kind: Option<ScalarKind>,
    min_len: Option<u32>,
    max_len: Option<u32>,
    min_i64: Option<i64>,
    max_i64: Option<i64>,
    format: Option<&str>,
) -> SchemaFieldIr {
    SchemaFieldIr {
        field_id: FieldId(field_id),
        name: String::from(name),
        required,
        scalar_kind,
        format: format.map(String::from),
        min_len,
        max_len,
        min_i64,
        max_i64,
    }
}

fn add_case(
    cases: &mut Vec<SchemaCase>,
    config: FuzzConfig,
    name: &'static str,
    payload: &[u8],
    fields: Vec<SchemaFieldIr>,
    reject_unknown: bool,
    selected_ids: &[u32],
) -> Result<()> {
    if payload.len() > config.max_input_bytes {
        return Ok(());
    }
    let required_count = u16::try_from(fields.iter().filter(|field| field.required).count())
        .map_err(|_| harness_error(ErrorCode::ConstraintViolation, "schema has too many required fields"))?;
    let ir = jmax_abi::schema::SchemaIr {
        name: String::from(name),
        schema_id: SchemaId(cases.len() as u32 + 1),
        required_count,
        reject_unknown,
        fields,
    };
    // Unsupported formats and constraints propagate as UnsupportedSchemaFeature;
    // they are never silently discarded from a corpus advertised as complete.
    let program = jmax_schema_aot::compile_schema(&ir)?;
    let image = program.image().clone();
    validate_image_and_selection(&image, &jmax_abi::schema::Selection {
        field_ids: selected_ids.iter().copied().map(FieldId).collect(),
        strict_validate_unselected: true,
    })?;
    let mut owned_name = String::new();
    owned_name.try_reserve_exact(name.len()).map_err(|_| {
        harness_error(ErrorCode::BudgetExhausted, "schema case name allocation failed")
    })?;
    owned_name.push_str(name);
    cases.try_reserve(1).map_err(|_| {
        harness_error(ErrorCode::BudgetExhausted, "schema corpus allocation failed")
    })?;
    cases.push(SchemaCase {
        name: owned_name,
        payload: Payload::from_slice(payload),
        schema: image,
        selection: jmax_abi::schema::Selection {
            field_ids: selected_ids.iter().copied().map(FieldId).collect(),
            strict_validate_unselected: true,
        },
    });
    Ok(())
}
