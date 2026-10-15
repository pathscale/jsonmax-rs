//! Per-object key dispatch, duplicate tracking, and required-field validation.
//!
//! Seen bits address entries within a key table, not dense required-field IDs.
//! Active invocations own independent scratch, including recursive instances.
//! The interpreter reuses unknown-key tables across sibling objects by depth.
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
    begin_with_unknown_keys(program, state, UnknownKeyTable::new(), budget, offset)
}

pub(crate) fn begin_with_unknown_keys(
    program: &ValidatedProgram,
    state: Option<StateId>,
    unknown_keys: UnknownKeyTable,
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
        unknown_keys,
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
    key_inner(program, object, key, None, selection, limits, budget, offset)
}

/// Event execution supplies stable spans into its immutable raw payload or
/// append-only decoded arena. No key bytes are copied into VM scratch.
pub(crate) fn key_ref(
    program: &ValidatedProgram,
    object: &mut ObjectState,
    key: &str,
    reference: StrRef,
    raw: &[u8],
    decoded: &[u8],
    selection: &extract::SelectionIndex,
    limits: &VmLimits,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<PendingValue> {
    // execute::key has already resolved and validated this exact reference.
    key_inner(program, object, key, Some((reference, raw, decoded)),
        selection, limits, budget, offset)
}

fn key_inner(
    program: &ValidatedProgram,
    object: &mut ObjectState,
    key: &str,
    input: Option<(StrRef, &[u8], &[u8])>,
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
        retain_unknown(&mut object.unknown_keys, key, input, budget, offset)?;
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
/// seen-mask capacity at close and retains reusable unknown-key storage.
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

impl UnknownKeyTable {
    pub(crate) fn new() -> Self {
        Self { slots: Vec::new(), len: 0, generation: 1, arena: Vec::new(), input_backed: false }
    }

    /// Siblings reuse capacity without clearing slots except at generation wrap.
    pub(crate) fn reset(&mut self, budget: &mut VmBudget, offset: u32) -> Result<()> {
        match self.generation.checked_add(1) {
            Some(generation) => self.generation = generation,
            None => {
                budget.charge(u64::try_from(self.slots.len()).map_err(|_| resource_error(offset))?, offset)?;
                self.slots.fill(UnknownKeySlot::EMPTY);
                self.generation = 1;
            }
        }
        self.len = 0;
        self.arena.clear();
        self.input_backed = false;
        Ok(())
    }

    pub(crate) fn scratch_bytes(&self, offset: u32) -> Result<u32> {
        allocation_bytes::<UnknownKeySlot>(self.slots.capacity(), offset)?
            .checked_add(allocation_bytes::<u8>(self.arena.capacity(), offset)?)
            .ok_or_else(|| resource_error(offset))
    }

    fn contains(&self, hash: u64, bytes: &[u8], raw: &[u8], decoded: &[u8],
        budget: &mut VmBudget, offset: u32) -> Result<bool>
    {
        if self.slots.is_empty() { return Ok(false); }
        let mask = self.slots.len() - 1;
        let mut index = hash as usize & mask;
        loop {
            budget.charge(1, offset)?;
            let slot = self.slots[index];
            if slot.generation != self.generation { return Ok(false); }
            if slot.hash == hash {
                let owner = match slot.reference.source {
                    SpanSource::RawPayload => raw,
                    SpanSource::DecodedArena => decoded,
                };
                let previous = slot.reference.span.slice(owner)
                    .ok_or_else(|| internal_error(offset))?;
                if previous.len() == bytes.len() {
                    budget.charge(u64::try_from(bytes.len()).map_err(|_| resource_error(offset))?, offset)?;
                    if previous == bytes { return Ok(true); }
                }
            }
            index = (index + 1) & mask;
        }
    }

    /// Keep a power-of-two table at most half full. Rehashing is transactional:
    /// a failed allocation or work charge preserves all active keys.
    fn reserve_insert(&mut self, budget: &mut VmBudget, offset: u32) -> Result<()> {
        if self.len < self.slots.len() / 2 { return Ok(()); }
        let capacity = if self.slots.is_empty() { 8 } else {
            self.slots.len().checked_mul(2).ok_or_else(|| resource_error(offset))?
        };
        budget.charge(u64::try_from(self.slots.len()).map_err(|_| resource_error(offset))?, offset)?;
        let mut slots = allocate::<UnknownKeySlot>(capacity, budget, offset)?;
        slots.resize(capacity, UnknownKeySlot::EMPTY);
        let mask = capacity - 1;
        let result: Result<()> = (|| {
            for slot in &self.slots {
                if slot.generation == self.generation {
                    let mut index = slot.hash as usize & mask;
                    loop {
                        budget.charge(1, offset)?;
                        if slots[index].generation != self.generation { break; }
                        index = (index + 1) & mask;
                    }
                    slots[index] = *slot;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let bytes = allocation_bytes::<UnknownKeySlot>(slots.capacity(), offset)?;
            drop(slots);
            budget.release(bytes)?;
            return Err(error);
        }
        let old_bytes = allocation_bytes::<UnknownKeySlot>(self.slots.capacity(), offset)?;
        let old = core::mem::replace(&mut self.slots, slots);
        drop(old);
        budget.release(old_bytes)
    }

    fn vacant(&self, hash: u64, budget: &mut VmBudget, offset: u32) -> Result<usize> {
        let mask = self.slots.len() - 1;
        let mut index = hash as usize & mask;
        loop {
            budget.charge(1, offset)?;
            if self.slots[index].generation != self.generation { return Ok(index); }
            index = (index + 1) & mask;
        }
    }

    fn reserve_arena(&mut self, needed: usize, budget: &mut VmBudget, offset: u32) -> Result<()> {
        if needed <= self.arena.capacity() { return Ok(()); }
        let capacity = self.arena.capacity().checked_mul(2)
            .ok_or_else(|| resource_error(offset))?.max(needed);
        // Allocation work covers copying the old bytes as well as growth.
        let mut arena = allocate::<u8>(capacity, budget, offset)?;
        arena.extend_from_slice(&self.arena);
        let old_bytes = allocation_bytes::<u8>(self.arena.capacity(), offset)?;
        let old = core::mem::replace(&mut self.arena, arena);
        drop(old);
        budget.release(old_bytes)
    }
}

fn retain_unknown(
    table: &mut UnknownKeyTable,
    key: &str,
    input: Option<(StrRef, &[u8], &[u8])>,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<()> {
    // One object uses a single backing domain. Standalone calls share its
    // arena; event execution always supplies the live input's two buffers.
    if table.len != 0 && table.input_backed != input.is_some() {
        return Err(internal_error(offset));
    }
    let bytes = key.as_bytes();
    budget.charge(u64::try_from(bytes.len()).map_err(|_| resource_error(offset))?, offset)?;
    let hash = unknown_key_hash(bytes);
    let (raw, decoded) = match input {
        Some((_, raw, decoded)) => (raw, decoded),
        None => (&[][..], table.arena.as_slice()),
    };
    if table.contains(hash, bytes, raw, decoded, budget, offset)? {
        return Err(vm_error(ErrorCode::DuplicateObjectKey, offset,
            "unknown decoded object key occurs more than once"));
    }
    table.reserve_insert(budget, offset)?;
    let index = table.vacant(hash, budget, offset)?;
    let reference = match input {
        Some((reference, _, _)) => reference,
        None => {
            let start = u32::try_from(table.arena.len()).map_err(|_| resource_error(offset))?;
            let len = u32::try_from(bytes.len()).map_err(|_| resource_error(offset))?;
            let end = start.checked_add(len).ok_or_else(|| resource_error(offset))?;
            // Charge copying and scalar counting before mutating the arena.
            budget.charge(u64::from(len) * 2, offset)?;
            let scalar_len = u32::try_from(key.chars().count()).map_err(|_| resource_error(offset))?;
            table.reserve_arena(usize::try_from(end).map_err(|_| resource_error(offset))?, budget, offset)?;
            table.arena.extend_from_slice(bytes);
            StrRef { span: Span32::new(start, len), source: SpanSource::DecodedArena, scalar_len }
        }
    };
    table.slots[index] = UnknownKeySlot { hash, reference, generation: table.generation };
    table.len += 1;
    table.input_backed = input.is_some();
    Ok(())
}

fn unknown_key_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
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
