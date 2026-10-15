//! Bounded event execution for a witnessed schema program.
//!
//! Syntax ownership is supplied only by the audited `EventInput` boundary.
//! This module never creates a proof from an arbitrary collection of events.
use super::*;
use core::mem::size_of;
use jmax_abi::schema::{InsnWord, Opcode};
use jmax_schema_compile::decode_opcode;

/// Validate the complete input and retain its exact immutable owners.
pub fn run<I: EventInput>(
    program: &ValidatedProgram,
    mut input: I,
    selection: &Selection,
    limits: VmLimits,
) -> Result<ValidatedRun> {
    let result = (|| {
        let lengths = buffer_lengths(input.raw_bytes(), input.decoded_bytes(), &limits)?;
        let raw_owner = input.raw_bytes().as_ptr();
        let decoded_owner = input.decoded_bytes().as_ptr();
        let mut context = new_context(program, selection, limits)?;
        // This cursor exists only to locate raw key tokens. Key events carry a
        // cooked reference, which may have no raw offset at all.
        let mut token_end = 0u32;
        loop {
            let event = input.next_event().map_err(|error| annotate(
                program, active_field(&context), error,
            ))?;
            verify_owners(input.raw_bytes(), input.decoded_bytes(), lengths,
                raw_owner, decoded_owner, &limits)?;
            let Some(event) = event else { break };
            let raw = input.raw_bytes();
            let decoded = input.decoded_bytes();
            if let Event::Key { .. } = event {
                let (offset, end) = next_key_token(raw, token_end, &mut context.budget)?;
                step_at(&mut context, event, raw, decoded, offset)?;
                token_end = end;
            } else {
                step(&mut context, event, raw, decoded)?;
                token_end = event_end(event)?;
            }
        }
        finish_document(&context, lengths.0)?;
        // Keep the stack allocation charged until it is actually dropped.
        let frame_bytes = allocation_bytes::<VmFrame>(context.frames.capacity(), lengths.0)?;
        drop(core::mem::take(&mut context.frames));
        context.budget.release(frame_bytes)?;
        let buffers = input.into_buffers()?;
        verify_owners(buffers.payload.as_slice(), &buffers.decoded, lengths,
            raw_owner, decoded_owner, &limits)?;
        revalidate_slots(&context.slots, buffers.payload.as_slice(), &buffers.decoded,
            &limits, &mut context.budget)?;
        Ok(ValidatedRun {
            buffers,
            slots: context.slots,
            fingerprint: program.image().header.fingerprint,
        })
    })();
    result.map_err(|error| annotate(program, FieldId(0), error))
}

/// Publish only after successful strict EOF, schema checks and ownership checks.
pub fn validate<I: EventInput>(
    program: &ValidatedProgram,
    input: I,
    selection: &Selection,
    limits: VmLimits,
) -> Result<ValidatedSlots> {
    extract::publish(run(program, input, selection, limits)?)
        .map_err(|error| annotate(program, FieldId(0), error))
}

fn annotate(program: &ValidatedProgram, field: FieldId, error: Error) -> Error {
    let field_id = if error.field_id == 0 { field.0 } else { error.field_id };
    error.with_schema(program.image().header.schema_id, field_id)
}

fn active_field(context: &VmContext<'_>) -> FieldId {
    context.frames.last().map_or(FieldId(0), |frame| match &frame.kind {
        FrameKind::Object(object) => object.pending.map_or(frame.field_id, |p| p.field_id),
        FrameKind::Array { .. } => frame.field_id,
    })
}

fn resource_error(offset: u32) -> Error {
    vm_error(ErrorCode::BudgetExhausted, offset, "schema VM resource arithmetic or allocation failed")
}

fn internal(offset: u32) -> Error {
    vm_error(ErrorCode::InternalError, offset, "inconsistent validated program or event metadata")
}

fn allocation_bytes<T>(count: usize, offset: u32) -> Result<u32> {
    count.checked_mul(size_of::<T>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| resource_error(offset))
}

/// Reserve fallibly and charge capacity, including allocator excess capacity.
/// A failed run discards all scratch, so accounting need not be rolled back on
/// an allocation error. Successful runs never reuse an uncharged allocation.
fn reserve<T>(values: &mut Vec<T>, additional: usize, budget: &mut VmBudget, offset: u32)
    -> Result<()>
{
    let needed = values.len().checked_add(additional).ok_or_else(|| resource_error(offset))?;
    if needed <= values.capacity() { return Ok(()); }
    let old = allocation_bytes::<T>(values.capacity(), offset)?;
    let requested = allocation_bytes::<T>(needed, offset)?;
    // Reallocation may copy every live element. Charge bytes before asking
    // the allocator to grow, even when it can satisfy the request in place.
    budget.charge(u64::from(requested), offset)?;
    budget.retain(requested.checked_sub(old).ok_or_else(|| internal(offset))?, offset)?;
    values.try_reserve_exact(additional).map_err(|_| resource_error(offset))?;
    let actual = allocation_bytes::<T>(values.capacity(), offset)?;
    budget.retain(actual.checked_sub(requested).ok_or_else(|| internal(offset))?, offset)
}

fn new_context<'a>(program: &'a ValidatedProgram, selection: &'a Selection, limits: VmLimits)
    -> Result<VmContext<'a>>
{
    let slots_len = usize::try_from(program.image().header.slot_count)
        .map_err(|_| resource_error(0))?;
    if selection.field_ids.len() > slots_len {
        return Err(vm_error(ErrorCode::ConstraintViolation, 0,
            "selection contains more fields than the compiled slots"));
    }
    let mut budget = VmBudget::new(limits);
    let n = u64::try_from(selection.field_ids.len()).map_err(|_| resource_error(0))?;
    let slot_specs = u64::try_from(program.program().slots.len()).map_err(|_| resource_error(0))?;
    // n*n bounds the duplicate scan; n*slot_specs bounds membership scans.
    let comparisons = n.checked_mul(n).and_then(|v| n.checked_mul(slot_specs)
        .and_then(|other| v.checked_add(other))).ok_or_else(|| resource_error(0))?;
    budget.charge(comparisons, 0)?;
    check_selection(program, selection)?;
    if program.program().slots.len() != slots_len { return Err(internal(0)); }
    budget.charge(u64::from(program.image().header.slot_count), 0)?;
    let mut slots = Vec::new();
    reserve(&mut slots, slots_len, &mut budget, 0)?;
    slots.resize(slots_len, SlotValue::Empty);
    Ok(VmContext {
        program, selection, limits, frames: Vec::new(), slots, budget,
        root_started: false, root_done: false,
    })
}

fn buffer_lengths(raw: &[u8], decoded: &[u8], limits: &VmLimits) -> Result<(u32, u32)> {
    let raw_len = u32::try_from(raw.len()).map_err(|_| vm_error(
        ErrorCode::PayloadTooLarge, 0, "payload length exceeds u32"))?;
    let decoded_len = u32::try_from(decoded.len()).map_err(|_| vm_error(
        ErrorCode::DecodedArenaOverflow, 0, "decoded arena length exceeds u32"))?;
    if raw_len > limits.syntax.max_payload_bytes {
        return Err(vm_error(ErrorCode::PayloadTooLarge, 0, "payload exceeds VM input limit"));
    }
    if decoded_len > limits.syntax.max_decoded_bytes {
        return Err(vm_error(ErrorCode::DecodedArenaOverflow, 0, "decoded arena exceeds VM input limit"));
    }
    Ok((raw_len, decoded_len))
}

fn verify_owners(
    raw: &[u8], decoded: &[u8], lengths: (u32, u32),
    raw_owner: *const u8, decoded_owner: *const u8, limits: &VmLimits,
) -> Result<()> {
    if buffer_lengths(raw, decoded, limits)? != lengths
        || (!raw.is_empty() && raw.as_ptr() != raw_owner)
        || (!decoded.is_empty() && decoded.as_ptr() != decoded_owner)
    {
        return Err(vm_error(ErrorCode::InternalError, 0,
            "event input changed the buffers backing validation"));
    }
    // Pointer/length equality is a defensive check, not an ownership proof.
    // Byte immutability and exact returned owners are EventInput obligations.
    Ok(())
}

fn finish_document(context: &VmContext<'_>, offset: u32) -> Result<()> {
    if !context.root_started || !context.root_done || !context.frames.is_empty() {
        return Err(annotate(context.program, active_field(context), vm_error(
            ErrorCode::UnexpectedEof, offset, "JSON document has no completed root value")));
    }
    Ok(())
}

fn event_end(event: Event) -> Result<u32> {
    match event {
        Event::ObjectStart { offset } | Event::ArrayStart { offset }
        | Event::ObjectEnd { offset } | Event::ArrayEnd { offset } =>
            offset.checked_add(1).ok_or_else(|| internal(offset)),
        Event::Scalar { span, .. } => span.end().ok_or_else(|| internal(span.offset)),
        Event::Key { .. } => Err(internal(0)),
    }
}

/// Locate the next key's complete raw token without decoding or allocating.
/// Punctuation and whitespace are already grammar-checked by EventInput.
fn next_key_token(raw: &[u8], after: u32, budget: &mut VmBudget) -> Result<(u32, u32)> {
    let mut index = usize::try_from(after).map_err(|_| internal(after))?;
    loop {
        let byte = *raw.get(index).ok_or_else(|| vm_error(
            ErrorCode::UnexpectedEof, after, "missing raw key token"))?;
        budget.charge(1, after)?;
        if byte == b'"' { break; }
        if !matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',') {
            return Err(vm_error(ErrorCode::InvalidSyntax, after, "key event has no raw key token"));
        }
        index = index.checked_add(1).ok_or_else(|| internal(after))?;
    }
    let start = u32::try_from(index).map_err(|_| internal(after))?;
    index = index.checked_add(1).ok_or_else(|| internal(start))?;
    let mut escaped = false;
    loop {
        let byte = *raw.get(index).ok_or_else(|| vm_error(
            ErrorCode::UnclosedString, start, "unterminated raw key token"))?;
        budget.charge(1, start)?;
        index = index.checked_add(1).ok_or_else(|| internal(start))?;
        if escaped { escaped = false; }
        else if byte == b'\\' { escaped = true; }
        else if byte == b'"' {
            return Ok((start, u32::try_from(index).map_err(|_| internal(start))?));
        }
    }
}

/// The standalone step API has no previous-token cursor. Resolve a cooked key
/// by scanning its containing object's raw prefix, bounded by the same budget.
/// `run` uses its linear cursor instead, so ordinary execution never rescans.
fn key_offset(context: &mut VmContext<'_>, reference: StrRef, raw: &[u8]) -> Result<u32> {
    if reference.source == SpanSource::RawPayload {
        return reference.span.offset.checked_sub(1).ok_or_else(|| internal(0));
    }
    let frame = context.frames.last().ok_or_else(|| vm_error(
        ErrorCode::InvalidSyntax, 0, "key outside an object"))?;
    let ordinal = match &frame.kind {
        FrameKind::Object(object) => object.property_count,
        _ => return Err(vm_error(ErrorCode::InvalidSyntax, frame.start, "key inside an array")),
    };
    let start = frame.start;
    let mut index = start.checked_add(1).ok_or_else(|| internal(start))?;
    let mut depth = 0u32;
    let mut keys = 0u32;
    let mut expect_key = true;
    loop {
        let byte = *raw.get(usize::try_from(index).map_err(|_| internal(start))?)
            .ok_or_else(|| vm_error(ErrorCode::UnexpectedEof, start, "missing raw key"))?;
        context.budget.charge(1, start)?;
        if byte == b'"' {
            let (_, end) = next_key_token(raw, index, &mut context.budget)?;
            if depth == 0 && expect_key {
                if keys == ordinal { return Ok(index); }
                keys = keys.checked_add(1).ok_or_else(|| resource_error(start))?;
                expect_key = false;
            }
            index = end;
            continue;
        }
        match byte {
            b'{' | b'[' => depth = depth.checked_add(1).ok_or_else(|| resource_error(start))?,
            b'}' | b']' => depth = depth.checked_sub(1).ok_or_else(|| vm_error(
                ErrorCode::InvalidSyntax, start, "key event follows object end"))?,
            b',' if depth == 0 => expect_key = true,
            _ => {}
        }
        index = index.checked_add(1).ok_or_else(|| internal(start))?;
    }
}

pub(crate) fn step(context: &mut VmContext<'_>, event: Event, raw: &[u8], decoded: &[u8])
    -> Result<()>
{
    let offset = match event {
        Event::ObjectStart { offset } | Event::ObjectEnd { offset }
        | Event::ArrayStart { offset } | Event::ArrayEnd { offset } => offset,
        Event::Scalar { span, .. } => span.offset,
        Event::Key { str_ref } => key_offset(context, str_ref, raw)
            .map_err(|error| annotate(context.program, active_field(context), error))?,
    };
    step_at(context, event, raw, decoded, offset)
}

fn step_at(context: &mut VmContext<'_>, event: Event, raw: &[u8], decoded: &[u8], offset: u32)
    -> Result<()>
{
    let field = active_field(context);
    let result = (|| {
        context.budget.charge(1, offset)?;
        buffer_lengths(raw, decoded, &context.limits)?;
        if context.root_done {
            return Err(vm_error(ErrorCode::TrailingContent, offset, "event after the root value"));
        }
        match event {
            Event::ObjectStart { .. } => open(context, ContainerKind::Object, raw, offset),
            Event::ArrayStart { .. } => open(context, ContainerKind::Array, raw, offset),
            Event::ObjectEnd { .. } => close(context, ContainerKind::Object, raw, offset),
            Event::ArrayEnd { .. } => close(context, ContainerKind::Array, raw, offset),
            Event::Key { str_ref } => key(context, str_ref, raw, decoded, offset),
            Event::Scalar { kind, span, decoded: reference } => {
                let pending = take_value(context, offset)?;
                let result = scalar(context, pending, ScalarInput {
                    kind, span, decoded: reference, raw, arena: decoded,
                });
                result.map_err(|error| annotate(context.program, pending.field_id, error))
            }
        }
    })();
    result.map_err(|error| annotate(context.program, field, error))
}

fn state(program: &ValidatedProgram, id: StateId, offset: u32) -> Result<&ProgramState> {
    let state = program.state(id).ok_or_else(|| internal(offset))?;
    if state.id != id { return Err(internal(offset)); }
    Ok(state)
}

fn instructions<'a>(program: &'a ValidatedProgram, state: &ProgramState, offset: u32)
    -> Result<&'a [InsnWord]>
{
    let end = state.insns.end().ok_or_else(|| internal(offset))?;
    let start = usize::try_from(state.insns.offset).map_err(|_| internal(offset))?;
    let end = usize::try_from(end).map_err(|_| internal(offset))?;
    let range = program.image().insns.get(start..end).ok_or_else(|| internal(offset))?;
    if range.is_empty() { return Err(internal(offset)); }
    Ok(range)
}

fn opcode(instruction: &InsnWord, budget: &mut VmBudget, offset: u32) -> Result<Opcode> {
    budget.charge(1, offset)?;
    if instruction.flags != 0 { return Err(internal(offset)); }
    decode_opcode(instruction.opcode).ok_or_else(|| internal(offset))
}

fn take_value(context: &mut VmContext<'_>, offset: u32) -> Result<PendingValue> {
    if let Some(parent) = context.frames.last_mut() {
        match &mut parent.kind {
            FrameKind::Object(object) => objects::take_value(object, offset),
            FrameKind::Array { count, items } => {
                let next = count.checked_add(1).ok_or_else(|| resource_error(offset))?;
                if next > context.limits.syntax.max_array_items { return Err(resource_error(offset)); }
                *count = next;
                Ok(PendingValue { state: *items, field_id: parent.field_id, slot: None })
            }
        }
    } else {
        if context.root_started {
            return Err(vm_error(ErrorCode::TrailingContent, offset, "more than one root value"));
        }
        context.root_started = true;
        Ok(PendingValue {
            state: Some(StateId(context.program.image().header.root_state)),
            field_id: FieldId(0), slot: None,
        })
    }
}

fn delimiter(raw: &[u8], offset: u32, expected: u8) -> Result<()> {
    let index = usize::try_from(offset).map_err(|_| internal(offset))?;
    if raw.get(index).copied() != Some(expected) {
        return Err(vm_error(ErrorCode::InvalidSyntax, offset, "container event has the wrong delimiter"));
    }
    Ok(())
}

fn open(context: &mut VmContext<'_>, kind: ContainerKind, raw: &[u8], offset: u32) -> Result<()> {
    delimiter(raw, offset, match kind { ContainerKind::Object => b'{', ContainerKind::Array => b'[' })?;
    let depth = context.frames.len().checked_add(1).ok_or_else(|| resource_error(offset))?;
    if depth > usize::from(context.limits.syntax.max_depth) {
        return Err(vm_error(ErrorCode::DepthLimitExceeded, offset, "container depth exceeds VM limit"));
    }
    let pending = take_value(context, offset)?;
    let result = (|| {
        let mut items = None;
        if let Some(id) = pending.state {
            let current = state(context.program, id, offset)?;
            let first = instructions(context.program, current, offset)?.first()
                .ok_or_else(|| internal(offset))?;
            let op = opcode(first, &mut context.budget, offset)?;
            if first.arg0 != id.0 || first.field_id != 0 { return Err(internal(offset)); }
            match (&current.kind, kind, op) {
                (StateKind::Any, _, Opcode::CheckScalarKind) if first.arg1 == 255 => {}
                (StateKind::Object { key_table, .. }, ContainerKind::Object, Opcode::EnterObject)
                    if first.arg1 == u64::from(*key_table) => {}
                (StateKind::Array { items: target }, ContainerKind::Array, Opcode::EnterArray)
                    if first.arg1 == u64::from(target.0) => { items = Some(*target); }
                (StateKind::Scalar(_), _, _) | (StateKind::Array { .. }, ContainerKind::Object, _)
                | (StateKind::Object { .. }, ContainerKind::Array, _) => {
                    return Err(vm_error(ErrorCode::TypeMismatch, offset, "container kind does not match schema"));
                }
                _ => return Err(internal(offset)),
            }
        }
        reserve(&mut context.frames, 1, &mut context.budget, offset)?;
        let frame_kind = match kind {
            ContainerKind::Object => FrameKind::Object(objects::begin(
                context.program, pending.state, &mut context.budget, offset,
            )?),
            ContainerKind::Array => FrameKind::Array { count: 0, items },
        };
        context.frames.push(VmFrame {
            state: pending.state, field_id: pending.field_id, start: offset,
            slot: pending.slot, kind: frame_kind,
        });
        Ok(())
    })();
    result.map_err(|error| annotate(context.program, pending.field_id, error))
}

fn key(context: &mut VmContext<'_>, reference: StrRef, raw: &[u8], decoded: &[u8], offset: u32)
    -> Result<()>
{
    delimiter(raw, offset, b'"')?;
    let text = checked_string(reference, raw, decoded, &context.limits,
        &mut context.budget, offset)?;
    let frame = context.frames.last_mut().ok_or_else(|| vm_error(
        ErrorCode::InvalidSyntax, offset, "key outside an object"))?;
    let object = match &mut frame.kind {
        FrameKind::Object(object) => object,
        _ => return Err(vm_error(ErrorCode::InvalidSyntax, offset, "key inside an array")),
    };
    if let Some(id) = frame.state {
        let current = state(context.program, id, offset)?;
        if let StateKind::Object { key_table, reject_unknown, .. } = &current.kind {
            let range = instructions(context.program, current, offset)?;
            let lookup = range.get(1).ok_or_else(|| internal(offset))?;
            if !matches!(opcode(lookup, &mut context.budget, offset)?,
                Opcode::MatchKeySwar | Opcode::MatchKeyPhf)
                || lookup.arg0 != *key_table || lookup.arg1 != 0 || lookup.field_id != 0
            { return Err(internal(offset)); }
            if *reject_unknown {
                let reject = range.get(2).ok_or_else(|| internal(offset))?;
                if opcode(reject, &mut context.budget, offset)? != Opcode::RejectUnknown
                    || reject.arg0 != id.0 || reject.arg1 != 0 || reject.field_id != 0
                { return Err(internal(offset)); }
            }
        } else if current.kind != StateKind::Any { return Err(internal(offset)); }
    }
    let pending = objects::key(context.program, object, text, context.selection,
        &context.limits, &mut context.budget, offset)?;
    // EmitSlot belongs to the containing object. It schedules a completion
    // action; it must never publish a container before its matching close.
    if let Some(slot) = pending.slot {
        let result = (|| {
            let id = frame.state.ok_or_else(|| internal(offset))?;
            let current = state(context.program, id, offset)?;
            let mut found = false;
            for instruction in instructions(context.program, current, offset)? {
                if opcode(instruction, &mut context.budget, offset)? == Opcode::EmitSlot
                    && u32::from(instruction.field_id) == pending.field_id.0
                {
                    if found || instruction.arg0 != slot
                        || pending.state.map(|s| u64::from(s.0)) != Some(instruction.arg1)
                    { return Err(internal(offset)); }
                    found = true;
                }
            }
            if !found { return Err(internal(offset)); }
            Ok(())
        })();
        result.map_err(|error| annotate(context.program, pending.field_id, error))?;
    }
    Ok(())
}

fn checked_string<'a>(reference: StrRef, raw: &'a [u8], decoded: &'a [u8], limits: &VmLimits,
    budget: &mut VmBudget, offset: u32) -> Result<&'a str>
{
    if reference.span.len > limits.syntax.max_string_bytes { return Err(resource_error(offset)); }
    // UTF-8 validation and scalar counting each take at most byte length work.
    budget.charge(u64::from(reference.span.len).checked_mul(2)
        .ok_or_else(|| resource_error(offset))?, offset)?;
    resolve_str(raw, decoded, reference).map_err(|error| Error { byte_offset: offset, ..error })
}

fn number_work(bytes: &[u8], limits: &VmLimits, budget: &mut VmBudget, offset: u32) -> Result<()> {
    budget.charge(u64::try_from(bytes.len()).map_err(|_| resource_error(offset))?, offset)?;
    let mut digits = 0u32;
    for byte in bytes {
        if byte.is_ascii_digit() {
            digits = digits.checked_add(1).ok_or_else(|| resource_error(offset))?;
            if digits > u32::from(limits.syntax.max_number_digits) { return Err(resource_error(offset)); }
        }
    }
    Ok(())
}

fn scalar(context: &mut VmContext<'_>, pending: PendingValue, value: ScalarInput<'_>) -> Result<()> {
    let offset = value.span.offset;
    let token = value.span.slice(value.raw).ok_or_else(|| internal(offset))?;
    if token.is_empty() { return Err(vm_error(ErrorCode::InvalidSyntax, offset, "empty scalar token")); }
    let mut any = false;
    let mut quoted_number = false;
    if let Some(id) = pending.state {
        let current = state(context.program, id, offset)?;
        any = current.kind == StateKind::Any;
        quoted_number = matches!(current.kind, StateKind::Scalar(ValueKind::Integer | ValueKind::Decimal));
        if matches!(current.kind, StateKind::Object { .. } | StateKind::Array { .. }) {
            return Err(vm_error(ErrorCode::TypeMismatch, offset, "scalar does not match container schema"));
        }
        for (index, instruction) in instructions(context.program, current, offset)?.iter().enumerate() {
            let op = opcode(instruction, &mut context.budget, offset)?;
            if instruction.arg0 != id.0 || instruction.field_id != 0
                || (index == 0 && op != Opcode::CheckScalarKind)
                || (index != 0 && !matches!(op, Opcode::CheckIntBounds | Opcode::CheckDecimalBounds
                    | Opcode::CheckStringLen | Opcode::ExecPattern | Opcode::ExecFormat))
            { return Err(internal(offset)); }
        }
    }
    match (value.kind, value.decoded) {
        (ScalarKind::String, Some(reference)) => {
            if token.len() < 2 || token.first() != Some(&b'"') || token.last() != Some(&b'"') {
                return Err(vm_error(ErrorCode::InvalidSyntax, offset, "string token lacks quotes"));
            }
            context.budget.charge(u64::from(value.span.len), offset)?;
            let text = checked_string(reference, value.raw, value.arena, &context.limits,
                &mut context.budget, offset)?;
            if quoted_number { number_work(text.as_bytes(), &context.limits, &mut context.budget, offset)?; }
        }
        (ScalarKind::String, None) | (_, Some(_)) => return Err(internal(offset)),
        (ScalarKind::Number, None) => number_work(token, &context.limits, &mut context.budget, offset)?,
        (ScalarKind::Null, None) if token == b"null" => {}
        (ScalarKind::True, None) if token == b"true" => {}
        (ScalarKind::False, None) if token == b"false" => {}
        _ => return Err(vm_error(ErrorCode::InvalidSyntax, offset, "scalar metadata disagrees with token")),
    }
    let converted = constraints::check_scalar(context.program, pending.state, value, &mut context.budget)?;
    if let Some(slot) = pending.slot {
        let captured = if any { SlotValue::RawSpan(value.span) } else { converted };
        extract::store(&mut context.slots, slot, captured, offset)?;
    }
    if context.frames.is_empty() { context.root_done = true; }
    Ok(())
}

fn close(context: &mut VmContext<'_>, kind: ContainerKind, raw: &[u8], offset: u32) -> Result<()> {
    delimiter(raw, offset, match kind { ContainerKind::Object => b'}', ContainerKind::Array => b']' })?;
    let frame = context.frames.last().ok_or_else(|| vm_error(
        ErrorCode::InvalidSyntax, offset, "closing delimiter has no opening frame"))?;
    let matches = matches!((&frame.kind, kind), (FrameKind::Object(_), ContainerKind::Object)
        | (FrameKind::Array { .. }, ContainerKind::Array));
    if !matches || offset <= frame.start {
        return Err(vm_error(ErrorCode::InvalidSyntax, offset, "mismatched container close"));
    }
    let end = offset.checked_add(1).ok_or_else(|| internal(offset))?;
    let span = Span32::new(frame.start, end.checked_sub(frame.start).ok_or_else(|| internal(offset))?);
    if span.slice(raw).is_none() { return Err(internal(offset)); }
    if let Some(id) = frame.state {
        let current = state(context.program, id, offset)?;
        if current.kind != StateKind::Any {
            let last = instructions(context.program, current, offset)?.last()
                .ok_or_else(|| internal(offset))?;
            let expected = match kind { ContainerKind::Object => Opcode::LeaveObject,
                ContainerKind::Array => Opcode::LeaveArray };
            if opcode(last, &mut context.budget, offset)? != expected || last.arg0 != id.0
                || last.arg1 != 0 || last.field_id != 0 { return Err(internal(offset)); }
        }
        let count = match &frame.kind {
            FrameKind::Object(object) => object.property_count,
            FrameKind::Array { count, .. } => *count,
        };
        constraints::check_count(&current.constraints, count, offset)?;
    }
    if let FrameKind::Object(object) = &frame.kind {
        objects::finish(context.program, frame.state, object, &mut context.budget, offset)?;
    }
    if let Some(slot) = frame.slot {
        extract::store(&mut context.slots, slot, SlotValue::RawSpan(span), offset)?;
    }
    let frame = context.frames.pop().ok_or_else(|| internal(offset))?;
    if let FrameKind::Object(object) = frame.kind {
        release_object(object, &mut context.budget, offset)?;
    }
    if context.frames.is_empty() { context.root_done = true; }
    Ok(())
}

fn release_object(object: ObjectState, budget: &mut VmBudget, offset: u32) -> Result<()> {
    let mut bytes = allocation_bytes::<u64>(object.seen_words.capacity(), offset)?
        .checked_add(allocation_bytes::<Vec<u8>>(object.unknown_keys.capacity(), offset)?)
        .ok_or_else(|| resource_error(offset))?;
    for name in &object.unknown_keys {
        budget.charge(1, offset)?;
        bytes = bytes.checked_add(allocation_bytes::<u8>(name.capacity(), offset)?)
            .ok_or_else(|| resource_error(offset))?;
    }
    drop(object);
    budget.release(bytes)
}

fn revalidate_slots(slots: &[SlotValue], raw: &[u8], decoded: &[u8], limits: &VmLimits,
    budget: &mut VmBudget) -> Result<()>
{
    buffer_lengths(raw, decoded, limits)?;
    for value in slots {
        budget.charge(1, 0)?;
        match *value {
            SlotValue::Str(reference) => {
                // An arena index is never a raw diagnostic offset.
                let offset = if reference.source == SpanSource::RawPayload { reference.span.offset } else { 0 };
                checked_string(reference, raw, decoded, limits, budget, offset)?;
            }
            SlotValue::RawSpan(span) => {
                if span.is_empty() || span.slice(raw).is_none() { return Err(internal(span.offset)); }
            }
            SlotValue::Empty | SlotValue::Bool(_) | SlotValue::U64(_) | SlotValue::I64(_)
            | SlotValue::F64Bits(_) | SlotValue::Decimal(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec};
    use jmax_schema_compile::CompilerOptions;
    use jmax_schema_ir::{ArrayShape, ObjectShape, Property, ResolveLimits, ScalarConstraints,
        Schema, SchemaId, TypeId, TypeNode};

    fn selection() -> Selection {
        Selection { field_ids: Vec::new(), strict_validate_unselected: true }
    }

    fn compile(nodes: Vec<TypeNode>) -> ValidatedProgram {
        let schema = Schema {
            name: String::from("execute-tests"), schema_id: SchemaId(73), root: TypeId(0),
            nodes, definitions: Vec::new(),
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits::default()).unwrap();
        jmax_schema_compile::lower::compile(&resolved, &CompilerOptions::default()).unwrap()
    }

    fn any_program() -> ValidatedProgram { compile(vec![TypeNode::Any]) }

    fn array_program(min: Option<u32>, max: Option<u32>) -> ValidatedProgram {
        compile(vec![TypeNode::Array(ArrayShape {
            items: TypeId(1), min_items: min, max_items: max,
        }), TypeNode::Any])
    }

    fn property(name: &str, id: u32, target: u32, required: bool, slot: Option<u32>) -> Property {
        Property { name: String::from(name), field_id: FieldId(id), ty: TypeId(target), required, slot }
    }

    fn object_program(required: bool, reject_unknown: bool) -> ValidatedProgram {
        compile(vec![
            TypeNode::Object(ObjectShape {
                properties: vec![property("a", 11, 1, required, Some(0)),
                    property("b", 22, 2, false, Some(1))],
                reject_unknown, min_properties: None, max_properties: None,
            }),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::Integer)),
            TypeNode::Array(ArrayShape { items: TypeId(3), min_items: Some(1), max_items: Some(2) }),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::Null)),
        ])
    }

    fn reference(offset: u32, len: u32, scalar_len: u32) -> StrRef {
        StrRef { source: SpanSource::RawPayload, span: Span32::new(offset, len), scalar_len }
    }

    fn raw_key(offset: u32, len: u32) -> Event {
        Event::Key { str_ref: reference(offset, len, len) }
    }

    fn null(offset: u32) -> Event {
        Event::Scalar { kind: ScalarKind::Null, span: Span32::new(offset, 4), decoded: None }
    }

    fn number(offset: u32, len: u32) -> Event {
        Event::Scalar { kind: ScalarKind::Number, span: Span32::new(offset, len), decoded: None }
    }

    fn drive(program: &ValidatedProgram, selection: &Selection, limits: VmLimits,
        raw: &[u8], decoded: &[u8], events: &[Event]) -> Result<Vec<SlotValue>>
    {
        let mut context = new_context(program, selection, limits)?;
        for &event in events { step(&mut context, event, raw, decoded)?; }
        finish_document(&context, u32::try_from(raw.len()).unwrap())?;
        Ok(context.slots)
    }

    /// Closed fixture: the only possible document is the literal `null`.
    /// Error switches can terminate execution, but cannot introduce events.
    struct NullInput {
        payload: Payload,
        decoded: Box<[u8]>,
        emitted: bool,
        eof_checked: bool,
        fail_eof: bool,
        fail_transfer: bool,
    }

    impl NullInput {
        fn new() -> Self {
            Self { payload: Payload::from_slice(b"null"), decoded: Vec::new().into_boxed_slice(),
                emitted: false, eof_checked: false, fail_eof: false, fail_transfer: false }
        }
    }

    // SAFETY: All constructors fix the exact UTF-8 document to `null`, which has
    // one root, valid complete grammar, no trailing content and no decoded data.
    // The only event describes exactly that immutable token. EOF is considered
    // only after it, and the exact same owners move into InputBuffers. Optional
    // failures return errors, never an incomplete successful stream. VM limits
    // are enforced by run/step before a completed-run capability can exist.
    unsafe impl EventInput for NullInput {
        fn raw_bytes(&self) -> &[u8] { self.payload.as_slice() }
        fn decoded_bytes(&self) -> &[u8] { &self.decoded }
        fn next_event(&mut self) -> Result<Option<Event>> {
            if !self.emitted {
                self.emitted = true;
                return Ok(Some(null(0)));
            }
            self.eof_checked = true;
            if self.fail_eof {
                Err(Error::new(ErrorCode::TrailingContent, ErrorSource::Syntax, 4,
                    "injected strict EOF failure"))
            } else { Ok(None) }
        }
        fn into_buffers(self) -> Result<InputBuffers> {
            if !self.eof_checked {
                return Err(vm_error(ErrorCode::InternalError, 0, "EOF was not requested"));
            }
            if self.fail_transfer {
                return Err(Error::new(ErrorCode::Cancelled, ErrorSource::Cursor, 4,
                    "injected ownership transfer failure"));
            }
            Ok(InputBuffers { payload: self.payload, decoded: self.decoded })
        }
    }

    struct ObjectInput {
        buffers: InputBuffers,
        index: usize,
        eof_checked: bool,
        fail_eof: bool,
    }

    impl ObjectInput {
        fn new() -> Self {
            Self {
                buffers: InputBuffers {
                    payload: Payload::from_slice(b"{\"\\u0061\":7,\"b\":[null]}"),
                    decoded: b"a".to_vec().into_boxed_slice(),
                },
                index: 0, eof_checked: false, fail_eof: false,
            }
        }
    }

    // SAFETY: This private fixture has exactly one hard-coded, fully inspected
    // JSON document. The escape decodes to 'a'; all other strings are raw.
    // Events below cover its entire grammar exactly once in order, with exact
    // token offsets and Unicode counts. There is no externally supplied event
    // vector. Buffers never change and move without copying into the output.
    // The VM enforces runtime/schema policies; EOF failure only denies success.
    unsafe impl EventInput for ObjectInput {
        fn raw_bytes(&self) -> &[u8] { self.buffers.payload.as_slice() }
        fn decoded_bytes(&self) -> &[u8] { &self.buffers.decoded }
        fn next_event(&mut self) -> Result<Option<Event>> {
            let events = [
                Event::ObjectStart { offset: 0 },
                Event::Key { str_ref: StrRef { source: SpanSource::DecodedArena,
                    span: Span32::new(0, 1), scalar_len: 1 } },
                number(10, 1), raw_key(13, 1), Event::ArrayStart { offset: 16 },
                null(17), Event::ArrayEnd { offset: 21 }, Event::ObjectEnd { offset: 22 },
            ];
            if let Some(event) = events.get(self.index).copied() {
                self.index = self.index.checked_add(1).ok_or_else(|| internal(0))?;
                return Ok(Some(event));
            }
            self.eof_checked = true;
            if self.fail_eof {
                Err(Error::new(ErrorCode::TrailingContent, ErrorSource::Syntax, 23,
                    "injected failure after the root object"))
            } else { Ok(None) }
        }
        fn into_buffers(self) -> Result<InputBuffers> {
            if !self.eof_checked {
                return Err(internal(0));
            }
            Ok(self.buffers)
        }
    }

    #[test]
    fn full_run_resolves_escaped_keys_captures_children_and_requests_container_eof() {
        let program = object_program(true, true);
        let input = ObjectInput::new();
        let arena_owner = input.decoded_bytes().as_ptr();
        let result = run(&program, input, &selection(), VmLimits::default()).unwrap();
        assert_eq!(result.slots(), &[SlotValue::U64(7), SlotValue::RawSpan(Span32::new(16, 6))]);
        assert_eq!(result.decoded_bytes(), b"a");
        assert_eq!(result.decoded_bytes().as_ptr(), arena_owner);
        let mut input = ObjectInput::new();
        input.fail_eof = true;
        let error = run(&program, input, &selection(), VmLimits::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::TrailingContent);
        assert_eq!(error.byte_offset, 23);
        assert_eq!(error.source, ErrorSource::Syntax);
    }

    #[test]
    fn run_requires_strict_eof_and_preserves_the_exact_payload_owner() {
        let program = any_program();
        let input = NullInput::new();
        let owner = input.raw_bytes().as_ptr();
        let run = run(&program, input, &selection(), VmLimits::default()).unwrap();
        assert_eq!(run.payload().as_slice(), b"null");
        assert_eq!(run.payload().as_slice().as_ptr(), owner);
        assert!(run.decoded_bytes().is_empty());
        assert!(run.slots().is_empty());
        assert_eq!(run.schema_fingerprint(), program.image().header.fingerprint);
    }

    #[test]
    fn validate_publishes_only_the_completed_run() {
        let program = any_program();
        let result = validate(&program, NullInput::new(), &selection(), VmLimits::default()).unwrap();
        assert_eq!(result.proof.payload_len(), 4);
        assert_eq!(result.proof.decoded_len(), 0);
        assert_eq!(result.proof.schema_fingerprint(), program.image().header.fingerprint);
        assert_eq!(result.payload.as_slice(), b"null");
    }

    #[test]
    fn late_input_and_ownership_failures_never_return_a_run() {
        let program = any_program();
        let mut input = NullInput::new();
        input.fail_eof = true;
        let error = run(&program, input, &selection(), VmLimits::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::TrailingContent);
        assert_eq!(error.source, ErrorSource::Syntax);
        assert_eq!(error.byte_offset, 4);
        assert_eq!(error.schema_id, 73);
        let mut input = NullInput::new();
        input.fail_transfer = true;
        let error = validate(&program, input, &selection(), VmLimits::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(error.source, ErrorSource::Cursor);
    }

    #[test]
    fn selection_is_strict_unique_known_and_bounded_before_scanning() {
        let program = object_program(false, false);
        let cases = [
            (Selection::default(), ErrorCode::UnsupportedSchemaFeature),
            (Selection { field_ids: vec![FieldId(11), FieldId(11)], strict_validate_unselected: true },
                ErrorCode::ConstraintViolation),
            (Selection { field_ids: vec![FieldId(99)], strict_validate_unselected: true },
                ErrorCode::UnknownObjectKey),
            (Selection { field_ids: vec![FieldId(11); 3], strict_validate_unselected: true },
                ErrorCode::ConstraintViolation),
        ];
        for (selection, expected) in cases {
            assert_eq!(new_context(&program, &selection, VmLimits::default()).err().unwrap().code, expected);
        }
        let chosen = Selection { field_ids: vec![FieldId(11)], strict_validate_unselected: true };
        let mut limits = VmLimits::default();
        limits.max_steps = 2; // 1 duplicate-comparison upper bound + 2 slot comparisons.
        assert_eq!(new_context(&program, &chosen, limits).err().unwrap().code, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn scalar_root_depth_is_zero_and_container_depth_is_inclusive() {
        let program = any_program();
        let mut limits = VmLimits::default();
        limits.syntax.max_depth = 0;
        drive(&program, &selection(), limits, b"null", b"", &[null(0)]).unwrap();
        let events = [Event::ArrayStart { offset: 0 }, Event::ArrayEnd { offset: 1 }];
        assert_eq!(drive(&program, &selection(), limits, b"[]", b"", &events).unwrap_err().code,
            ErrorCode::DepthLimitExceeded);
        limits.syntax.max_depth = 1;
        drive(&program, &selection(), limits, b"[]", b"", &events).unwrap();
        let events = [Event::ArrayStart { offset: 0 }, Event::ArrayStart { offset: 1 },
            Event::ArrayEnd { offset: 2 }, Event::ArrayEnd { offset: 3 }];
        assert_eq!(drive(&program, &selection(), limits, b"[[]]", b"", &events).unwrap_err().code,
            ErrorCode::DepthLimitExceeded);
        limits.syntax.max_depth = 2;
        drive(&program, &selection(), limits, b"[[]]", b"", &events).unwrap();
    }

    #[test]
    fn child_containers_count_once_at_value_start() {
        let program = array_program(Some(2), Some(2));
        let events = [Event::ArrayStart { offset: 0 }, Event::ArrayStart { offset: 1 }, null(2),
            Event::ArrayEnd { offset: 6 }, Event::ArrayStart { offset: 8 },
            Event::ArrayEnd { offset: 9 }, Event::ArrayEnd { offset: 10 }];
        let mut limits = VmLimits::default();
        limits.syntax.max_array_items = 2;
        drive(&program, &selection(), limits, b"[[null],[]]", b"", &events).unwrap();
        limits.syntax.max_array_items = 1;
        let error = drive(&program, &selection(), limits, b"[[null],[]]", b"", &events).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 8);
    }

    #[test]
    fn count_constraints_are_checked_on_the_matching_close() {
        let program = array_program(Some(1), Some(1));
        let error = drive(&program, &selection(), VmLimits::default(), b"[]", b"",
            &[Event::ArrayStart { offset: 0 }, Event::ArrayEnd { offset: 1 }]).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.byte_offset, 1);
        let events = [Event::ArrayStart { offset: 0 }, null(1), null(6), Event::ArrayEnd { offset: 10 }];
        let error = drive(&program, &selection(), VmLimits::default(), b"[null,null]", b"", &events).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.byte_offset, 10);
    }

    #[test]
    fn selected_container_is_written_only_after_successful_close() {
        let program = object_program(false, false);
        let selected = selection();
        let raw = b"{\"b\":[null]}";
        let mut context = new_context(&program, &selected, VmLimits::default()).unwrap();
        for event in [Event::ObjectStart { offset: 0 }, raw_key(2, 1), Event::ArrayStart { offset: 5 }, null(6)] {
            step(&mut context, event, raw, b"").unwrap();
        }
        assert_eq!(context.slots, vec![SlotValue::Empty, SlotValue::Empty]);
        assert_eq!(context.frames.last().unwrap().field_id, FieldId(22));
        step(&mut context, Event::ArrayEnd { offset: 10 }, raw, b"").unwrap();
        assert_eq!(context.slots, vec![SlotValue::Empty, SlotValue::RawSpan(Span32::new(5, 6))]);
        assert!(!context.root_done);
        step(&mut context, Event::ObjectEnd { offset: 11 }, raw, b"").unwrap();
        assert!(context.root_done);
        assert!(context.frames.is_empty());
    }

    #[test]
    fn failed_child_constraints_leave_the_capture_empty() {
        let program = object_program(false, false);
        let selected = selection();
        let mut context = new_context(&program, &selected, VmLimits::default()).unwrap();
        let raw = b"{\"b\":[]}";
        for event in [Event::ObjectStart { offset: 0 }, raw_key(2, 1), Event::ArrayStart { offset: 5 }] {
            step(&mut context, event, raw, b"").unwrap();
        }
        let error = step(&mut context, Event::ArrayEnd { offset: 6 }, raw, b"").unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!((error.schema_id, error.field_id, error.byte_offset), (73, 22, 6));
        assert_eq!(context.slots[1], SlotValue::Empty);
        assert!(!context.root_done);
    }

    #[test]
    fn selecting_one_field_still_checks_all_fields_and_keeps_slot_count() {
        let program = object_program(false, false);
        let selected = Selection { field_ids: vec![FieldId(11)], strict_validate_unselected: true };
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), number(5, 1),
            raw_key(8, 1), Event::ArrayStart { offset: 11 }, null(12),
            Event::ArrayEnd { offset: 16 }, Event::ObjectEnd { offset: 17 }];
        let slots = drive(&program, &selected, VmLimits::default(), b"{\"a\":7,\"b\":[null]}", b"", &events).unwrap();
        assert_eq!(slots, vec![SlotValue::U64(7), SlotValue::Empty]);
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), Event::ArrayStart { offset: 5 },
            number(6, 1), Event::ArrayEnd { offset: 7 }, Event::ObjectEnd { offset: 8 }];
        let error = drive(&program, &selected, VmLimits::default(), b"{\"b\":[1]}", b"", &events).unwrap_err();
        assert_eq!(error.code, ErrorCode::TypeMismatch);
        assert_eq!(error.field_id, 22);
    }

    #[test]
    fn missing_required_fields_are_checked_even_without_strict_key_policies() {
        let program = object_program(true, false);
        let mut limits = VmLimits::default();
        limits.syntax.reject_duplicate_keys = false;
        limits.syntax.reject_unknown_keys = false;
        let error = drive(&program, &selection(), limits, b"{}", b"",
            &[Event::ObjectStart { offset: 0 }, Event::ObjectEnd { offset: 1 }]).unwrap_err();
        assert_eq!(error.code, ErrorCode::MissingRequiredField);
        assert_eq!((error.schema_id, error.field_id, error.byte_offset), (73, 11, 1));
    }

    #[test]
    fn unknown_objects_remain_unconstrained_but_enforce_runtime_limits() {
        let program = object_program(false, false);
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), Event::ObjectStart { offset: 5 },
            raw_key(7, 1), null(10), Event::ObjectEnd { offset: 14 }, Event::ObjectEnd { offset: 15 }];
        let raw = b"{\"x\":{\"y\":null}}";
        drive(&program, &selection(), limits, raw, b"", &events).unwrap();
        limits.syntax.max_fields_per_object = 0;
        assert_eq!(drive(&program, &selection(), limits, raw, b"", &events).unwrap_err().code,
            ErrorCode::BudgetExhausted);
        // Any/None has no whitelist: the default reject_unknown_keys flag must
        // not make every member of an unconstrained object an unknown error.
        drive(&any_program(), &selection(), VmLimits::default(), b"{\"x\":null}", b"",
            &[Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5), Event::ObjectEnd { offset: 9 }]).unwrap();
    }

    #[test]
    fn either_compiled_or_runtime_unknown_policy_rejects_unknown_keys() {
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5), Event::ObjectEnd { offset: 9 }];
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let program = object_program(false, true);
        assert_eq!(drive(&program, &selection(), limits, b"{\"x\":null}", b"", &events).unwrap_err().code,
            ErrorCode::UnknownObjectKey);
        let program = object_program(false, false);
        limits.syntax.reject_unknown_keys = true;
        assert_eq!(drive(&program, &selection(), limits, b"{\"x\":null}", b"", &events).unwrap_err().code,
            ErrorCode::UnknownObjectKey);
    }

    #[test]
    fn captured_fields_cannot_repeat_even_when_unselected_and_duplicates_are_allowed() {
        let program = object_program(false, false);
        let selected = Selection { field_ids: vec![FieldId(22)], strict_validate_unselected: true };
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), number(5, 1), raw_key(8, 1),
            number(11, 1), Event::ObjectEnd { offset: 12 }];
        let mut limits = VmLimits::default();
        let raw = b"{\"a\":1,\"a\":2}";
        assert_eq!(drive(&program, &selected, limits, raw, b"", &events).unwrap_err().code,
            ErrorCode::DuplicateObjectKey);
        limits.syntax.reject_duplicate_keys = false;
        assert_eq!(drive(&program, &selected, limits, raw, b"", &events).unwrap_err().code,
            ErrorCode::ConstraintViolation);
    }

    #[test]
    fn escaped_unknown_names_compare_by_decoded_bytes_and_report_raw_offsets() {
        let program = any_program();
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5),
            Event::Key { str_ref: StrRef { source: SpanSource::DecodedArena,
                span: Span32::new(0, 1), scalar_len: 1 } }];
        let error = drive(&program, &selection(), VmLimits::default(), b"{\"a\":null,\"\\u0061\":null}", b"a", &events)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
        assert_eq!(error.byte_offset, 10);
    }

    #[test]
    fn structural_errors_are_rejected_without_publishing_any_capability() {
        let program = any_program();
        let selected = selection();
        for (raw, events, expected) in [
            (&b""[..], vec![], ErrorCode::UnexpectedEof),
            (&b"["[..], vec![Event::ArrayStart { offset: 0 }], ErrorCode::UnexpectedEof),
            (&b"}"[..], vec![Event::ObjectEnd { offset: 0 }], ErrorCode::InvalidSyntax),
            (&b"[}"[..], vec![Event::ArrayStart { offset: 0 }, Event::ObjectEnd { offset: 1 }], ErrorCode::InvalidSyntax),
            (&b"null null"[..], vec![null(0), null(5)], ErrorCode::TrailingContent),
            (&b"{null}"[..], vec![Event::ObjectStart { offset: 0 }, null(1)], ErrorCode::InvalidSyntax),
            (&b"{\"a\":}"[..], vec![Event::ObjectStart { offset: 0 }, raw_key(2, 1),
                Event::ObjectEnd { offset: 5 }], ErrorCode::InvalidSyntax),
            (&b"{\"a\",\"b\"}"[..], vec![Event::ObjectStart { offset: 0 }, raw_key(2, 1),
                raw_key(6, 1)], ErrorCode::InvalidSyntax),
            (&b"[\"a\"]"[..], vec![Event::ArrayStart { offset: 0 }, raw_key(2, 1)], ErrorCode::InvalidSyntax),
        ] {
            let error = drive(&program, &selected, VmLimits::default(), raw, b"", &events).unwrap_err();
            assert_eq!(error.code, expected, "raw={:?}", raw);
            assert_eq!(error.schema_id, 73);
        }
    }

    #[test]
    fn recursive_schema_instances_have_independent_required_masks() {
        let program = compile(vec![TypeNode::Object(ObjectShape {
            properties: vec![property("next", 1, 0, false, None), property("v", 2, 1, true, None)],
            reject_unknown: true, min_properties: None, max_properties: None,
        }), TypeNode::Scalar(ScalarConstraints::new(ValueKind::Null))]);
        let raw = b"{\"v\":null,\"next\":{\"v\":null}}";
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5), raw_key(11, 4),
            Event::ObjectStart { offset: 17 }, raw_key(19, 1), null(22),
            Event::ObjectEnd { offset: 26 }, Event::ObjectEnd { offset: 27 }];
        drive(&program, &selection(), VmLimits::default(), raw, b"", &events).unwrap();
        let raw = b"{\"v\":null,\"next\":{}}";
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5), raw_key(11, 4),
            Event::ObjectStart { offset: 17 }, Event::ObjectEnd { offset: 18 }, Event::ObjectEnd { offset: 19 }];
        let error = drive(&program, &selection(), VmLimits::default(), raw, b"", &events).unwrap_err();
        assert_eq!(error.code, ErrorCode::MissingRequiredField);
        assert_eq!((error.field_id, error.byte_offset), (2, 18));
    }

    #[test]
    fn state_dispatch_cannot_fall_through_into_a_sibling_template() {
        let program = object_program(false, false);
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), Event::ArrayStart { offset: 5 }];
        let error = drive(&program, &selection(), VmLimits::default(), b"{\"a\":[]}", b"", &events).unwrap_err();
        assert_eq!(error.code, ErrorCode::TypeMismatch);
        assert_eq!(error.field_id, 11);
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5)];
        let error = drive(&program, &selection(), VmLimits::default(), b"{\"b\":null}", b"", &events).unwrap_err();
        assert_eq!(error.code, ErrorCode::TypeMismatch);
        assert_eq!(error.field_id, 22);
    }

    #[test]
    fn runtime_string_limits_use_utf8_bytes_and_validate_scalar_metadata() {
        let program = compile(vec![TypeNode::Scalar(ScalarConstraints::new(ValueKind::String))]);
        let raw = "\"é\"".as_bytes();
        let event = Event::Scalar { kind: ScalarKind::String, span: Span32::new(0, 4),
            decoded: Some(reference(1, 2, 1)) };
        let mut limits = VmLimits::default();
        limits.syntax.max_string_bytes = 2;
        drive(&program, &selection(), limits, raw, b"", &[event]).unwrap();
        limits.syntax.max_string_bytes = 1;
        assert_eq!(drive(&program, &selection(), limits, raw, b"", &[event]).unwrap_err().code,
            ErrorCode::BudgetExhausted);
        let bad = Event::Scalar { kind: ScalarKind::String, span: Span32::new(0, 4),
            decoded: Some(reference(1, 2, 2)) };
        assert_eq!(drive(&program, &selection(), VmLimits::default(), raw, b"", &[bad]).unwrap_err().code,
            ErrorCode::InternalError);
    }

    #[test]
    fn empty_strings_and_keys_are_valid_at_zero_byte_limits() {
        let program = compile(vec![TypeNode::Object(ObjectShape {
            properties: vec![property("", 1, 1, true, Some(0))], reject_unknown: true,
            min_properties: Some(1), max_properties: Some(1),
        }), TypeNode::Scalar(ScalarConstraints::new(ValueKind::String))]);
        let mut limits = VmLimits::default();
        limits.syntax.max_string_bytes = 0;
        limits.syntax.max_fields_per_object = 1;
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 0),
            Event::Scalar { kind: ScalarKind::String, span: Span32::new(4, 2),
                decoded: Some(reference(5, 0, 0)) }, Event::ObjectEnd { offset: 6 }];
        let slots = drive(&program, &selection(), limits, b"{\"\":\"\"}", b"", &events).unwrap();
        assert_eq!(slots, vec![SlotValue::Str(reference(5, 0, 0))]);
        revalidate_slots(&slots, b"{\"\":\"\"}", b"", &limits, &mut VmBudget::new(limits)).unwrap();
    }

    #[test]
    fn number_digit_limit_is_inclusive_and_shared_work_is_not_reset() {
        let program = any_program();
        let mut limits = VmLimits::default();
        limits.syntax.max_number_digits = 3;
        drive(&program, &selection(), limits, b"123", b"", &[number(0, 3)]).unwrap();
        limits.syntax.max_number_digits = 2;
        assert_eq!(drive(&program, &selection(), limits, b"123", b"", &[number(0, 3)]).unwrap_err().code,
            ErrorCode::BudgetExhausted);
        limits = VmLimits::default();
        let selected = selection();
        let mut measured = new_context(&program, &selected, limits).unwrap();
        step(&mut measured, null(0), b"null", b"").unwrap();
        let used = limits.max_steps - measured.budget.steps_left();
        assert!(used >= 2); // The event and its instruction both consume work.
        limits.max_steps = used;
        run(&program, NullInput::new(), &selection(), limits).unwrap();
        limits.max_steps = used - 1;
        assert_eq!(run(&program, NullInput::new(), &selection(), limits).unwrap_err().code,
            ErrorCode::BudgetExhausted);
    }

    #[test]
    fn invalid_scalar_tokens_and_metadata_cannot_complete_the_root() {
        let program = any_program();
        for (raw, event, expected) in [
            (&b"true"[..], null(0), ErrorCode::InvalidSyntax),
            (&b"null"[..], Event::Scalar { kind: ScalarKind::Null, span: Span32::new(0, 4),
                decoded: Some(reference(0, 4, 4)) }, ErrorCode::InternalError),
            (&b"\"a\""[..], Event::Scalar { kind: ScalarKind::String, span: Span32::new(0, 3),
                decoded: None }, ErrorCode::InternalError),
            (&b"null"[..], Event::Scalar { kind: ScalarKind::Null, span: Span32::new(u32::MAX, 2),
                decoded: None }, ErrorCode::InternalError),
            (&b""[..], Event::Scalar { kind: ScalarKind::Null, span: Span32::new(0, 0),
                decoded: None }, ErrorCode::InvalidSyntax),
            (&b"01"[..], number(0, 2), ErrorCode::InvalidNumber),
            (&b"1e"[..], number(0, 2), ErrorCode::InvalidNumber),
        ] {
            let selected = selection();
            let mut context = new_context(&program, &selected, VmLimits::default()).unwrap();
            assert_eq!(step(&mut context, event, raw, b"").unwrap_err().code, expected);
            assert!(!context.root_done);
        }
    }

    #[test]
    fn input_limits_accept_the_boundary_and_reject_the_next_byte() {
        let mut limits = VmLimits::default();
        limits.syntax.max_payload_bytes = 4;
        limits.syntax.max_decoded_bytes = 2;
        assert_eq!(buffer_lengths(b"null", b"ab", &limits).unwrap(), (4, 2));
        assert_eq!(buffer_lengths(b"null ", b"ab", &limits).unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert_eq!(buffer_lengths(b"null", b"abc", &limits).unwrap_err().code, ErrorCode::DecodedArenaOverflow);
        limits.syntax.max_payload_bytes = 0;
        limits.syntax.max_decoded_bytes = 0;
        assert_eq!(buffer_lengths(b"", b"", &limits).unwrap(), (0, 0));
    }

    #[test]
    fn scratch_charges_slots_and_retained_frame_capacity() {
        let program = object_program(false, false);
        let selected = selection();
        let mut limits = VmLimits::default();
        let slot_bytes = allocation_bytes::<SlotValue>(2, 0).unwrap();
        limits.max_scratch_bytes = slot_bytes - 1;
        assert_eq!(new_context(&program, &selected, limits).err().unwrap().code, ErrorCode::BudgetExhausted);
        limits.max_scratch_bytes = slot_bytes;
        let mut context = new_context(&program, &selected, limits).unwrap();
        assert_eq!(context.budget.scratch_bytes(), slot_bytes);
        assert_eq!(step(&mut context, Event::ObjectStart { offset: 0 }, b"{}", b"").unwrap_err().code,
            ErrorCode::BudgetExhausted);
        let program = any_program();
        let mut context = new_context(&program, &selected, VmLimits::default()).unwrap();
        step(&mut context, Event::ObjectStart { offset: 0 }, b"{\"a\":null}", b"").unwrap();
        let frame_bytes = allocation_bytes::<VmFrame>(context.frames.capacity(), 0).unwrap();
        step(&mut context, raw_key(2, 1), b"{\"a\":null}", b"").unwrap();
        assert!(context.budget.scratch_bytes() > frame_bytes);
        step(&mut context, null(5), b"{\"a\":null}", b"").unwrap();
        step(&mut context, Event::ObjectEnd { offset: 9 }, b"{\"a\":null}", b"").unwrap();
        assert_eq!(context.budget.scratch_bytes(), frame_bytes);
    }

    #[test]
    fn scratch_arithmetic_and_array_counter_overflow_are_errors() {
        assert_eq!(allocation_bytes::<VmFrame>(usize::MAX, 17).unwrap_err().code, ErrorCode::BudgetExhausted);
        let program = any_program();
        let selected = selection();
        let mut context = new_context(&program, &selected, VmLimits::default()).unwrap();
        context.frames.push(VmFrame { state: None, field_id: FieldId(9), start: 0, slot: None,
            kind: FrameKind::Array { count: u32::MAX, items: None } });
        let error = take_value(&mut context, 5).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        match context.frames[0].kind {
            FrameKind::Array { count, .. } => assert_eq!(count, u32::MAX),
            _ => panic!("expected array frame"),
        }
    }

    #[test]
    fn final_owner_checks_reject_equal_length_replacements() {
        let limits = VmLimits::default();
        let raw = b"null".to_vec();
        let decoded = b"a".to_vec();
        let replacement = raw.clone();
        let arena_replacement = decoded.clone();
        verify_owners(&raw, &decoded, (4, 1), raw.as_ptr(), decoded.as_ptr(), &limits).unwrap();
        assert_eq!(verify_owners(&replacement, &decoded, (4, 1), raw.as_ptr(), decoded.as_ptr(), &limits)
            .unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(verify_owners(&raw, &arena_replacement, (4, 1), raw.as_ptr(), decoded.as_ptr(), &limits)
            .unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(verify_owners(&raw, &decoded, (3, 1), raw.as_ptr(), decoded.as_ptr(), &limits)
            .unwrap_err().code, ErrorCode::InternalError);
    }

    #[test]
    fn final_slot_validation_checks_ranges_utf8_counts_and_work() {
        let limits = VmLimits::default();
        let raw = b"null";
        let decoded = "é".as_bytes();
        let string = StrRef { source: SpanSource::DecodedArena, span: Span32::new(0, 2), scalar_len: 1 };
        let slots = [SlotValue::Empty, SlotValue::RawSpan(Span32::new(0, 4)), SlotValue::Str(string)];
        revalidate_slots(&slots, raw, decoded, &limits, &mut VmBudget::new(limits)).unwrap();
        for bad in [SlotValue::RawSpan(Span32::new(0, 0)), SlotValue::RawSpan(Span32::new(3, 2)),
            SlotValue::RawSpan(Span32::new(u32::MAX, 1)),
            SlotValue::Str(StrRef { scalar_len: 2, ..string })]
        {
            assert_eq!(revalidate_slots(&[bad], raw, decoded, &limits, &mut VmBudget::new(limits))
                .unwrap_err().code, ErrorCode::InternalError);
        }
        let bad = SlotValue::Str(StrRef { span: Span32::new(1, 1), ..string });
        assert_eq!(revalidate_slots(&[bad], raw, decoded, &limits, &mut VmBudget::new(limits))
            .unwrap_err().code, ErrorCode::InvalidUtf8);
        let mut exhausted = limits;
        exhausted.max_steps = 0;
        assert_eq!(revalidate_slots(&slots, raw, decoded, &limits, &mut VmBudget::new(exhausted))
            .unwrap_err().code, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn raw_key_cursor_handles_escapes_and_reports_unclosed_tokens() {
        let limits = VmLimits::default();
        let mut budget = VmBudget::new(limits);
        assert_eq!(next_key_token(b" , \"a\\\"b\":null", 0, &mut budget).unwrap(), (3, 9));
        assert_eq!(next_key_token(b"\"\"", 0, &mut budget).unwrap(), (0, 2));
        assert_eq!(next_key_token(b"\"abc", 0, &mut budget).unwrap_err().code, ErrorCode::UnclosedString);
        assert_eq!(next_key_token(b"null", 0, &mut budget).unwrap_err().code, ErrorCode::InvalidSyntax);
        assert_eq!(next_key_token(b"", 0, &mut budget).unwrap_err().code, ErrorCode::UnexpectedEof);
        assert_eq!(event_end(Event::ArrayEnd { offset: u32::MAX }).unwrap_err().code, ErrorCode::InternalError);
    }

    #[test]
    fn any_captured_values_keep_the_complete_raw_token() {
        let program = compile(vec![TypeNode::Object(ObjectShape {
            properties: vec![property("a", 1, 1, false, Some(0))], reject_unknown: true,
            min_properties: None, max_properties: None,
        }), TypeNode::Any]);
        let events = [Event::ObjectStart { offset: 0 }, raw_key(2, 1), null(5), Event::ObjectEnd { offset: 9 }];
        let slots = drive(&program, &selection(), VmLimits::default(), b"{\"a\":null}", b"", &events).unwrap();
        assert_eq!(slots, vec![SlotValue::RawSpan(Span32::new(5, 4))]);
    }
}
