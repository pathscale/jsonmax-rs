//! The `native_vs_vm` fuzz target over its seed corpus and deterministic mutants,
//! without a fuzzing engine. `JMAX_FUZZ_ROUNDS` sets the mutants per seed and limit set.

use jmax_native_fixtures::fuzzing::{native_vs_vm, seed_corpus, SUBJECTS};
use jmax_native_fixtures::harness::{limit_variants, mutate, Rng};

#[test]
fn seeds_and_mutants_agree_with_the_vm() {
    let rounds: usize = std::env::var("JMAX_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let seeds = seed_corpus();
    assert!(seeds.len() > 20);
    let mut subjects = [false; 16];
    let mut rng = Rng(0x6675_7a7a_6e76_6d31);
    let limits = limit_variants().len();
    let mut calls = 0usize;
    for seed in &seeds {
        subjects[usize::from(seed[0])] = true;
        for limit in 0..limits {
            let mut data = seed.clone();
            data[1] = limit as u8;
            native_vs_vm(&data);
            calls += 1;
            for _ in 0..rounds {
                // Mutate the document only, so the selector keeps choosing the same decoder.
                let mut mutant = data[..2].to_vec();
                mutant.extend(mutate(&data[2..], &mut rng));
                native_vs_vm(&mutant);
                calls += 1;
            }
        }
    }
    assert!(subjects[..SUBJECTS.len()].iter().all(|seen| *seen), "every decoder has a seed");
    assert!(calls > 1_000, "{calls}");
    // Degenerate inputs select nothing or an empty document.
    native_vs_vm(&[]);
    native_vs_vm(&[3]);
    for subject in 0..=255u8 {
        native_vs_vm(&[subject, 0]);
        native_vs_vm(&[subject, 255, b'{']);
    }
}
