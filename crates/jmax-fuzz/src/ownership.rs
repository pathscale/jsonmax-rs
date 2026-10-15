//! Stress ownership boundaries, span resolution, and retained storage.

use alloc::{string::String, vec::Vec};

use crate::{
    harness_error, Error, ErrorCode, ErrorSource, Failure, FailureKind,
    FuzzConfig, FuzzReport, OwnershipConfig, OwnershipProbe, OwnershipSnapshot, Payload, Result,
    Span32, SpanSource,
};

/// Validate every reference and the minimum storage accounting in a snapshot.
pub fn validate_snapshot(snapshot: &OwnershipSnapshot, max_retained_bytes: u64) -> Result<()> {
    let payload = snapshot.payload.as_slice();
    let decoded = snapshot.decoded.as_slice();

    for string in &snapshot.strings {
        let owner = match string.source {
            SpanSource::RawPayload => payload,
            SpanSource::DecodedArena => decoded,
        };
        let bytes = string.span.slice(owner).ok_or_else(|| Error::new(
            ErrorCode::InternalError,
            ErrorSource::Decode,
            string.span.offset,
            "ownership string span is outside its source buffer",
        ))?;
        let text = core::str::from_utf8(bytes).map_err(|_| Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            string.span.offset,
            "ownership string span is not UTF-8",
        ))?;
        let scalar_len = u32::try_from(text.chars().count()).map_err(|_| harness_error(
            ErrorCode::ConstraintViolation,
            "ownership string scalar count is not representable",
        ))?;
        if scalar_len != string.scalar_len {
            return Err(Error::new(
                ErrorCode::ConstraintViolation,
                ErrorSource::Decode,
                string.span.offset,
                "ownership string scalar count does not match its bytes",
            ));
        }
    }

    for span in &snapshot.raw_spans {
        span.slice(payload).ok_or_else(|| Error::new(
            ErrorCode::InternalError,
            ErrorSource::Decode,
            span.offset,
            "ownership raw span is outside the payload",
        ))?;
    }

    let payload_storage = match &snapshot.payload {
        Payload::Shared(bytes) => bytes.len(),
        Payload::Bytes(bytes) => bytes.capacity(),
    };
    let mut minimum = u64::try_from(core::cmp::max(payload.len(), payload_storage)).map_err(|_| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership payload length cannot be accounted",
    ))?;
    minimum = add_retained(minimum, decoded.capacity(), 1)?;
    minimum = add_retained(minimum, snapshot.strings.capacity(), core::mem::size_of::<crate::StrRef>())?;
    minimum = add_retained(minimum, snapshot.raw_spans.capacity(), core::mem::size_of::<Span32>())?;
    if snapshot.retained_bytes < minimum || snapshot.retained_bytes > max_retained_bytes {
        return Err(harness_error(ErrorCode::BudgetExhausted, "ownership retention accounting is outside budget"));
    }
    Ok(())
}

fn add_retained(total: u64, count: usize, element_size: usize) -> Result<u64> {
    let bytes = count.checked_mul(element_size).ok_or_else(|| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership retained storage size overflow",
    ))?;
    total.checked_add(u64::try_from(bytes).map_err(|_| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership retained storage is not representable",
    ))?).ok_or_else(|| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership retained byte count overflow",
    ))
}

/// Run an ownership probe across deterministic input offsets and sentinel tails.
pub fn run_ownership(
    inputs: &[Payload],
    probe: &mut dyn OwnershipProbe,
    ownership: OwnershipConfig,
    config: FuzzConfig,
) -> Result<FuzzReport> {
    config.validate()?;
    if inputs.is_empty() || ownership.max_retained_bytes == 0
        || !(1..=64).contains(&ownership.alignments)
    {
        return Err(harness_error(ErrorCode::ConstraintViolation, "invalid ownership run configuration"));
    }

    let parse_options = jsonmax::ParseOptions { limits: ownership.limits, numeric: crate::NumericPolicy::strict_default() };
    for input in inputs {
        if input.len() > config.max_input_bytes {
            return Err(harness_error(ErrorCode::PayloadTooLarge, "ownership input exceeds byte cap"));
        }
        jsonmax::parse::validate_syntax(input.as_slice(), parse_options)?;
    }

    let available = inputs.len().checked_mul(ownership.alignments as usize).ok_or_else(|| {
        harness_error(ErrorCode::ConstraintViolation, "ownership case count overflow")
    })?;
    let target = core::cmp::min(available, config.max_cases as usize);
    let mut report = FuzzReport::default();
    let mut case_index = 0usize;
    let mut parity: Option<Vec<Vec<u8>>> = None;

    'inputs: for input in inputs {
        parity = None;
        for alignment in 0..ownership.alignments {
            if case_index >= target { break 'inputs; }
            if report.failures.len() >= config.max_failures as usize {
                report.truncated = true;
                break 'inputs;
            }

            let offset = alignment as usize;
            let input_bytes = input.as_slice();
            let total = offset.checked_add(input_bytes.len()).and_then(|v| v.checked_add(64)).ok_or_else(|| {
                harness_error(ErrorCode::BudgetExhausted, "ownership backing size overflow")
            })?;
            let mut backing = Vec::new();
            backing.try_reserve_exact(total).map_err(|_| harness_error(
                ErrorCode::BudgetExhausted,
                "ownership backing allocation failed",
            ))?;
            for index in 0..offset {
                backing.push(sentinel(index, case_index));
            }
            let start = backing.len();
            backing.extend_from_slice(input_bytes);
            let end = backing.len();
            for index in 0..64 {
                backing.push(sentinel(index.wrapping_add(offset), case_index));
            }

            let inspected = probe.inspect(&backing[start..end], ownership.limits);
            drop(backing);
            report.attempted += 1;
            let tested_index = case_index;
            case_index += 1;

            let snapshot = match inspected {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let kind = if error.code == ErrorCode::BudgetExhausted { FailureKind::Retention } else { FailureKind::Ownership };
                    record_ownership_failure(&mut report, config, tested_index, input_bytes, kind, error.detail)?;
                    continue;
                }
            };

            if snapshot.payload.as_slice() != input_bytes {
                record_ownership_failure(
                    &mut report, config, tested_index, input_bytes, FailureKind::Ownership,
                    "probe snapshot payload differs from tested input",
                )?;
                continue;
            }
            if snapshot.strings.len() != json_string_count(input_bytes) {
                record_ownership_failure(
                    &mut report, config, tested_index, input_bytes, FailureKind::Ownership,
                    "snapshot string references do not cover every JSON string token",
                )?;
                continue;
            }
            if let Err(error) = validate_snapshot(&snapshot, ownership.max_retained_bytes) {
                let kind = if error.code == ErrorCode::BudgetExhausted { FailureKind::Retention } else { FailureKind::Ownership };
                record_ownership_failure(&mut report, config, tested_index, input_bytes, kind, error.detail)?;
                continue;
            }

            let resolved = resolve_contents(&snapshot)?;
            if let Some(expected) = &parity {
                if expected != &resolved {
                    record_ownership_failure(
                        &mut report, config, tested_index, input_bytes, FailureKind::Ownership,
                        "resolved references differ across input alignments",
                    )?;
                    continue;
                }
            } else {
                parity = Some(resolved);
            }
            report.agreed += 1;
        }
    }
    if report.failures.len() >= config.max_failures as usize && case_index < available.min(config.max_cases as usize) {
        report.truncated = true;
    }
    if report.attempted == 0 {
        return Err(harness_error(ErrorCode::ConstraintViolation, "ownership run attempted no cases"));
    }
    Ok(report)
}

fn json_string_count(input: &[u8]) -> usize {
    let mut count = 0usize;
    let mut index = 0usize;
    while index < input.len() {
        if input[index] != b'"' {
            index += 1;
            continue;
        }
        count += 1;
        index += 1;
        while index < input.len() {
            match input[index] {
                b'\\' => index = index.saturating_add(2),
                b'"' => { index += 1; break; }
                _ => index += 1,
            }
        }
    }
    count
}

fn sentinel(index: usize, case_index: usize) -> u8 {
    const VALUES: [u8; 16] = [0xff, 0xc0, b'{', b'}', b'[', b']', b'"', b'\\', b',', b':', 0, 0x80, b'/', b' ', b'9', b'\n'];
    VALUES[index.wrapping_add(case_index) % VALUES.len()]
}

fn resolve_contents(snapshot: &OwnershipSnapshot) -> Result<Vec<Vec<u8>>> {
    let count = snapshot.strings.len().checked_add(snapshot.raw_spans.len()).ok_or_else(|| {
        harness_error(ErrorCode::BudgetExhausted, "ownership reference count overflow")
    })?;
    let mut contents = Vec::new();
    contents.try_reserve_exact(count).map_err(|_| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership resolved-content allocation failed",
    ))?;
    for string in &snapshot.strings {
        let owner = match string.source {
            SpanSource::RawPayload => snapshot.payload.as_slice(),
            SpanSource::DecodedArena => snapshot.decoded.as_slice(),
        };
        let bytes = string.span.slice(owner).ok_or_else(|| harness_error(
            ErrorCode::InternalError,
            "validated ownership string became unresolved",
        ))?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(bytes.len()).map_err(|_| harness_error(
            ErrorCode::BudgetExhausted,
            "ownership string copy allocation failed",
        ))?;
        copy.extend_from_slice(bytes);
        contents.push(copy);
    }
    for span in &snapshot.raw_spans {
        let bytes = span.slice(snapshot.payload.as_slice()).ok_or_else(|| harness_error(
            ErrorCode::InternalError,
            "validated ownership raw span became unresolved",
        ))?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(bytes.len()).map_err(|_| harness_error(
            ErrorCode::BudgetExhausted,
            "ownership raw-span copy allocation failed",
        ))?;
        copy.extend_from_slice(bytes);
        contents.push(copy);
    }
    Ok(contents)
}

fn record_ownership_failure(
    report: &mut FuzzReport,
    config: FuzzConfig,
    case_index: usize,
    input: &[u8],
    kind: FailureKind,
    detail: &'static str,
) -> Result<()> {
    let mut copied_input = Vec::new();
    copied_input.try_reserve_exact(input.len()).map_err(|_| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership failure input allocation failed",
    ))?;
    copied_input.extend_from_slice(input);
    let mut message = String::new();
    message.try_reserve_exact(detail.len()).map_err(|_| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership failure detail allocation failed",
    ))?;
    message.push_str(detail);
    report.failures.try_reserve(1).map_err(|_| harness_error(
        ErrorCode::BudgetExhausted,
        "ownership report allocation failed",
    ))?;
    report.record_failure(Failure {
        case_index: case_index as u64,
        seed: config.seed,
        kind,
        input: copied_input,
        detail: message,
    }, config.max_failures);
    Ok(())
}
