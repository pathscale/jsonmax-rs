//! Per-object key dispatch, duplicate tracking, and required-field validation.
//!
//! Seen bits address entries within a key table, not dense required-field IDs.
//! Each invocation owns its scratch, including for recursive schema instances.
use super::*;
use core::mem::size_of;
use jmax_abi::schema::KeyEntry;
use jmax_schema_compile::{keys, KeyStrategy, NO_REQUIRED_BIT};

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
    selection: &extract::SelectionIndex,
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
        charge_lookup(table, key_len, budget, offset)?;
        if let Some(global) = keys::lookup_index(program.image(), table, key.as_bytes())? {
            let local = global.checked_sub(table.first_key).ok_or_else(|| internal_error(offset))?;
            let index = usize::try_from(local).map_err(|_| internal_error(offset))?;
            let entry = entries.get(index).ok_or_else(|| internal_error(offset))?;
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
    selection: &extract::SelectionIndex,
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
    budget.charge(1, offset)?;
    let spec = program.slot_for_field(field);
    if spec.is_some_and(|spec| spec.state != target || spec.slot >= program.image().header.slot_count) {
        return Err(internal_error(offset));
    }
    if repeated && spec.is_some() {
        return Err(vm_error(ErrorCode::ConstraintViolation, offset,
            "a compiled captured field cannot occur more than once"));
    }
    let slot = spec.filter(|spec| extract::selected(selection, spec.slot)).map(|spec| spec.slot);
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
    budget.charge(1, offset)?;
    match &program.table_state(table_id).ok_or_else(|| internal_error(offset))?.kind {
        StateKind::Object { reject_unknown, .. } => Ok(*reject_unknown),
        _ => Err(internal_error(offset)),
    }
}

/// At most eight SWAR comparisons or two hash passes and one comparison.
/// No work proportional to the full key table is performed per property.
fn charge_lookup(table: &KeyTable, key_len: u32, budget: &mut VmBudget, offset: u32) -> Result<()> {
    let passes = match &table.strategy {
        KeyStrategy::Swar => u64::from(table.key_count).checked_add(1),
        KeyStrategy::PerfectHash { .. } => Some(2),
        KeyStrategy::BucketedPerfectHash { .. } => Some(3),
    }.ok_or_else(|| resource_error(offset))?;
    let work = (u64::from(key_len) + 8).checked_mul(passes).ok_or_else(|| resource_error(offset))?;
    budget.charge(work, offset)
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
