#!/bin/zsh
set -eu
rg -n 'target_arch|target_feature|is_.*feature_detected|vmull_p64|_mm_clmulepi64|vld1q_u8|_mm256_loadu|input.len\(\) == BLOCK_BYTES|tails::load' crates/jmax-scan-{neon,avx2,avx512}/src/{classify,quotes,structural}.rs
rg -n 'classify_block|dispatch|refill' crates/jmax-syntax/src/block_cursor.rs
rg -n 'DuplicatePolicyOwner|SkipMode::' crates/jmax-schema-vm/src/direct.rs crates/jmax-syntax/src/block_cursor.rs
rg -n 'ValidationCapability|ValidationOutput|capability_matches|validate_output|as_slice\(\) !=' crates/jmax-nagoya/src/{lib,submit,worker}.rs
