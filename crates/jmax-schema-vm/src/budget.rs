//! A conservative, input-independent bound on what a strict validation can
//! consume, used to decide whether the VM can fail on resources at all.
//!
//! The bound is `bytes * per_byte + fixed`: every input byte belongs to exactly
//! one token, every token kind is charged by a bounded amount per byte of that
//! token, and the fixed part covers per-depth and per-program allocations. It is
//! deliberately generous (a test measures real consumption against it) and it
//! gives up (`None`) for programs whose cost is not linear in the input:
//! patterns and constrained untyped values. The legacy entry points also keep
//! open objects and enums conservative; the counted native entry point accepts
//! sound counts for their input-dependent work.

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

/// Native validation can supply sound upper bounds for work that the legacy
/// VM-only shortcut cannot infer from a schema and payload length. Unknown
/// fields are retained for duplicate checks, and enum membership scans the
/// declared variants. `skipped_fields` counts strict skips of unknown values;
/// each such skip may traverse a nested subtree. Disjoint skipped ranges are
/// bounded by the full input length, in addition to per-field setup costs.
pub fn budget_safe_native_counted(
    program: &ValidatedProgram,
    len: usize,
    decimals: u64,
    skipped_fields: u64,
    enum_values: u64,
    limits: VmLimits,
) -> bool {
    safe_with(
        len,
        limits,
        budget_bound_native_counted(program, len, decimals, skipped_fields, enum_values, limits),
    )
}

/// Budget proof for a strict native decode with disjoint skipped input spans.
/// Unknown-key counts and decoded key bytes cover every schema-visited unknown
/// field. Skipped values are syntax-checked but do not create VM frames or run
/// per-token schema work; their bytes are included in the input prepayment.
/// The caller must obey NativeDecode's counted-progress contract.
pub fn budget_safe_native_progress(
    program: &ValidatedProgram, len: usize, decimals: u64, skipped_fields: u64,
    enum_values: u64, unknown_fields: u64, unknown_key_bytes: u64,
    skipped_bytes: u64, limits: VmLimits,
) -> bool {
    safe_with(len, limits, budget_bound_native_progress(program, len, decimals,
        skipped_fields, enum_values, unknown_fields, unknown_key_bytes, skipped_bytes, limits))
}

fn budget_bound_native_progress(
    program: &ValidatedProgram, len: usize, decimals: u64, skipped_fields: u64,
    enum_values: u64, unknown_fields: u64, unknown_key_bytes: u64,
    skipped_bytes: u64, limits: VmLimits,
) -> Option<BudgetBound> {
    let compiled = program.program();
    if !compiled.patterns.is_empty() { return None; }
    let mut decimal = false;
    let mut formatted = false;
    let mut enum_variants = 0u64;
    for state in &compiled.states {
        if state.constraints.pattern.is_some() { return None; }
        formatted |= state.constraints.format.is_some();
        match &state.kind {
            StateKind::Any if state.constraints != ConstraintSet::default() => return None,
            StateKind::Enum => enum_variants = enum_variants.max(state.constraints.enum_variants.len() as u64),
            StateKind::Scalar(ValueKind::Decimal) => decimal = true,
            _ => {}
        }
    }
    let len = u64::try_from(len).ok()?;
    let visited = len.checked_sub(skipped_bytes)?;
    let depth = u64::from(limits.syntax.max_depth).min(len);
    let mut passes = 0u64;
    let mut words = 1u64;
    let mut mask_bytes = 0u64;
    for table in &compiled.key_tables {
        passes = passes.max(lookup_passes(table));
        let n = u64::from(table.key_count).div_ceil(64).max(1);
        words = words.max(n);
        if n > 1 { mask_bytes = mask_bytes.max(n.checked_mul(8)?.checked_add(24)?); }
    }
    let factor = (if formatted { FORMAT_PER_BYTE } else { SCALAR_PER_BYTE })
        .max(passes.checked_add(1)?.checked_mul(3)?.checked_add(3)?)
        .max(words.checked_add(8)?).checked_add(2)?;
    let frame = size_of::<VmFrame>() as u64;
    // Frame growth also covers the smaller per-depth table/mask vectors.
    let frame_growth = depth.checked_mul(depth.checked_add(1)?)?.checked_div(2)?.checked_mul(frame)?.checked_mul(2)?;
    let frames = depth.checked_add(1)?.checked_mul(frame)?.checked_mul(2)?;
    let masks = depth.checked_mul(mask_bytes)?.checked_mul(2)?;
    let slots = u64::from(program.image().header.slot_count).checked_mul(size_of::<SlotValue>() as u64)?;
    let states = u64::try_from(compiled.states.len()).ok()?;
    // Per-depth tables retain maxima from disjoint sets of unknown keys. The
    // sum of those maxima is bounded by the total observed key count/bytes.
    // A table has at most 8*n+8 slots, including power-of-two growth slack.
    let n = unknown_fields.min(len / 4);
    let table_bytes = n.checked_mul(8)?.checked_add(depth.checked_mul(8)?)?
        .checked_add(8)?.checked_mul(size_of::<crate::UnknownKeySlot>() as u64)?;
    let unknown = if limits.syntax.reject_duplicate_keys {
        table_bytes.checked_add(unknown_key_bytes)?.checked_add(depth.checked_mul(48)?)?.checked_mul(2)?
    } else { 0 };
    let duplicate_work = if limits.syntax.reject_duplicate_keys {
        n.checked_mul(n)?.checked_add(n.checked_mul(unknown_key_bytes)?)?
            .checked_add(table_bytes.checked_mul(2)?)?
    } else { 0 };
    let steps = len.checked_add(visited.checked_mul(factor)?)?
        .checked_add(FIXED_SLACK)?.checked_add(frame_growth)?.checked_add(masks)?
        .checked_add(slots.checked_mul(2)?)?.checked_add(states.checked_mul(16)?)?
        .checked_add(unknown)?.checked_add(duplicate_work)?
        .checked_add(skipped_fields.checked_mul(depth.checked_add(1)?)?.checked_mul(8)?)?
        .checked_add(enum_values.checked_add(visited)?.checked_mul(enum_variants)?)?
        .checked_add(if decimal { decimals.checked_mul(DECIMAL_FIXED)? } else { 0 })?;
    let scratch_bytes = frames.checked_add(len.checked_mul(2)?)?.checked_add(masks)?
        .checked_add(slots)?.checked_add(states.checked_mul(16)?)?.checked_add(unknown)?
        .checked_add(FIXED_SLACK.checked_mul(2)?)?;
    Some(BudgetBound { steps, scratch_bytes })
}

fn budget_bound_native_counted(
    program: &ValidatedProgram,
    len: usize,
    decimals: u64,
    skipped_fields: u64,
    enum_values: u64,
    limits: VmLimits,
) -> Option<BudgetBound> {
    let compiled = program.program();
    if !compiled.patterns.is_empty() {
        return None;
    }
    let mut decimal = false;
    let mut formatted = false;
    let mut enum_variants = 0u64;
    for state in &compiled.states {
        if state.constraints.pattern.is_some() {
            return None;
        }
        formatted |= state.constraints.format.is_some();
        match &state.kind {
            StateKind::Any if state.constraints != ConstraintSet::default() => return None,
            StateKind::Any => {}
            StateKind::Object { .. } | StateKind::Array { .. } => {}
            StateKind::Enum => {
                enum_variants =
                    enum_variants.max(u64::try_from(state.constraints.enum_variants.len()).ok()?);
            }
            StateKind::Scalar(ValueKind::Decimal) => decimal = true,
            StateKind::Scalar(_) => {}
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
    let scalar = if formatted {
        FORMAT_PER_BYTE
    } else {
        SCALAR_PER_BYTE
    };
    let per_byte = COMMON_PER_BYTE.checked_add(scalar.max(key_factor).max(structural))?;

    let frame = size_of::<VmFrame>() as u64;
    let frame_growth = depth
        .checked_mul(depth.checked_add(1)?)?
        .checked_div(2)?
        .checked_mul(frame)?;
    let mask_growth = depth.checked_mul(mask_bytes)?.checked_mul(2)?;
    let slots =
        u64::from(program.image().header.slot_count).checked_mul(size_of::<SlotValue>() as u64)?;
    let states = u64::try_from(compiled.states.len()).ok()?;
    let skipped = skipped_fields;
    // Every unknown object field needs at least four input bytes. Clamp only
    // the derived allocation/probe count: a larger supplied upper bound is
    // still charged below for strict nested-value work.
    let retained_keys = skipped.min(len / 4);
    let unknown_slots = retained_keys.checked_mul(8)?.checked_add(8)?;
    let unknown_slot_bytes = unknown_slots.checked_mul(size_of::<crate::UnknownKeySlot>() as u64)?;
    let unknown_scratch = unknown_slot_bytes.checked_mul(2)?.checked_add(len)?;

    // Linear probing has a quadratic worst case. Equal-hash comparisons also
    // scan key bytes, but keys compared by the table have equal lengths; the
    // total of all retained key lengths cannot exceed the payload length.
    let duplicate_work = retained_keys
        .checked_mul(retained_keys)?
        .checked_add(retained_keys.checked_mul(len)?)?;
    let rehash_work = unknown_slot_bytes.checked_mul(2)?;
    // Unknown values skipped by the VM occupy disjoint input ranges. Their
    // total traversal is bounded by len, including nested subtrees. Per-skip
    // setup may inspect the active depth and is charged separately.
    let skipped_work = skipped
        .checked_mul(depth.checked_add(1)?)?
        .checked_add(if skipped != 0 { len } else { 0 })?
        .checked_mul(8)?;
    // Each comparison costs at most 1 + decoded token length. Enum tokens
    // occupy disjoint ranges and JSON escapes never shorten their raw spelling,
    // so all decoded enum text lengths together are at most len.
    let enum_work = enum_values.checked_add(len)?.checked_mul(enum_variants)?;
    let fixed = FIXED_SLACK
        .checked_add(frame_growth)?
        .checked_add(mask_growth)?
        .checked_add(slots.checked_mul(2)?)?
        .checked_add(states.checked_mul(16)?)?
        .checked_add(unknown_scratch)?;
    let steps = len
        .checked_mul(per_byte)?
        .checked_add(fixed)?
        .checked_add(if decimal {
            decimals.checked_mul(DECIMAL_FIXED)?
        } else {
            0
        })?
        .checked_add(duplicate_work)?
        .checked_add(rehash_work)?
        .checked_add(skipped_work)?
        .checked_add(enum_work)?;

    let frames = depth.checked_add(1)?.checked_mul(frame)?.checked_mul(2)?;
    let arena = len.checked_mul(2)?.checked_add(FIXED_SLACK)?;
    let scratch_bytes = frames
        .checked_add(arena)?
        .checked_add(mask_growth)?
        .checked_add(slots)?
        .checked_add(states.checked_mul(16)?)?
        .checked_add(unknown_scratch)?
        .checked_add(FIXED_SLACK)?;
    Some(BudgetBound {
        steps,
        scratch_bytes,
    })
}

/// Bound the strict-validation cost of any input of at most `len` bytes.
/// `None` means no bound is claimed.
pub fn budget_bound(
    program: &ValidatedProgram,
    len: usize,
    limits: VmLimits,
) -> Option<BudgetBound> {
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
    let scalar = if formatted {
        FORMAT_PER_BYTE
    } else {
        SCALAR_PER_BYTE
    };
    let per_byte = COMMON_PER_BYTE.checked_add(scalar.max(key_factor).max(structural))?;

    // Frames grow one slot at a time, charging the new size at each growth.
    let frame = size_of::<VmFrame>() as u64;
    let frame_growth = depth
        .checked_mul(depth.checked_add(1)?)?
        .checked_div(2)?
        .checked_mul(frame)?;
    let mask_growth = depth.checked_mul(mask_bytes)?.checked_mul(2)?;
    let slots =
        u64::from(program.image().header.slot_count).checked_mul(size_of::<SlotValue>() as u64)?;
    let states = u64::try_from(compiled.states.len()).ok()?;
    let fixed = FIXED_SLACK
        .checked_add(frame_growth)?
        .checked_add(mask_growth)?
        .checked_add(slots.checked_mul(2)?)?
        .checked_add(states.checked_mul(16)?)?;
    let steps = len
        .checked_mul(per_byte)?
        .checked_add(fixed)?
        .checked_add(if decimal {
            decimals.checked_mul(DECIMAL_FIXED)?
        } else {
            0
        })?;

    let frames = depth.checked_add(1)?.checked_mul(frame)?.checked_mul(2)?;
    let arena = len.checked_mul(2)?.checked_add(FIXED_SLACK)?;
    let scratch_bytes = frames
        .checked_add(arena)?
        .checked_add(mask_growth)?
        .checked_add(slots)?
        .checked_add(states.checked_mul(16)?)?
        .checked_add(FIXED_SLACK)?;
    Some(BudgetBound {
        steps,
        scratch_bytes,
    })
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
pub fn budget_safe_counted(
    program: &ValidatedProgram,
    len: usize,
    decimals: u64,
    limits: VmLimits,
) -> bool {
    safe_with(
        len,
        limits,
        budget_bound_counted(program, len, decimals, limits),
    )
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

#[cfg(test)]
mod native_tests {
    use super::*;
    use alloc::{format, vec};
    use jmax_schema_ir::{
        EnumShape, EnumVariant, FieldId, ObjectShape, Property, Schema, SchemaId, TypeId, TypeNode,
    };

    fn open_enum_program() -> ValidatedProgram {
        let schema = Schema {
            name: "counted_budget".into(),
            schema_id: SchemaId(751),
            root: TypeId(0),
            definitions: vec![],
            nodes: vec![
                TypeNode::Object(ObjectShape {
                    nullable: false,
                    reject_unknown: false,
                    min_properties: None,
                    max_properties: None,
                    properties: vec![Property {
                        field_id: FieldId(1),
                        name: "state".into(),
                        ty: TypeId(1),
                        required: true,
                        slot: None,
                    }],
                }),
                TypeNode::Enum(EnumShape {
                    nullable: false,
                    variants: vec![
                        EnumVariant {
                            name: "Ready".into(),
                            discriminant: 7,
                        },
                        EnumVariant {
                            name: "Last".into(),
                            discriminant: -3,
                        },
                    ],
                }),
            ],
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default()).unwrap();
        jmax_schema_compile::compile(&resolved, &Default::default()).unwrap()
    }

    #[test]
    fn open_objects_and_enums_validate_within_claimed_native_bound() {
        let program = open_enum_program();
        for fields in [0, 1, 8, 32, 128] {
            let mut text = alloc::string::String::from("{\"state\":\"Last\"");
            for n in 0..fields {
                text.push_str(&format!(
                    ",\"unknown_{n}\":{{\"x\":[1,2,{{\"escaped\":\"a\\nb\"}}]}}"
                ));
            }
            text.push('}');
            let mut limits = VmLimits::default();
            limits.syntax.reject_unknown_keys = false;
            limits.syntax.max_depth = 16;
            let bound =
                budget_bound_native_counted(&program, text.len(), 0, fields, 1, limits).unwrap();
            limits.max_steps = bound.steps;
            limits.max_scratch_bytes = u32::try_from(bound.scratch_bytes).unwrap();
            assert!(budget_safe_native_counted(
                &program,
                text.len(),
                0,
                fields,
                1,
                limits
            ));
            crate::direct::validate_borrowed_into(
                &program,
                text.as_bytes(),
                limits,
                jmax_abi::number::NumericPolicy::strict_default(),
                &mut crate::ValidationScratch::default(),
            )
            .unwrap();
            limits.max_steps = 1;
            assert!(!budget_safe_native_counted(
                &program,
                text.len(),
                0,
                fields,
                1,
                limits
            ));
        }
    }

    #[test]
    fn progress_bound_covers_large_strict_skip_and_rejects_overflow() {
        let program = open_enum_program();
        let payload_len = 2_250_000;
        let text = format!("{{\"state\":\"Last\",\"unknown\":\"{}\"}}", "x".repeat(payload_len));
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        limits.syntax.max_string_bytes = payload_len as u32;
        let skipped = (payload_len + 2) as u64;
        let bound = budget_bound_native_progress(&program, text.len(), 0, 1, 1, 1, 7, skipped, limits).unwrap();
        assert!(bound.steps < 16_000_000);
        limits.max_steps = bound.steps;
        limits.max_scratch_bytes = u32::try_from(bound.scratch_bytes).unwrap();
        assert!(budget_safe_native_progress(&program, text.len(), 0, 1, 1, 1, 7, skipped, limits));
        crate::direct::validate_borrowed_into(&program, text.as_bytes(), limits,
            jmax_abi::number::NumericPolicy::strict_default(), &mut crate::ValidationScratch::default()).unwrap();
        limits.max_steps -= 1;
        assert!(!budget_safe_native_progress(&program, text.len(), 0, 1, 1, 1, 7, skipped, limits));
        assert!(!budget_safe_native_progress(&program, text.len(), 0, 1, 1, 1, 7, u64::MAX, limits));
        assert!(!budget_safe_native_progress(&program, text.len(), 0, 1, 1, u64::MAX, u64::MAX, skipped, limits));
    }

    #[test]
    fn progress_bound_covers_wide_unknown_tables_at_multiple_depths() {
        for depth in [1usize, 4, 16] {
            let mut nodes = vec![];
            let mut raw = alloc::string::String::new();
            let mut key_bytes = 0;
            for level in 0..depth {
                let mut properties = (0..70).map(|key| Property {
                    field_id: FieldId((level * 100 + key + 1) as u32),
                    name: format!("known_{level}_{key}"), ty: TypeId(depth as u32),
                    required: false, slot: None,
                }).collect::<alloc::vec::Vec<_>>();
                if level + 1 < depth {
                    properties.push(Property { field_id: FieldId((level * 100 + 99) as u32),
                        name: "child".into(), ty: TypeId((level + 1) as u32), required: true, slot: None });
                }
                nodes.push(TypeNode::Object(ObjectShape {
                    nullable: false, reject_unknown: false, min_properties: None,
                    max_properties: None, properties,
                }));
                raw.push_str("{");
                for key in 0..3 {
                    if key != 0 { raw.push_str(","); }
                    let name = format!("unknown_{level}_{key}");
                    key_bytes += name.len() as u64;
                    raw.push_str(&format!("\"{name}\":{key}"));
                }
                if level + 1 < depth { raw.push_str(",\"child\":"); }
            }
            raw.push_str(&"}".repeat(depth));
            nodes.push(TypeNode::Scalar(jmax_schema_ir::ScalarConstraints::new(ValueKind::Boolean)));
            let schema = Schema { name: "progress_depth".into(), schema_id: SchemaId(752),
                root: TypeId(0), definitions: vec![], nodes };
            let resolved = jmax_schema_ir::resolve::resolve(schema, Default::default()).unwrap();
            let program = jmax_schema_compile::compile(&resolved, &Default::default()).unwrap();
            let mut limits = VmLimits::default();
            limits.syntax.reject_unknown_keys = false;
            limits.syntax.max_depth = depth as u16;
            let count = depth as u64 * 3;
            let bound = budget_bound_native_progress(&program, raw.len(), 0, count, 0, count, key_bytes, count, limits).unwrap();
            limits.max_steps = bound.steps;
            limits.max_scratch_bytes = u32::try_from(bound.scratch_bytes).unwrap();
            crate::direct::validate_borrowed_into(&program, raw.as_bytes(), limits,
                jmax_abi::number::NumericPolicy::strict_default(), &mut crate::ValidationScratch::default()).unwrap();
        }
    }

    #[test]
    fn native_bound_overflow_and_missing_resources_fail_closed() {
        let program = open_enum_program();
        let limits = VmLimits::default();
        assert!(!budget_safe_native_counted(
            &program,
            64,
            0,
            u64::MAX,
            1,
            limits
        ));
        assert!(!budget_safe_native_counted(
            &program,
            64,
            0,
            1,
            u64::MAX,
            limits
        ));
        let mut no_scratch = limits;
        no_scratch.max_scratch_bytes = 0;
        assert!(!budget_safe_native_counted(
            &program, 64, 0, 1, 1, no_scratch
        ));
    }
}
