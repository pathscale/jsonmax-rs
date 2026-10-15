//! Deterministic differential harness for strict JSON syntax implementations.

use alloc::{string::String, vec::Vec};

use crate::{
    compare_outcomes, harness_error, DeterministicRng, ErrorCode, Failure, FailureKind,
    FuzzConfig, FuzzReport, Limits, Outcome, Payload, RejectionComparison, Result, SyntaxOracle,
};

/// Produces an ordered, deterministic corpus of valid and invalid JSON bytes.
/// A candidate is included only in its original form when it fits the cap.
pub fn seed_corpus(max_input_bytes: usize) -> Result<Vec<Payload>> {
    if max_input_bytes == 0 || max_input_bytes > u32::MAX as usize {
        return Err(harness_error(ErrorCode::ConstraintViolation, "invalid syntax corpus byte cap"));
    }

    let mut corpus = Vec::new();
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    let samples: &[&[u8]] = &[
        b"null", b"true", b"false", b"0", b"-0", b"1.25", b"\"\"", b"\"x\"",
        b"[]", b"{}", b"[null]", b"{\"a\":1}", b"[[],{}]", b"{\"a\":[true,{\"b\":null}]}",
        b"{", b"}", b"[", b"]", b"{\"a\" 1}", b"{\"a\":}", b"{\"a\":1,}",
        b"{,\"a\":1}", b"[1,]", b"[,1]", b"{\"a\"::1}", b"{\"a\"  :1}",
        b"\"\\\"\"", b"\"\\\\\"", b"\"\\\\\\\"", b"\"\\q\"", b"\"\\u12G4\"",
        b"\"\x01\"", b"\"\n\"", b"\"\xc3\"", b"\"\xe2\x82\"", b"\"\xff\"",
        b"\"\\uD834\\uDD1E\"", b"\"\\uD800\"", b"\"\\uDC00\"",
        b"{\"a\":1,\"a\":2}", b"{\"\\u0061\":1,\"a\":2}",
        b"01", b"-01", b"+1", b".1", b"1.", b"1e", b"1e+", b"--1", b"1x",
        b"null ", b"nullx", b"\"unterminated", b"[1", b"{\"a\":1", b"tru",
    ];
    for sample in samples {
        let mut candidate = Vec::new();
        candidate.try_reserve_exact(sample.len()).map_err(|_| {
            harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
        })?;
        candidate.extend_from_slice(sample);
        candidates.try_reserve(1).map_err(|_| {
            harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
        })?;
        candidates.push(candidate);
    }

    // Exercise parser boundaries with valid JSON padded by legal trailing
    // whitespace. Distinct lengths remain replayable and are never truncated.
    for target in [0usize, 1, 15, 16, 31, 32, 63, 64, 65, 127, 128] {
        if target < 4 {
            let mut v = Vec::new();
            v.try_reserve_exact(target).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
            })?;
            v.extend(core::iter::repeat(b' ').take(target));
            candidates.try_reserve(1).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
            })?;
            candidates.push(v);
        } else {
            let mut v = Vec::new();
            v.try_reserve_exact(target).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
            })?;
            v.extend_from_slice(b"null");
            v.extend(core::iter::repeat(b' ').take(target - 4));
            candidates.try_reserve(1).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
            })?;
            candidates.push(v);
        }
    }

    corpus.try_reserve(candidates.len()).map_err(|_| {
        harness_error(ErrorCode::BudgetExhausted, "syntax corpus allocation failed")
    })?;
    for candidate in candidates {
        if candidate.len() <= max_input_bytes {
            corpus.push(Payload::from_vec(candidate));
        }
    }
    Ok(corpus)
}

/// Applies one bounded byte-level mutation using deterministic SplitMix64 draws.
pub fn mutate(input: &[u8], rng: &mut DeterministicRng, max_input_bytes: usize) -> Result<Vec<u8>> {
    if max_input_bytes == 0 || max_input_bytes > u32::MAX as usize {
        return Err(harness_error(ErrorCode::ConstraintViolation, "invalid mutation byte cap"));
    }
    if input.len() > max_input_bytes {
        return Err(harness_error(ErrorCode::PayloadTooLarge, "mutation input exceeds byte cap"));
    }

    // Exactly one draw selects the operation. Every subsequent draw belongs to
    // that operation, and all indexing uses a nonempty range or a guarded path.
    let operation = rng.index(5).unwrap_or(0);
    let mut output = Vec::new();
    output.try_reserve_exact(max_input_bytes.min(input.len().saturating_add(1))).map_err(|_| {
        harness_error(ErrorCode::BudgetExhausted, "mutation allocation failed")
    })?;
    output.extend_from_slice(input);
    match operation {
        0 => { // insertion
            if output.len() < max_input_bytes {
                let pos = rng.index(output.len() + 1).unwrap_or(0);
                let byte = rng.next_u64() as u8;
                output.insert(pos, byte);
            } else {
                replace_one(&mut output, rng);
            }
        }
        1 => { // deletion
            if !output.is_empty() {
                let pos = rng.index(output.len()).unwrap_or(0);
                output.remove(pos);
            } else if max_input_bytes > 0 {
                output.push(rng.next_u64() as u8);
            }
        }
        2 => replace_one(&mut output, rng),
        3 => { // duplicate one byte at a selected insertion point
            if !output.is_empty() && output.len() < max_input_bytes {
                let source = rng.index(output.len()).unwrap_or(0);
                let pos = rng.index(output.len() + 1).unwrap_or(0);
                let byte = output[source];
                output.insert(pos, byte);
            } else if output.len() == max_input_bytes {
                replace_one(&mut output, rng);
            } else if output.is_empty() && max_input_bytes > 0 {
                output.push(rng.next_u64() as u8);
            }
        }
        _ => { // truncation, including a possible empty result
            if output.is_empty() {
                output.push(rng.next_u64() as u8);
            } else {
                let new_len = rng.index(output.len()).unwrap_or(0);
                output.truncate(new_len);
            }
        }
    }
    Ok(output)
}

fn replace_one(output: &mut Vec<u8>, rng: &mut DeterministicRng) {
    if output.is_empty() {
        output.push(rng.next_u64() as u8);
    } else {
        let pos = rng.index(output.len()).unwrap_or(0);
        let byte = rng.next_u64() as u8;
        output[pos] = if byte == output[pos] { byte.wrapping_add(1) } else { byte };
    }
}

/// Runs paired syntax oracles over unchanged seeds followed by deterministic mutations.
pub fn run_syntax(
    inputs: &[Payload],
    left: &mut dyn SyntaxOracle,
    right: &mut dyn SyntaxOracle,
    limits: Limits,
    config: FuzzConfig,
) -> Result<FuzzReport> {
    config.validate()?;
    if inputs.is_empty() {
        return Err(harness_error(ErrorCode::ConstraintViolation, "syntax run requires seeds"));
    }
    for input in inputs {
        if input.len() > config.max_input_bytes {
            return Err(harness_error(ErrorCode::PayloadTooLarge, "syntax seed exceeds byte cap"));
        }
    }

    let target = core::cmp::min(inputs.len().max(config.max_cases as usize), config.max_cases as usize);
    let unchanged = core::cmp::min(inputs.len(), target);
    let mut rng = DeterministicRng::new(config.seed);
    let mut report = FuzzReport::default();
    for case_index in 0..target {
        if report.failures.len() >= config.max_failures as usize {
            report.truncated = true;
            break;
        }
        let tested: Vec<u8> = if case_index < unchanged {
            let bytes = inputs[case_index].as_slice();
            let mut copy = Vec::new();
            copy.try_reserve_exact(bytes.len()).map_err(|_| {
                harness_error(ErrorCode::BudgetExhausted, "syntax case allocation failed")
            })?;
            copy.extend_from_slice(bytes);
            copy
        } else {
            let seed_index = rng.index(inputs.len()).ok_or_else(|| {
                harness_error(ErrorCode::InternalError, "syntax seed selection failed")
            })?;
            mutate(inputs[seed_index].as_slice(), &mut rng, config.max_input_bytes)?
        };

        let left_outcome = left.observe(&tested, limits)?;
        let right_outcome = right.observe(&tested, limits)?;
        report.attempted += 1;
        match compare_outcomes(&left_outcome, &right_outcome, config.rejection_comparison) {
            None => report.agreed += 1,
            Some(kind) => {
                let mut failure_input = Vec::new();
                failure_input.try_reserve_exact(tested.len()).map_err(|_| {
                    harness_error(ErrorCode::BudgetExhausted, "syntax failure allocation failed")
                })?;
                failure_input.extend_from_slice(&tested);
                let detail = oracle_detail(left.name(), right.name());
                report.failures.try_reserve(1).map_err(|_| {
                    harness_error(ErrorCode::BudgetExhausted, "syntax report allocation failed")
                })?;
                let recorded = report.record_failure(Failure {
                    case_index: case_index as u64,
                    seed: config.seed,
                    kind,
                    input: failure_input,
                    detail,
                }, config.max_failures);
                if !recorded { break; }
            }
        }
    }
    if report.attempted == 0 {
        return Err(harness_error(ErrorCode::ConstraintViolation, "syntax run attempted no cases"));
    }
    Ok(report)
}

fn oracle_detail(left: &str, right: &str) -> String {
    // Names are bounded to keep diagnostics predictable even for a faulty adapter.
    let mut detail = String::new();
    detail.push_str("syntax oracle mismatch: ");
    detail.push_str(&left.chars().take(48).collect::<String>());
    detail.push_str(" vs ");
    detail.push_str(&right.chars().take(48).collect::<String>());
    detail
}
