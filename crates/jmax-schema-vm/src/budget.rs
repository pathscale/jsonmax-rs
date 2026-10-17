//! A conservative, input-independent bound on what a strict validation can
//! consume, used to decide whether the VM can fail on resources at all.
//!
//! The bound is `bytes * per_byte + fixed`: every input byte belongs to exactly
//! one token, every token kind is charged by a bounded amount per byte of that
//! token, and the fixed part covers per-depth and per-program allocations. It is
//! deliberately generous (a test measures real consumption against it) and it
//! gives up (`None`) for programs whose cost is not linear in the input:
//! patterns, retained unknown keys and constrained untyped values.

use crate::{
    ConstraintSet, KeyTable, SlotValue, StateKind, ValidatedProgram, ValueKind, VmFrame, VmLimits,
};
use core::mem::size_of;

/// Upper bounds, valid for every input of at most the stated length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetBound {
    pub steps: u64,
    pub scratch_bytes: u64,
}

/// Steps charged by one decimal scalar beyond its token bytes: the VM prepays
/// 2,048 for rescaling and validation; the rest is slack for its own passes.
const DECIMAL_FIXED: u64 = 2_048 + 48;
/// Per-byte factor for plain scalars and strings, with slack.
const SCALAR_PER_BYTE: u64 = 8;
/// Shared by every byte: input prepayment, loop iteration and value charge.
const COMMON_PER_BYTE: u64 = 3;
/// Per-byte factor when a format validator runs: eight passes of its own charge
/// on top of the scalar work.
const FORMAT_PER_BYTE: u64 = 24;
const FIXED_SLACK: u64 = 4_096;

/// Bound the strict-validation cost of any input of at most `len` bytes.
/// `None` means no bound is claimed.
pub fn budget_bound(program: &ValidatedProgram, len: usize, limits: VmLimits) -> Option<BudgetBound> {
    // A decimal is at least three bytes plus a delimiter, so an input of `len`
    // bytes holds at most len / 4 + 1 of them.
    let decimals = u64::try_from(len).ok()? / 4 + 1;
    budget_bound_counted(program, len, decimals, limits)
}

/// Bound the strict-validation cost of any input of at most `len` bytes that
/// holds at most `decimals` decimal scalars. The VM bills each decimal a fixed
/// amount that does not depend on its length, so a caller that knows the count
/// (a decoder that has just read the whole input) gets a much tighter bound than
/// the worst case `budget_bound` must assume. The bound is `None` for the same
/// programs, and is never below the measured consumption of an input with at
/// most that many decimals.
pub fn budget_bound_counted(
    program: &ValidatedProgram,
    len: usize,
    decimals: u64,
    limits: VmLimits,
) -> Option<BudgetBound> {
    let compiled = program.program();
    if !compiled.patterns.is_empty() {
        return None;
    }
    let mut decimal = false;
    let mut formatted = false;
    for state in &compiled.states {
        if state.constraints.pattern.is_some() {
            return None;
        }
        formatted |= state.constraints.format.is_some();
        match &state.kind {
            StateKind::Any => {
                if state.constraints != ConstraintSet::default() {
                    return None;
                }
            }
            StateKind::Object { reject_unknown, .. } => {
                if !*reject_unknown {
                    return None;
                }
            }
            // Enum membership cost depends on its declared table. Keep the
            // native budget shortcut conservative until it has an enum bound.
            StateKind::Enum => return None,
            StateKind::Scalar(ValueKind::Decimal) => decimal = true,
            StateKind::Scalar(_) | StateKind::Array { .. } => {}
        }
    }

    let mut passes = 0u64;
    let mut words = 1u64;
    let mut mask_bytes = 0u64;
    for table in &compiled.key_tables {
        passes = passes.max(lookup_passes(table));
        let table_words = u64::from(table.key_count).div_ceil(64).max(1);
        words = words.max(table_words);
        if table_words > 1 {
            mask_bytes = mask_bytes.max(table_words.checked_mul(8)?.checked_add(24)?);
        }
    }

    let len = u64::try_from(len).ok()?;
    let depth = u64::from(limits.syntax.max_depth).min(len);
    let key_factor = passes.checked_add(1)?.checked_mul(3)?.checked_add(3)?;
    let structural = words.checked_add(8)?;
    let scalar = if formatted { FORMAT_PER_BYTE } else { SCALAR_PER_BYTE };
    let per_byte = COMMON_PER_BYTE
        .checked_add(scalar.max(key_factor).max(structural))?;

    // Frames grow one slot at a time, charging the new size at each growth.
    let frame = size_of::<VmFrame>() as u64;
    let frame_growth = depth.checked_mul(depth.checked_add(1)?)?.checked_div(2)?.checked_mul(frame)?;
    let mask_growth = depth.checked_mul(mask_bytes)?.checked_mul(2)?;
    let slots = u64::from(program.image().header.slot_count)
        .checked_mul(size_of::<SlotValue>() as u64)?;
    let states = u64::try_from(compiled.states.len()).ok()?;
    let fixed = FIXED_SLACK
        .checked_add(frame_growth)?
        .checked_add(mask_growth)?
        .checked_add(slots.checked_mul(2)?)?
        .checked_add(states.checked_mul(16)?)?;
    let steps = len
        .checked_mul(per_byte)?
        .checked_add(fixed)?
        .checked_add(if decimal { decimals.checked_mul(DECIMAL_FIXED)? } else { 0 })?;

    let frames = depth.checked_add(1)?.checked_mul(frame)?.checked_mul(2)?;
    let arena = len.checked_mul(2)?.checked_add(FIXED_SLACK)?;
    let scratch_bytes = frames
        .checked_add(arena)?
        .checked_add(mask_growth)?
        .checked_add(slots)?
        .checked_add(states.checked_mul(16)?)?
        .checked_add(FIXED_SLACK)?;
    Some(BudgetBound { steps, scratch_bytes })
}

fn lookup_passes(table: &KeyTable) -> u64 {
    match &table.strategy {
        jmax_schema_compile::KeyStrategy::Swar => u64::from(table.key_count) + 1,
        jmax_schema_compile::KeyStrategy::PerfectHash { .. } => 2,
        jmax_schema_compile::KeyStrategy::BucketedPerfectHash { .. } => 3,
    }
}

/// True only when a strict validation of any input of at most `len` bytes
/// cannot fail by exhausting steps, scratch, payload or decoded-arena limits.
pub fn budget_safe(program: &ValidatedProgram, len: usize, limits: VmLimits) -> bool {
    safe_with(len, limits, budget_bound(program, len, limits))
}

/// `budget_safe` for an input known to hold at most `decimals` decimal scalars.
pub fn budget_safe_counted(program: &ValidatedProgram, len: usize, decimals: u64, limits: VmLimits) -> bool {
    safe_with(len, limits, budget_bound_counted(program, len, decimals, limits))
}

fn safe_with(len: usize, limits: VmLimits, bound: Option<BudgetBound>) -> bool {
    let Ok(wide) = u32::try_from(len) else {
        return false;
    };
    if wide > limits.syntax.max_payload_bytes || wide > limits.syntax.max_decoded_bytes {
        return false;
    }
    match bound {
        Some(bound) => {
            bound.steps <= limits.max_steps
                && bound.scratch_bytes <= u64::from(limits.max_scratch_bytes)
        }
        None => false,
    }
}
