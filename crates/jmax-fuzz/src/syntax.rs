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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    struct FixedOracle { label: &'static str, outcome: Outcome }
    impl SyntaxOracle for FixedOracle {
        fn name(&self) -> &str { self.label }
        fn observe(&mut self, _input: &[u8], _limits: Limits) -> Result<Outcome> {
            Ok(self.outcome.clone())
        }
    }

    struct ErrorOracle;
    impl SyntaxOracle for ErrorOracle {
        fn name(&self) -> &str { "outer-error" }
        fn observe(&mut self, _input: &[u8], _limits: Limits) -> Result<Outcome> {
            Err(harness_error(ErrorCode::Cancelled, "oracle infrastructure failure"))
        }
    }

    fn config(max_cases: u32, max_failures: u32, max_input_bytes: usize) -> FuzzConfig {
        FuzzConfig { seed: 7, max_cases, max_input_bytes, max_failures,
            rejection_comparison: RejectionComparison::CodeAndOffset }
    }

    #[test]
    fn corpus_has_scalar_structural_utf8_number_and_boundary_vectors() {
        let corpus = seed_corpus(128).unwrap();
        let bytes: Vec<&[u8]> = corpus.iter().map(Payload::as_slice).collect();
        for expected in [b"null".as_slice(), b"true", b"false", b"[]", b"{}",
            b"\"\\uD834\\uDD1E\"", b"\"\\uD800\"", b"01", b"nullx"] {
            assert!(bytes.contains(&expected));
        }
        for len in [0usize, 1, 15, 16, 31, 32, 63, 64, 65, 127, 128] {
            assert!(bytes.iter().any(|v| v.len() == len), "missing length {len}");
        }
        assert!(seed_corpus(8).unwrap().iter().all(|p| p.len() <= 8));
    }

    #[test]
    fn corpus_rejects_bad_caps_and_omits_oversized_candidates_without_truncation() {
        assert_eq!(seed_corpus(0).unwrap_err().code, ErrorCode::ConstraintViolation);
        assert_eq!(seed_corpus(u32::MAX as usize + 1).unwrap_err().code, ErrorCode::ConstraintViolation);
        let tiny = seed_corpus(3).unwrap();
        assert!(tiny.iter().all(|p| p.len() <= 3));
        assert!(!tiny.iter().any(|p| p.as_slice() == b"nul"));
    }

    #[test]
    fn mutation_is_replayable_bounded_and_handles_empty_and_full_buffers() {
        let mut a = DeterministicRng::new(91);
        let mut b = DeterministicRng::new(91);
        for source in [&b""[..], &b"abc"[..], &b"1234"[..]] {
            assert_eq!(mutate(source, &mut a, 4).unwrap(), mutate(source, &mut b, 4).unwrap());
        }
        let mut rng = DeterministicRng::new(1);
        assert!(mutate(b"", &mut rng, 1).unwrap().len() <= 1);
        assert_eq!(mutate(b"abcde", &mut rng, 4).unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert_eq!(mutate(b"", &mut rng, 0).unwrap_err().code, ErrorCode::ConstraintViolation);
        for seed in 0..64 {
            let mut rng = DeterministicRng::new(seed);
            let changed = mutate(b"abc", &mut rng, 4).unwrap();
            assert_ne!(changed, b"abc", "mutation made no progress for seed {seed}");
            assert!(changed.len() <= 4);
        }
    }

    #[test]
    fn differential_run_counts_agreement_and_records_replayable_mismatch() {
        let seeds = vec![Payload::from_slice(b"null"), Payload::from_slice(b"[1]")];
        let accepted = Outcome::Accepted { canonical: b"null".to_vec() };
        let mut left = FixedOracle { label: "left", outcome: accepted.clone() };
        let mut right = FixedOracle { label: "right", outcome: Outcome::Accepted { canonical: b"other".to_vec() } };
        let result = run_syntax(&seeds, &mut left, &mut right, Limits::strict_default(), config(2, 2, 8)).unwrap();
        assert_eq!((result.attempted, result.agreed), (2, 0));
        assert_eq!(result.failures.len(), 2);
        assert_eq!(result.failures[0].input, b"null");
        assert_eq!(result.failures[0].case_index, 0);
        assert_eq!(result.failures[0].seed, 7);
        assert_eq!(result.failures[0].kind, FailureKind::Value);
        assert!(!result.truncated);
    }

    #[test]
    fn failure_cap_stops_and_marks_remaining_work_truncated() {
        let seeds = vec![Payload::from_slice(b"null")];
        let mut left = FixedOracle { label: "l", outcome: Outcome::Accepted { canonical: vec![1] } };
        let mut right = FixedOracle { label: "r", outcome: Outcome::Accepted { canonical: vec![2] } };
        let report = run_syntax(&seeds, &mut left, &mut right, Limits::strict_default(), config(4, 1, 8)).unwrap();
        assert_eq!(report.attempted, 1);
        assert_eq!(report.failures.len(), 1);
        assert!(report.truncated);
        assert!(!report.is_success());
    }

    #[test]
    fn rejection_policy_and_outer_error_paths_are_distinct() {
        let inputs = vec![Payload::from_slice(b"{")];
        let mut left = FixedOracle { label: "l", outcome: Outcome::Rejected { code: ErrorCode::InvalidSyntax, byte_offset: 0 } };
        let mut right = FixedOracle { label: "r", outcome: Outcome::Rejected { code: ErrorCode::UnexpectedEof, byte_offset: 1 } };
        let mut accept_only = config(1, 1, 8);
        accept_only.rejection_comparison = RejectionComparison::AcceptanceOnly;
        assert_eq!(run_syntax(&inputs, &mut left, &mut right, Limits::strict_default(), accept_only).unwrap().agreed, 1);
        let error = run_syntax(&inputs, &mut ErrorOracle, &mut right, Limits::strict_default(), config(1, 1, 8)).unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        let empty = run_syntax(&[], &mut left, &mut right, Limits::strict_default(), config(1, 1, 8)).unwrap_err();
        assert_eq!(empty.code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn oversized_seed_and_invalid_configuration_fail_before_oracles() {
        let seeds = vec![Payload::from_slice(b"12345")];
        let mut left = FixedOracle { label: "same", outcome: Outcome::Accepted { canonical: vec![] } };
        let mut right = FixedOracle { label: "same", outcome: Outcome::Accepted { canonical: vec![] } };
        assert_eq!(run_syntax(&seeds, &mut left, &mut right, Limits::strict_default(), config(1, 1, 4)).unwrap_err().code,
            ErrorCode::PayloadTooLarge);
        assert_eq!(run_syntax(&seeds, &mut left, &mut right, Limits::strict_default(), config(0, 1, 8)).unwrap_err().code,
            ErrorCode::ConstraintViolation);
    }

    #[test]
    fn diagnostics_keep_names_bounded() {
        let diagnostic = oracle_detail("x".repeat(200).as_str(), "right");
        assert!(diagnostic.len() < 128);
        assert!(diagnostic.starts_with("syntax oracle mismatch"));
    }
}
