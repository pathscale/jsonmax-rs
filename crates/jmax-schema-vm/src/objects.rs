//! Per-object key dispatch, duplicate tracking, and required-field validation.
//!
//! Seen bits address entries within a key table, not dense required-field IDs.
//! Each invocation owns its scratch, including for recursive schema instances.
use super::*;
use core::mem::size_of;
use jmax_abi::schema::KeyEntry;
use jmax_schema_compile::{keys, KeyStrategy, NO_REQUIRED_BIT};

// keys::lookup validates field uniqueness with a stack bitmap covering every
// u16 FieldId. Include that temporary seen storage in the VM scratch ceiling.
const LOOKUP_SCRATCH_BYTES: u32 = 65_536 / 8;

/// Start an object with an independent, zeroed seen mask.
pub fn begin(
    program: &ValidatedProgram,
    state: Option<StateId>,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<ObjectState> {
    budget.charge(1, offset)?;
    let table_id = match object_state(program, state, offset)? {
        Some(current) => match &current.kind {
            StateKind::Object { key_table, .. } => Some(*key_table),
            StateKind::Any => None,
            _ => return Err(type_error(offset)),
        },
        None => None,
    };
    let mut seen_words = Vec::new();
    if let Some(id) = table_id {
        let table = key_table(program, id, offset)?;
        table_entries(program, table, offset)?;
        let words = word_count(table.key_count, offset)?;
        seen_words = allocate::<u64>(words, budget, offset)?;
        // Allocation work already includes initialization of these bytes.
        seen_words.resize(words, 0);
    }
    Ok(ObjectState {
        table: table_id,
        seen_words,
        unknown_keys: Vec::new(),
        property_count: 0,
        pending: None,
    })
}

/// Match an already decoded key and schedule exactly one following value.
///
/// The interpreter validates selection once at its entry boundary. Duplicate
/// captures are forbidden regardless of whether that field was selected.
pub fn key(
    program: &ValidatedProgram,
    object: &mut ObjectState,
    key: &str,
    selection: &Selection,
    limits: &VmLimits,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<PendingValue> {
    if object.pending.is_some() {
        return Err(vm_error(ErrorCode::InvalidSyntax, offset,
            "object key arrived before the previous value"));
    }
    budget.charge(1, offset)?;
    let next_count = object.property_count.checked_add(1)
        .ok_or_else(|| resource_error(offset))?;
    if next_count > limits.syntax.max_fields_per_object {
        return Err(resource_error(offset));
    }
    let key_len = u32::try_from(key.len()).map_err(|_| resource_error(offset))?;
    if key_len > limits.syntax.max_string_bytes {
        return Err(resource_error(offset));
    }
    budget.charge(u64::from(key_len), offset)?;

    if let Some(id) = object.table {
        let table = key_table(program, id, offset)?;
        let entries = table_entries(program, table, offset)?;
        check_mask(object, table, offset)?;
        charge_lookup(table, entries, key_len, budget, offset)?;
        budget.retain(LOOKUP_SCRATCH_BYTES, offset)?;
        let matched = keys::lookup(program.image(), table, key.as_bytes());
        budget.release(LOOKUP_SCRATCH_BYTES)?;
        if let Some(entry) = matched? {
            // lookup returns a borrowed image entry. Locate that exact entry
            // without pointer arithmetic or relying on required_bit numbering.
            let mut local_index = None;
            for (index, candidate) in entries.iter().enumerate() {
                budget.charge(1, offset)?;
                if core::ptr::eq(candidate, entry) {
                    local_index = Some(index);
                    break;
                }
            }
            let index = local_index.ok_or_else(|| internal_error(offset))?;
            let field = FieldId(u32::from(entry.field_id));
            let result = known_value(program, object, entry, index, selection,
                limits, budget, offset);
            let pending = result.map_err(|error| error.with_schema(
                program.image().header.schema_id, field.0))?;
            object.property_count = next_count;
            object.pending = Some(pending);
            return Ok(pending);
        }

        // ObjectState deliberately stores only the table ID. The witnessed
        // program has exactly one owning Object state for each table.
        if limits.syntax.reject_unknown_keys || rejects_unknown(program, id, budget, offset)? {
            return Err(vm_error(ErrorCode::UnknownObjectKey, offset,
                "object key is not permitted by the schema"));
        }
    } else if !object.seen_words.is_empty() {
        return Err(internal_error(offset));
    }

    // Any and None have no whitelist. Runtime unknown-key rejection therefore
    // applies only when there actually is a compiled object table.
    if limits.syntax.reject_duplicate_keys {
        retain_unknown(object, key.as_bytes(), budget, offset)?;
    }
    let pending = PendingValue { state: None, field_id: FieldId(0), slot: None };
    object.property_count = next_count;
    object.pending = Some(pending);
    Ok(pending)
}

/// Consume a key's pending value at value start, including container opens.
pub fn take_value(object: &mut ObjectState, offset: u32) -> Result<PendingValue> {
    object.pending.take().ok_or_else(|| vm_error(ErrorCode::InvalidSyntax, offset,
        "object value has no preceding key"))
}

/// Check required membership and inclusive schema property-count bounds.
///
/// Scratch remains owned by `object`. The interpreter releases its actual
/// vector capacities only when it destroys the completed frame.
pub fn finish(
    program: &ValidatedProgram,
    state: Option<StateId>,
    object: &ObjectState,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<()> {
    if object.pending.is_some() {
        return Err(vm_error(ErrorCode::InvalidSyntax, offset,
            "object closed before its pending value"));
    }
    budget.charge(1, offset)?;
    let current = object_state(program, state, offset)?;
    match current.map(|current| &current.kind) {
        Some(StateKind::Object { key_table: id, required_count, .. }) => {
            if object.table != Some(*id) { return Err(type_error(offset)); }
            let table = key_table(program, *id, offset)?;
            let entries = table_entries(program, table, offset)?;
            check_mask(object, table, offset)?;
            let mut required = 0u32;
            for (index, entry) in entries.iter().enumerate() {
                budget.charge(1, offset)?;
                if entry.required_bit != NO_REQUIRED_BIT {
                    // This also defends the required numbering invariant at
                    // the point where it matters, without using it as a bit index.
                    if u32::from(entry.required_bit) != required {
                        return Err(internal_error(offset));
                    }
                    required = required.checked_add(1).ok_or_else(|| resource_error(offset))?;
                    if !was_seen(object, index, offset)? {
                        return Err(vm_error(ErrorCode::MissingRequiredField, offset,
                            "required object field is absent").with_schema(
                                program.image().header.schema_id, u32::from(entry.field_id)));
                    }
                }
            }
            if required != u32::from(*required_count) { return Err(internal_error(offset)); }
        }
        Some(StateKind::Any) | None => {
            if object.table.is_some() { return Err(type_error(offset)); }
            if !object.seen_words.is_empty() { return Err(internal_error(offset)); }
        }
        _ => return Err(type_error(offset)),
    }
    if let Some(current) = current {
        let count = object.property_count;
        if current.constraints.min_count.is_some_and(|min| count < min)
            || current.constraints.max_count.is_some_and(|max| count > max)
        {
            return Err(vm_error(ErrorCode::ConstraintViolation, offset,
                "object property count is outside schema bounds"));
        }
    }
    Ok(())
}

fn known_value(
    program: &ValidatedProgram,
    object: &mut ObjectState,
    entry: &KeyEntry,
    index: usize,
    selection: &Selection,
    limits: &VmLimits,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<PendingValue> {
    let repeated = was_seen(object, index, offset)?;
    if repeated && limits.syntax.reject_duplicate_keys {
        return Err(vm_error(ErrorCode::DuplicateObjectKey, offset,
            "known object key occurs more than once"));
    }
    let field = FieldId(u32::from(entry.field_id));
    let target = StateId(entry.target_state);
    checked_state(program, target, offset)?;
    let mut slot = None;
    for spec in &program.program().slots {
        budget.charge(1, offset)?;
        if spec.field_id == field {
            if slot.is_some() || spec.state != target || spec.slot >= program.image().header.slot_count {
                return Err(internal_error(offset));
            }
            slot = Some(spec.slot);
        }
    }
    if repeated && slot.is_some() {
        return Err(vm_error(ErrorCode::ConstraintViolation, offset,
            "a compiled captured field cannot occur more than once"));
    }
    if slot.is_some() && !selection.field_ids.is_empty() {
        let mut selected = false;
        for candidate in &selection.field_ids {
            budget.charge(1, offset)?;
            if *candidate == field {
                selected = true;
                break;
            }
        }
        if !selected { slot = None; }
    }
    let word = object.seen_words.get_mut(index / 64)
        .ok_or_else(|| internal_error(offset))?;
    *word |= 1u64 << (index % 64);
    Ok(PendingValue { state: Some(target), field_id: field, slot })
}

fn checked_state(program: &ValidatedProgram, id: StateId, offset: u32) -> Result<&ProgramState> {
    let index = usize::try_from(id.0).map_err(|_| internal_error(offset))?;
    let state = program.program().states.get(index).ok_or_else(|| internal_error(offset))?;
    if state.id != id { return Err(internal_error(offset)); }
    Ok(state)
}

fn object_state(program: &ValidatedProgram, state: Option<StateId>, offset: u32)
    -> Result<Option<&ProgramState>>
{
    match state {
        Some(id) => {
            let current = checked_state(program, id, offset)?;
            if !matches!(current.kind, StateKind::Any | StateKind::Object { .. }) {
                return Err(type_error(offset));
            }
            Ok(Some(current))
        }
        None => Ok(None),
    }
}

fn key_table(program: &ValidatedProgram, id: u32, offset: u32) -> Result<&KeyTable> {
    let index = usize::try_from(id).map_err(|_| internal_error(offset))?;
    program.program().key_tables.get(index).ok_or_else(|| internal_error(offset))
}

fn table_entries<'a>(program: &'a ValidatedProgram, table: &KeyTable, offset: u32)
    -> Result<&'a [KeyEntry]>
{
    let end = table.first_key.checked_add(table.key_count).ok_or_else(|| internal_error(offset))?;
    let start = usize::try_from(table.first_key).map_err(|_| internal_error(offset))?;
    let end = usize::try_from(end).map_err(|_| internal_error(offset))?;
    program.image().keys.get(start..end).ok_or_else(|| internal_error(offset))
}

fn word_count(keys: u32, offset: u32) -> Result<usize> {
    // Widen before rounding so even u32::MAX is handled without overflow.
    let rounded = u64::from(keys).checked_add(63).ok_or_else(|| resource_error(offset))? / 64;
    usize::try_from(rounded).map_err(|_| resource_error(offset))
}

fn check_mask(object: &ObjectState, table: &KeyTable, offset: u32) -> Result<()> {
    if object.seen_words.len() != word_count(table.key_count, offset)? {
        return Err(internal_error(offset));
    }
    Ok(())
}

fn was_seen(object: &ObjectState, index: usize, offset: u32) -> Result<bool> {
    let word = object.seen_words.get(index / 64).ok_or_else(|| internal_error(offset))?;
    Ok(word & (1u64 << (index % 64)) != 0)
}

fn rejects_unknown(program: &ValidatedProgram, table_id: u32, budget: &mut VmBudget, offset: u32)
    -> Result<bool>
{
    for state in &program.program().states {
        budget.charge(1, offset)?;
        if let StateKind::Object { key_table, reject_unknown, .. } = &state.kind {
            if *key_table == table_id { return Ok(*reject_unknown); }
        }
    }
    Err(internal_error(offset))
}

/// lookup has no budget parameter. Prepay for table/candidate validation,
/// hashing, SWAR heads, and worst-case complete byte comparisons before calling it.
fn charge_lookup(table: &KeyTable, entries: &[KeyEntry], key_len: u32,
    budget: &mut VmBudget, offset: u32) -> Result<()>
{
    // The compiler's raw-image lookup initializes a fixed field bitmap even
    // for empty tables. Four byte passes cover UTF-8, ordering, SWAR heads and
    // stored-key hashing; query comparisons are charged separately per entry.
    let fixed = u64::from(LOOKUP_SCRATCH_BYTES).checked_add(8)
        .and_then(|v| v.checked_add(u64::from(key_len))).ok_or_else(|| resource_error(offset))?;
    budget.charge(fixed, offset)?;
    for entry in entries {
        let work = u64::from(entry.span.len).checked_mul(4)
            .and_then(|v| v.checked_add(u64::from(key_len)))
            .and_then(|v| v.checked_add(16)).ok_or_else(|| resource_error(offset))?;
        budget.charge(work, offset)?;
    }
    if let KeyStrategy::PerfectHash { slots, .. } = &table.strategy {
        let work = u64::try_from(slots.len()).ok().and_then(|v| v.checked_mul(4))
            .ok_or_else(|| resource_error(offset))?;
        budget.charge(work, offset)?;
    }
    Ok(())
}

fn retain_unknown(object: &mut ObjectState, key: &[u8], budget: &mut VmBudget, offset: u32)
    -> Result<()>
{
    for previous in &object.unknown_keys {
        budget.charge(1, offset)?;
        if previous.len() == key.len() {
            budget.charge(u64::try_from(key.len()).map_err(|_| resource_error(offset))?, offset)?;
            if previous.as_slice() == key {
                return Err(vm_error(ErrorCode::DuplicateObjectKey, offset,
                    "unknown decoded object key occurs more than once"));
            }
        }
    }
    let needed = object.unknown_keys.len().checked_add(1).ok_or_else(|| resource_error(offset))?;
    // Grow first so a later name-allocation failure leaves only fully accounted
    // capacity in the object, with no partially inserted name or pending value.
    if needed > object.unknown_keys.capacity() {
        let old_bytes = allocation_bytes::<Vec<u8>>(object.unknown_keys.capacity(), offset)?;
        let mut replacement = allocate::<Vec<u8>>(needed, budget, offset)?;
        replacement.append(&mut object.unknown_keys);
        let old = core::mem::replace(&mut object.unknown_keys, replacement);
        drop(old);
        budget.release(old_bytes)?;
    }
    let mut name = allocate::<u8>(key.len(), budget, offset)?;
    name.extend_from_slice(key);
    object.unknown_keys.push(name);
    Ok(())
}

fn allocation_bytes<T>(capacity: usize, offset: u32) -> Result<u32> {
    capacity.checked_mul(size_of::<T>()).and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| resource_error(offset))
}

/// Return a fallibly allocated empty vector with its actual capacity charged.
/// Roll back scratch on failure; consumed work is never refunded.
fn allocate<T>(capacity: usize, budget: &mut VmBudget, offset: u32) -> Result<Vec<T>> {
    let requested = allocation_bytes::<T>(capacity, offset)?;
    budget.charge(u64::from(requested), offset)?;
    budget.retain(requested, offset)?;
    let mut values = Vec::new();
    if values.try_reserve_exact(capacity).is_err() {
        drop(values);
        budget.release(requested)?;
        return Err(resource_error(offset));
    }
    let excess = allocation_bytes::<T>(values.capacity(), offset)
        .and_then(|actual| actual.checked_sub(requested).ok_or_else(|| internal_error(offset)));
    let result = excess.and_then(|excess| budget.retain(excess, offset));
    if let Err(error) = result {
        drop(values);
        budget.release(requested)?;
        return Err(error);
    }
    Ok(values)
}

fn resource_error(offset: u32) -> Error {
    vm_error(ErrorCode::BudgetExhausted, offset, "object work, scratch, or count limit exceeded")
}

fn internal_error(offset: u32) -> Error {
    vm_error(ErrorCode::InternalError, offset, "inconsistent object state or validated key table")
}

fn type_error(offset: u32) -> Error {
    vm_error(ErrorCode::TypeMismatch, offset, "schema state does not match this object")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::String, vec};
    use jmax_schema_compile::CompilerOptions;
    use jmax_schema_ir::{ArrayShape, ObjectShape, Property, ResolveLimits,
        ScalarConstraints, Schema, SchemaId, TypeId, TypeNode};

    fn selection() -> Selection {
        Selection { field_ids: Vec::new(), strict_validate_unselected: true }
    }

    fn compile_with(nodes: Vec<TypeNode>, options: CompilerOptions) -> ValidatedProgram {
        let schema = Schema {
            name: String::from("object-tests"), schema_id: SchemaId(218),
            root: TypeId(0), nodes, definitions: Vec::new(),
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits::default()).unwrap();
        jmax_schema_compile::lower::compile(&resolved, &options).unwrap()
    }

    fn compile(nodes: Vec<TypeNode>) -> ValidatedProgram {
        compile_with(nodes, CompilerOptions::default())
    }

    fn property(name: &str, field: u32, required: bool, slot: Option<u32>) -> Property {
        Property { name: String::from(name), field_id: FieldId(field), ty: TypeId(1),
            required, slot }
    }

    fn shape(properties: Vec<Property>, reject_unknown: bool,
        min: Option<u32>, max: Option<u32>) -> TypeNode
    {
        TypeNode::Object(ObjectShape { properties, reject_unknown,
            min_properties: min, max_properties: max })
    }

    fn object_program(properties: Vec<Property>, reject_unknown: bool) -> ValidatedProgram {
        compile(vec![shape(properties, reject_unknown, None, None), TypeNode::Any])
    }

    fn root(program: &ValidatedProgram) -> Option<StateId> {
        Some(StateId(program.image().header.root_state))
    }

    fn start(program: &ValidatedProgram, budget: &mut VmBudget) -> ObjectState {
        begin(program, root(program), budget, 0).unwrap()
    }

    fn member(program: &ValidatedProgram, object: &mut ObjectState, name: &str,
        limits: &VmLimits, budget: &mut VmBudget) -> PendingValue
    {
        let pending = key(program, object, name, &selection(), limits, budget, 7).unwrap();
        assert_eq!(object.pending, Some(pending));
        assert_eq!(take_value(object, 11).unwrap(), pending);
        assert_eq!(object.pending, None);
        pending
    }

    fn assert_error(error: Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaVm);
        assert_eq!(error.byte_offset, offset);
    }

    fn retained(object: &ObjectState) -> u32 {
        let mut bytes = allocation_bytes::<u64>(object.seen_words.capacity(), 0).unwrap()
            + allocation_bytes::<Vec<u8>>(object.unknown_keys.capacity(), 0).unwrap();
        for name in &object.unknown_keys {
            bytes += allocation_bytes::<u8>(name.capacity(), 0).unwrap();
        }
        bytes
    }

    #[test]
    fn seen_word_rounding_includes_optional_entries_and_word_boundaries() {
        for (count, words) in [(0, 0), (1, 1), (63, 1), (64, 1), (65, 2), (129, 3)] {
            let properties = (0..count).map(|i| property(&format!("k{i:03}"), i + 1, false, None))
                .collect();
            let program = object_program(properties, false);
            let limits = VmLimits::default();
            let mut budget = VmBudget::new(limits);
            let mut object = start(&program, &mut budget);
            assert_eq!(object.seen_words.len(), words);
            assert!(object.seen_words.iter().all(|&word| word == 0));
            assert_eq!(budget.scratch_bytes(), retained(&object));
            assert_eq!(object.property_count, 0);
            assert!(object.unknown_keys.is_empty());
            for i in 0..count {
                member(&program, &mut object, &format!("k{i:03}"), &limits, &mut budget);
                assert!(was_seen(&object, usize::try_from(i).unwrap(), 0).unwrap());
            }
            if count == 65 { assert_eq!(object.seen_words, vec![u64::MAX, 1]); }
            if count == 129 { assert_eq!(object.seen_words, vec![u64::MAX, u64::MAX, 1]); }
            finish(&program, root(&program), &object, &mut budget, 99).unwrap();
        }
        assert_eq!(word_count(u32::MAX, 0).unwrap(), 67_108_864);
    }

    #[test]
    fn any_and_none_need_no_seen_scratch_or_known_key_whitelist() {
        let program = compile(vec![TypeNode::Any]);
        let mut limits = VmLimits::default();
        limits.max_scratch_bytes = 0;
        limits.syntax.reject_duplicate_keys = false;
        assert!(limits.syntax.reject_unknown_keys);
        for state in [None, root(&program)] {
            let mut budget = VmBudget::new(limits);
            let mut object = begin(&program, state, &mut budget, 0).unwrap();
            assert_eq!(object.table, None);
            assert!(object.seen_words.is_empty());
            let pending = member(&program, &mut object, "arbitrary", &limits, &mut budget);
            assert_eq!(pending, PendingValue { state: None, field_id: FieldId(0), slot: None });
            assert!(object.unknown_keys.is_empty());
            finish(&program, state, &object, &mut budget, 20).unwrap();
            assert_eq!(budget.scratch_bytes(), 0);
        }
    }

    #[test]
    fn scalar_and_array_states_cannot_begin_or_finish_objects() {
        for program in [
            compile(vec![TypeNode::Scalar(ScalarConstraints::new(ValueKind::Null))]),
            compile(vec![TypeNode::Array(ArrayShape {
                items: TypeId(1), min_items: None, max_items: None,
            }), TypeNode::Any]),
        ] {
            let mut budget = VmBudget::new(VmLimits::default());
            assert_error(begin(&program, root(&program), &mut budget, 19).unwrap_err(),
                ErrorCode::TypeMismatch, 19);
            let object = begin(&program, None, &mut budget, 0).unwrap();
            assert_error(finish(&program, root(&program), &object, &mut budget, 21).unwrap_err(),
                ErrorCode::TypeMismatch, 21);
            assert_error(begin(&program, Some(StateId(u32::MAX)), &mut budget, 23).unwrap_err(),
                ErrorCode::InternalError, 23);
        }
    }

    #[test]
    fn required_bits_are_not_entry_indices() {
        let program = object_program(vec![property("a", 10, false, None),
            property("b", 20, true, None), property("c", 30, false, None),
            property("d", 40, true, None)], false);
        let mut limits = VmLimits::default();
        limits.syntax.reject_duplicate_keys = false;
        limits.syntax.reject_unknown_keys = false;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        member(&program, &mut object, "a", &limits, &mut budget);
        member(&program, &mut object, "c", &limits, &mut budget);
        assert_eq!(object.seen_words, vec![0b0101]);
        let error = finish(&program, root(&program), &object, &mut budget, 40).unwrap_err();
        assert_eq!((error.schema_id, error.field_id), (218, 20));
        assert_error(error, ErrorCode::MissingRequiredField, 40);
        member(&program, &mut object, "b", &limits, &mut budget);
        let error = finish(&program, root(&program), &object, &mut budget, 50).unwrap_err();
        assert_eq!(error.field_id, 40);
        assert_error(error, ErrorCode::MissingRequiredField, 50);
        member(&program, &mut object, "d", &limits, &mut budget);
        assert_eq!(object.seen_words, vec![0b1111]);
        finish(&program, root(&program), &object, &mut budget, 60).unwrap();
    }

    #[test]
    fn required_entry_across_a_word_boundary_is_independent_of_optional_keys() {
        let properties = (0..65).map(|i| property(&format!("k{i:03}"), i + 1, i == 64, None))
            .collect();
        let program = object_program(properties, false);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        member(&program, &mut object, "k000", &limits, &mut budget);
        let error = finish(&program, root(&program), &object, &mut budget, 90).unwrap_err();
        assert_eq!(error.field_id, 65);
        assert_error(error, ErrorCode::MissingRequiredField, 90);
        member(&program, &mut object, "k064", &limits, &mut budget);
        assert_eq!(object.seen_words, vec![1, 1]);
        finish(&program, root(&program), &object, &mut budget, 91).unwrap();
    }

    #[test]
    fn recursive_instances_do_not_share_required_membership() {
        let mut next = property("next", 1, false, None);
        next.ty = TypeId(0);
        let program = object_program(vec![next, property("value", 2, true, None)], true);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut parent = start(&program, &mut budget);
        member(&program, &mut parent, "value", &limits, &mut budget);
        let child_value = member(&program, &mut parent, "next", &limits, &mut budget);
        assert_eq!(child_value.state, root(&program));
        let mut child = begin(&program, child_value.state, &mut budget, 12).unwrap();
        let error = finish(&program, child_value.state, &child, &mut budget, 13).unwrap_err();
        assert_eq!(error.field_id, 2);
        assert_error(error, ErrorCode::MissingRequiredField, 13);
        member(&program, &mut child, "value", &limits, &mut budget);
        finish(&program, child_value.state, &child, &mut budget, 30).unwrap();
        finish(&program, root(&program), &parent, &mut budget, 31).unwrap();
        assert_eq!(parent.seen_words, vec![3]);
        assert_eq!(child.seen_words, vec![2]);
        assert_eq!(budget.scratch_bytes(), retained(&parent) + retained(&child));
    }

    #[test]
    fn optional_duplicates_are_checked_and_uncaptured_repeats_can_be_allowed() {
        let program = object_program(vec![property("optional", 7, false, None)], false);
        for reject in [true, false] {
            let mut limits = VmLimits::default();
            limits.syntax.reject_duplicate_keys = reject;
            let mut budget = VmBudget::new(limits);
            let mut object = start(&program, &mut budget);
            member(&program, &mut object, "optional", &limits, &mut budget);
            let result = key(&program, &mut object, "optional", &selection(), &limits, &mut budget, 33);
            if reject {
                let error = result.unwrap_err();
                assert_eq!((error.schema_id, error.field_id), (218, 7));
                assert_error(error, ErrorCode::DuplicateObjectKey, 33);
                assert_eq!(object.property_count, 1);
                assert!(object.pending.is_none());
            } else {
                assert_eq!(result.unwrap().slot, None);
                take_value(&mut object, 34).unwrap();
                assert_eq!(object.property_count, 2);
                finish(&program, root(&program), &object, &mut budget, 35).unwrap();
            }
        }
    }

    #[test]
    fn captured_duplicates_fail_even_when_the_field_is_unselected() {
        let program = object_program(vec![property("a", 11, false, Some(0)),
            property("b", 22, false, Some(1))], false);
        for selected in [selection(), Selection {
            field_ids: vec![FieldId(22)], strict_validate_unselected: true,
        }] {
            check_selection(&program, &selected).unwrap();
            for reject in [true, false] {
                let mut limits = VmLimits::default();
                limits.syntax.reject_duplicate_keys = reject;
                let mut budget = VmBudget::new(limits);
                let mut object = start(&program, &mut budget);
                let pending = key(&program, &mut object, "a", &selected, &limits, &mut budget, 4).unwrap();
                assert_eq!(pending.field_id, FieldId(11));
                assert_eq!(pending.slot, if selected.field_ids.is_empty() { Some(0) } else { None });
                take_value(&mut object, 5).unwrap();
                let error = key(&program, &mut object, "a", &selected, &limits, &mut budget, 9).unwrap_err();
                assert_eq!(error.field_id, 11);
                assert_error(error, if reject { ErrorCode::DuplicateObjectKey }
                    else { ErrorCode::ConstraintViolation }, 9);
                assert!(object.pending.is_none());
                assert_eq!(object.property_count, 1);
            }
        }
    }

    #[test]
    fn selection_controls_only_slots_and_required_checks_still_run() {
        let program = object_program(vec![property("a", 11, true, Some(0)),
            property("b", 22, false, Some(1)), property("c", 33, false, None)], false);
        let selected = Selection { field_ids: vec![FieldId(22)], strict_validate_unselected: true };
        check_selection(&program, &selected).unwrap();
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        let b = key(&program, &mut object, "b", &selected, &limits, &mut budget, 1).unwrap();
        assert_eq!((b.field_id, b.slot), (FieldId(22), Some(1)));
        take_value(&mut object, 2).unwrap();
        let error = finish(&program, root(&program), &object, &mut budget, 3).unwrap_err();
        assert_eq!(error.field_id, 11);
        assert_error(error, ErrorCode::MissingRequiredField, 3);
        let a = key(&program, &mut object, "a", &selected, &limits, &mut budget, 4).unwrap();
        assert_eq!((a.field_id, a.slot), (FieldId(11), None));
        assert!(a.state.is_some());
        take_value(&mut object, 5).unwrap();
        assert_eq!(member(&program, &mut object, "c", &limits, &mut budget).slot, None);
        finish(&program, root(&program), &object, &mut budget, 9).unwrap();
        let invalid = Selection { field_ids: vec![FieldId(33)], strict_validate_unselected: true };
        assert_eq!(check_selection(&program, &invalid).unwrap_err().code, ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn either_unknown_rejection_policy_wins_without_retaining_the_name() {
        for compiled in [false, true] {
            for runtime in [false, true] {
                let program = object_program(Vec::new(), compiled);
                let mut limits = VmLimits::default();
                limits.syntax.reject_unknown_keys = runtime;
                let mut budget = VmBudget::new(limits);
                let mut object = start(&program, &mut budget);
                let result = key(&program, &mut object, "x", &selection(), &limits, &mut budget, 8);
                if compiled || runtime {
                    assert_error(result.unwrap_err(), ErrorCode::UnknownObjectKey, 8);
                    assert_eq!(budget.scratch_bytes(), 0);
                    assert!(object.unknown_keys.is_empty());
                    assert!(object.pending.is_none());
                    assert_eq!(object.property_count, 0);
                } else {
                    assert_eq!(result.unwrap(), PendingValue { state: None, field_id: FieldId(0), slot: None });
                    assert_eq!(object.unknown_keys, vec![b"x".to_vec()]);
                }
            }
        }
    }

    #[test]
    fn unknown_names_compare_decoded_bytes_without_unicode_normalization() {
        let program = compile(vec![TypeNode::Any]);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        for name in ["", "a", "é", "e\u{301}", "\0", "abcdefgh-one", "abcdefgh-two", "😀"] {
            member(&program, &mut object, name, &limits, &mut budget);
        }
        assert_eq!(object.property_count, 8);
        assert_eq!(object.unknown_keys.len(), 8);
        // A literal 'a' and a JSON escaped U+0061 both arrive here as "a".
        let decoded_escape = String::from("\u{61}");
        let error = key(&program, &mut object, &decoded_escape, &selection(), &limits,
            &mut budget, 100).unwrap_err();
        assert_error(error, ErrorCode::DuplicateObjectKey, 100);
        assert_eq!(object.property_count, 8);
        assert_eq!(budget.scratch_bytes(), retained(&object));
        for name in ["", "é", "\0", "😀"] {
            assert_error(key(&program, &mut object, name, &selection(), &limits,
                &mut budget, 101).unwrap_err(), ErrorCode::DuplicateObjectKey, 101);
        }
    }

    #[test]
    fn unknown_repeats_need_no_retained_names_when_duplicates_are_allowed() {
        let program = compile(vec![TypeNode::Any]);
        let mut limits = VmLimits::default();
        limits.syntax.reject_duplicate_keys = false;
        limits.syntax.reject_unknown_keys = false;
        limits.max_scratch_bytes = 0;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        for _ in 0..3 { member(&program, &mut object, "same", &limits, &mut budget); }
        assert_eq!(object.property_count, 3);
        assert!(object.unknown_keys.is_empty());
        assert_eq!(budget.scratch_bytes(), 0);
        finish(&program, root(&program), &object, &mut budget, 99).unwrap();
    }

    #[test]
    fn pending_value_order_is_enforced_without_consuming_or_double_counting() {
        let program = object_program(vec![property("a", 9, true, None)], false);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_error(take_value(&mut object, 1).unwrap_err(), ErrorCode::InvalidSyntax, 1);
        let pending = key(&program, &mut object, "a", &selection(), &limits, &mut budget, 2).unwrap();
        assert_error(key(&program, &mut object, "a", &selection(), &limits,
            &mut budget, 3).unwrap_err(), ErrorCode::InvalidSyntax, 3);
        assert_error(finish(&program, root(&program), &object, &mut budget, 4).unwrap_err(),
            ErrorCode::InvalidSyntax, 4);
        assert_eq!(object.property_count, 1);
        assert_eq!(object.pending, Some(pending));
        assert_eq!(take_value(&mut object, 5).unwrap(), pending);
        assert_error(take_value(&mut object, 6).unwrap_err(), ErrorCode::InvalidSyntax, 6);
        assert_eq!(object.property_count, 1);
        finish(&program, root(&program), &object, &mut budget, 7).unwrap();
    }

    #[test]
    fn runtime_property_limit_is_inclusive_and_overflow_does_not_mutate_count() {
        let program = compile(vec![TypeNode::Any]);
        let mut limits = VmLimits::default();
        limits.syntax.max_fields_per_object = 1;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        member(&program, &mut object, "a", &limits, &mut budget);
        assert_error(key(&program, &mut object, "b", &selection(), &limits,
            &mut budget, 15).unwrap_err(), ErrorCode::BudgetExhausted, 15);
        assert_eq!(object.property_count, 1);
        assert!(object.pending.is_none());
        finish(&program, root(&program), &object, &mut budget, 16).unwrap();
        limits.syntax.max_fields_per_object = 0;
        let mut empty = start(&program, &mut budget);
        finish(&program, root(&program), &empty, &mut budget, 17).unwrap();
        assert_error(key(&program, &mut empty, "", &selection(), &limits,
            &mut budget, 18).unwrap_err(), ErrorCode::BudgetExhausted, 18);
        limits.syntax.max_fields_per_object = u32::MAX;
        empty.property_count = u32::MAX;
        assert_error(key(&program, &mut empty, "", &selection(), &limits,
            &mut budget, u32::MAX).unwrap_err(), ErrorCode::BudgetExhausted, u32::MAX);
        assert_eq!(empty.property_count, u32::MAX);
    }

    #[test]
    fn property_constraints_count_unknowns_and_permitted_repeats() {
        let program = compile(vec![shape(Vec::new(), false, Some(1), Some(2))]);
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        limits.syntax.reject_duplicate_keys = false;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_error(finish(&program, root(&program), &object, &mut budget, 10).unwrap_err(),
            ErrorCode::ConstraintViolation, 10);
        for count in 1..=2 {
            member(&program, &mut object, "same", &limits, &mut budget);
            assert_eq!(object.property_count, count);
            finish(&program, root(&program), &object, &mut budget, 20).unwrap();
        }
        member(&program, &mut object, "same", &limits, &mut budget);
        assert_error(finish(&program, root(&program), &object, &mut budget, 30).unwrap_err(),
            ErrorCode::ConstraintViolation, 30);
        let zero = compile(vec![shape(Vec::new(), false, Some(0), Some(0))]);
        let object = start(&zero, &mut budget);
        finish(&zero, root(&zero), &object, &mut budget, 31).unwrap();
    }

    #[test]
    fn key_byte_limits_are_inclusive_and_empty_keys_are_valid() {
        let program = object_program(vec![property("", 1, false, None),
            property("é", 2, false, None)], false);
        let mut limits = VmLimits::default();
        limits.syntax.max_string_bytes = 0;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_eq!(member(&program, &mut object, "", &limits, &mut budget).field_id, FieldId(1));
        limits.syntax.max_string_bytes = 1;
        assert_error(key(&program, &mut object, "é", &selection(), &limits,
            &mut budget, 22).unwrap_err(), ErrorCode::BudgetExhausted, 22);
        assert_eq!(object.property_count, 1);
        limits.syntax.max_string_bytes = 2;
        assert_eq!(member(&program, &mut object, "é", &limits, &mut budget).field_id, FieldId(2));
        finish(&program, root(&program), &object, &mut budget, 23).unwrap();
    }

    #[test]
    fn same_swar_prefix_never_aliases_distinct_known_or_unknown_keys() {
        let program = object_program(vec![property("abcdefgh-one", 1, true, None),
            property("abcdefgh-two", 2, true, None)], false);
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_eq!(member(&program, &mut object, "abcdefgh-two", &limits, &mut budget).field_id, FieldId(2));
        assert_eq!(member(&program, &mut object, "abcdefgh-xxx", &limits, &mut budget).state, None);
        let error = finish(&program, root(&program), &object, &mut budget, 44).unwrap_err();
        assert_eq!(error.field_id, 1);
        assert_error(error, ErrorCode::MissingRequiredField, 44);
        member(&program, &mut object, "abcdefgh-one", &limits, &mut budget);
        finish(&program, root(&program), &object, &mut budget, 45).unwrap();
    }

    #[test]
    fn perfect_hash_lookup_and_unknown_hash_collisions_use_exact_names() {
        let mut options = CompilerOptions::default();
        options.swar_threshold = 0;
        let program = compile_with(vec![shape(vec![property("a", 9, true, None)], false,
            None, None), TypeNode::Any], options);
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        let table = key_table(&program, object.table.unwrap(), 0).unwrap();
        let (seed, slots) = match &table.strategy {
            KeyStrategy::PerfectHash { seed, slots } => (*seed, slots),
            _ => panic!("a single key always admits a perfect hash"),
        };
        let mask = u64::try_from(slots.len() - 1).unwrap();
        let bucket = jmax_schema_compile::key_hash(b"a", seed) & mask;
        let collision = (b'b'..=b'z').find(|&byte|
            jmax_schema_compile::key_hash(&[byte], seed) & mask == bucket).unwrap();
        let name = [collision];
        let name = core::str::from_utf8(&name).unwrap();
        assert_eq!(member(&program, &mut object, name, &limits, &mut budget).state, None);
        assert_eq!(object.seen_words, vec![0]);
        assert_eq!(member(&program, &mut object, "a", &limits, &mut budget).field_id, FieldId(9));
        finish(&program, root(&program), &object, &mut budget, 30).unwrap();
    }

    #[test]
    fn nested_tables_use_local_seen_indices_and_keep_their_own_unknown_policy() {
        let program = compile(vec![shape(vec![property("child", 1, false, None)], false, None, None),
            shape(vec![Property { ty: TypeId(2), ..property("v", 2, true, None) }],
                true, None, None), TypeNode::Any]);
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let mut budget = VmBudget::new(limits);
        let mut parent = start(&program, &mut budget);
        let pending = member(&program, &mut parent, "child", &limits, &mut budget);
        let mut child = begin(&program, pending.state, &mut budget, 10).unwrap();
        assert_ne!(parent.table, child.table);
        assert!(key_table(&program, child.table.unwrap(), 0).unwrap().first_key > 0);
        assert_error(key(&program, &mut child, "unknown", &selection(), &limits,
            &mut budget, 12).unwrap_err(), ErrorCode::UnknownObjectKey, 12);
        let value = member(&program, &mut child, "v", &limits, &mut budget);
        assert_eq!(value.field_id, FieldId(2));
        assert_eq!(child.seen_words, vec![1]);
        assert_error(finish(&program, root(&program), &child, &mut budget, 13).unwrap_err(),
            ErrorCode::TypeMismatch, 13);
        finish(&program, pending.state, &child, &mut budget, 14).unwrap();
        member(&program, &mut parent, "unknown", &limits, &mut budget);
        finish(&program, root(&program), &parent, &mut budget, 15).unwrap();
    }

    #[test]
    fn caller_modified_object_indices_and_masks_fail_without_panicking() {
        let program = object_program(vec![property("a", 1, true, None)], false);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        object.seen_words.clear();
        assert_error(key(&program, &mut object, "a", &selection(), &limits,
            &mut budget, 7).unwrap_err(), ErrorCode::InternalError, 7);
        assert_error(finish(&program, root(&program), &object, &mut budget, 8).unwrap_err(),
            ErrorCode::InternalError, 8);
        object.table = Some(u32::MAX);
        assert_error(key(&program, &mut object, "a", &selection(), &limits,
            &mut budget, 9).unwrap_err(), ErrorCode::InternalError, 9);
        assert_error(finish(&program, None, &object, &mut budget, 10).unwrap_err(),
            ErrorCode::TypeMismatch, 10);
        object.table = None;
        object.seen_words.push(0);
        assert_error(key(&program, &mut object, "x", &selection(), &limits,
            &mut budget, 11).unwrap_err(), ErrorCode::InternalError, 11);
        assert_error(finish(&program, None, &object, &mut budget, 12).unwrap_err(),
            ErrorCode::InternalError, 12);
        let bad_table = KeyTable { first_key: u32::MAX, key_count: 1, strategy: KeyStrategy::Swar };
        assert_error(table_entries(&program, &bad_table, 13).unwrap_err(), ErrorCode::InternalError, 13);
        let bad_table = KeyTable { first_key: 0, key_count: 2, strategy: KeyStrategy::Swar };
        assert_error(table_entries(&program, &bad_table, 14).unwrap_err(), ErrorCode::InternalError, 14);
    }

    #[test]
    fn seen_scratch_limits_accept_exact_capacity_and_reject_one_byte_less() {
        let program = object_program(vec![property("a", 1, false, None)], false);
        let mut limits = VmLimits::default();
        let mut measured = VmBudget::new(limits);
        let object = start(&program, &mut measured);
        let bytes = retained(&object);
        assert!(bytes >= 8);
        limits.max_scratch_bytes = bytes;
        let mut exact = VmBudget::new(limits);
        let object = start(&program, &mut exact);
        assert_eq!(exact.scratch_bytes(), bytes);
        finish(&program, root(&program), &object, &mut exact, 8).unwrap();
        assert_eq!(exact.scratch_bytes(), bytes);
        drop(object);
        exact.release(bytes).unwrap();
        assert_eq!(exact.scratch_bytes(), 0);
        limits.max_scratch_bytes = bytes - 1;
        let mut tight = VmBudget::new(limits);
        assert_error(begin(&program, root(&program), &mut tight, 9).unwrap_err(),
            ErrorCode::BudgetExhausted, 9);
        assert_eq!(tight.scratch_bytes(), 0);
    }

    #[test]
    fn retained_unknown_scratch_counts_headers_empty_names_and_capacity_growth() {
        let program = compile(vec![TypeNode::Any]);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        for name in ["", "a", "é", "longer-name"] {
            member(&program, &mut object, name, &limits, &mut budget);
            assert_eq!(budget.scratch_bytes(), retained(&object));
        }
        let bytes = retained(&object);
        assert!(bytes >= u32::try_from(4 * size_of::<Vec<u8>>() + 14).unwrap());
        finish(&program, root(&program), &object, &mut budget, 50).unwrap();
        assert_eq!(budget.scratch_bytes(), bytes);
        drop(object);
        budget.release(bytes).unwrap();
        assert_eq!(budget.scratch_bytes(), 0);
    }

    #[test]
    fn unknown_allocation_failure_keeps_accounted_capacity_and_no_pending_key() {
        let program = compile(vec![TypeNode::Any]);
        let mut limits = VmLimits::default();
        limits.max_scratch_bytes = 0;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_error(key(&program, &mut object, "", &selection(), &limits,
            &mut budget, 3).unwrap_err(), ErrorCode::BudgetExhausted, 3);
        assert_eq!(budget.scratch_bytes(), 0);
        assert!(object.unknown_keys.is_empty());
        // Enough room for the outer Vec header, but not the nonempty name.
        limits.max_scratch_bytes = u32::try_from(size_of::<Vec<u8>>()).unwrap();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_error(key(&program, &mut object, "x", &selection(), &limits,
            &mut budget, 4).unwrap_err(), ErrorCode::BudgetExhausted, 4);
        assert_eq!(object.property_count, 0);
        assert!(object.pending.is_none());
        assert!(object.unknown_keys.is_empty());
        assert_eq!(budget.scratch_bytes(), retained(&object));
        let bytes = retained(&object);
        drop(object);
        budget.release(bytes).unwrap();
        assert_eq!(budget.scratch_bytes(), 0);
    }

    #[test]
    fn lookup_temporary_bitmap_is_bounded_and_released_after_each_call() {
        let program = object_program(vec![property("a", 1, false, None)], false);
        let mut limits = VmLimits::default();
        let mut measured = VmBudget::new(limits);
        let object = start(&program, &mut measured);
        let seen_bytes = retained(&object);
        drop(object);
        limits.max_scratch_bytes = seen_bytes + LOOKUP_SCRATCH_BYTES;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        member(&program, &mut object, "a", &limits, &mut budget);
        assert_eq!(budget.scratch_bytes(), seen_bytes);
        assert_error(key(&program, &mut object, "unknown", &selection(), &limits,
            &mut budget, 31).unwrap_err(), ErrorCode::UnknownObjectKey, 31);
        assert_eq!(budget.scratch_bytes(), seen_bytes);
        limits.max_scratch_bytes -= 1;
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        assert_error(key(&program, &mut object, "a", &selection(), &limits,
            &mut budget, 32).unwrap_err(), ErrorCode::BudgetExhausted, 32);
        assert_eq!(budget.scratch_bytes(), seen_bytes);
        assert_eq!(object.property_count, 0);
        assert!(object.pending.is_none());
        assert_eq!(object.seen_words, vec![0]);
    }

    #[test]
    fn complete_sequence_uses_one_shared_work_budget_with_an_exact_boundary() {
        let program = object_program(vec![property("a", 1, true, Some(0)),
            property("b", 2, false, Some(1))], false);
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let selected = Selection { field_ids: vec![FieldId(2)], strict_validate_unselected: true };
        let sequence = |budget: &mut VmBudget| -> Result<()> {
            let mut object = begin(&program, root(&program), budget, 0)?;
            key(&program, &mut object, "a", &selected, &limits, budget, 1)?;
            take_value(&mut object, 2)?;
            key(&program, &mut object, "x", &selected, &limits, budget, 3)?;
            take_value(&mut object, 4)?;
            finish(&program, root(&program), &object, budget, 5)
        };
        let mut measured = VmBudget::new(limits);
        sequence(&mut measured).unwrap();
        let used = limits.max_steps - measured.steps_left();
        assert!(used > 10);
        let mut exact = VmBudget::new(VmLimits { max_steps: used, ..limits });
        sequence(&mut exact).unwrap();
        assert_eq!(exact.steps_left(), 0);
        let mut tight = VmBudget::new(VmLimits { max_steps: used - 1, ..limits });
        assert_error(sequence(&mut tight).unwrap_err(), ErrorCode::BudgetExhausted, 5);
    }

    #[test]
    fn every_budgeted_entry_point_rejects_zero_work_and_retains_no_new_state() {
        let program = object_program(vec![property("a", 1, false, None)], false);
        let limits = VmLimits::default();
        let mut ample = VmBudget::new(limits);
        let mut object = start(&program, &mut ample);
        let mut empty = VmBudget::new(VmLimits { max_steps: 0, ..limits });
        assert_error(begin(&program, root(&program), &mut empty, 17).unwrap_err(),
            ErrorCode::BudgetExhausted, 17);
        assert_error(key(&program, &mut object, "a", &selection(), &limits,
            &mut empty, 18).unwrap_err(), ErrorCode::BudgetExhausted, 18);
        assert_error(finish(&program, root(&program), &object, &mut empty, 19).unwrap_err(),
            ErrorCode::BudgetExhausted, 19);
        assert_eq!(object.property_count, 0);
        assert!(object.pending.is_none());
        assert_eq!(object.seen_words, vec![0]);
    }

    #[test]
    fn unknown_comparison_budget_cannot_turn_exhaustion_into_a_miss() {
        let program = compile(vec![TypeNode::Any]);
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        let mut object = start(&program, &mut budget);
        member(&program, &mut object, "abcdefgh", &limits, &mut budget);
        let scratch = budget.scratch_bytes();
        budget.steps_left = 1 + 8 + 1; // Key dispatch, bytes, comparison header.
        assert_error(key(&program, &mut object, "abcdefgh", &selection(), &limits,
            &mut budget, 40).unwrap_err(), ErrorCode::BudgetExhausted, 40);
        assert_eq!(object.property_count, 1);
        assert_eq!(object.unknown_keys.len(), 1);
        assert!(object.pending.is_none());
        assert_eq!(budget.scratch_bytes(), scratch);
        assert_eq!(budget.steps_left(), 0);
    }

    #[test]
    fn checked_allocation_arithmetic_fails_before_attempting_huge_allocations() {
        assert_error(allocation_bytes::<u64>(usize::MAX, 9).unwrap_err(),
            ErrorCode::BudgetExhausted, 9);
        let mut budget = VmBudget::new(VmLimits::default());
        let before = budget.steps_left();
        assert_error(allocate::<u64>(usize::MAX, &mut budget, 10).unwrap_err(),
            ErrorCode::BudgetExhausted, 10);
        assert_eq!(budget.scratch_bytes(), 0);
        assert_eq!(budget.steps_left(), before);
        let empty = allocate::<u64>(0, &mut budget, 11).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.capacity(), 0);
        assert_eq!(budget.scratch_bytes(), 0);
    }
}
