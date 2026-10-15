//! Bounded event execution for a witnessed schema program.
//!
//! Syntax ownership is supplied only by the audited `EventInput` boundary.
//! This module never creates a proof from an arbitrary collection of events.
use super::*;
use core::mem::size_of;
use jmax_abi::schema::{InsnWord, Opcode};
use jmax_schema_compile::decode_opcode;

/// Accepted owners for a cursor input. A borrowed cursor must be fresh and
/// contain the complete input; the adapter declares EOF before traversal.
#[derive(Debug)]
pub enum CursorSource<'a> {
    Payload(Payload),
    Cursor(&'a mut jmax_cursor::Cursor),
}

impl From<Payload> for CursorSource<'_> {
    fn from(payload: Payload) -> Self { Self::Payload(payload) }
}
impl<'a> From<&'a mut jmax_cursor::Cursor> for CursorSource<'a> {
    fn from(cursor: &'a mut jmax_cursor::Cursor) -> Self { Self::Cursor(cursor) }
}

#[derive(Debug)]
enum CursorOwner<'a> {
    Owned(jmax_cursor::Cursor),
    Borrowed(&'a mut jmax_cursor::Cursor),
}

/// Strict cursor bridge. The decoded accessor always borrows the live arena,
/// including escaped keys and strings decoded since the previous event.
/// A successful publication consumes the adapter. For a borrowed cursor it
/// transfers the completed cursor's buffers and leaves a fresh strict cursor.
#[derive(Debug)]
pub struct CursorInput<'a> {
    cursor: Option<CursorOwner<'a>>,
    limits: Limits,
    numeric: jmax_abi::number::NumericPolicy,
    finished: bool,
    failure: Option<Error>,
}

impl<'a> CursorInput<'a> {
    pub fn new(source: impl Into<CursorSource<'a>>, limits: Limits,
        numeric: jmax_abi::number::NumericPolicy) -> Result<Self>
    {
        if numeric.max_exponent_abs < 0 {
            return Err(vm_error(ErrorCode::ConstraintViolation, 0,
                "numeric exponent limit cannot be negative"));
        }
        let cursor = match source.into() {
            CursorSource::Payload(payload) => CursorOwner::Owned(jmax_cursor::walk::from_payload(
                payload, limits, jmax_cursor::SkipMode::Strict,
            )?),
            CursorSource::Cursor(cursor) => {
                let state = cursor.state();
                if cursor.mode() != jmax_cursor::SkipMode::Strict || state.byte_offset != 0
                    || state.depth != 0 || state.finished || cursor.current().is_some()
                    || !cursor.decoded_bytes().is_empty()
                {
                    return Err(vm_error(ErrorCode::ConstraintViolation, state.byte_offset,
                        "schema validation requires a fresh strict cursor"));
                }
                jmax_cursor::walk::end_input(cursor)?;
                CursorOwner::Borrowed(cursor)
            }
        };
        let input = Self { cursor: Some(cursor), limits, numeric, finished: false, failure: None };
        buffer_lengths(input.raw_bytes(), input.decoded_bytes(), &VmLimits {
            syntax: limits, ..VmLimits::default()
        })?;
        Ok(input)
    }

    fn cursor(&self) -> Option<&jmax_cursor::Cursor> {
        match self.cursor.as_ref()? {
            CursorOwner::Owned(cursor) => Some(cursor),
            CursorOwner::Borrowed(cursor) => Some(cursor),
        }
    }
    fn cursor_mut(&mut self) -> Result<&mut jmax_cursor::Cursor> {
        match self.cursor.as_mut() {
            Some(CursorOwner::Owned(cursor)) => Ok(cursor),
            Some(CursorOwner::Borrowed(cursor)) => Ok(cursor),
            None => Err(internal(0)),
        }
    }
    fn advance(&mut self) -> Result<Option<Event>> {
        if let Some(error) = &self.failure { return Err(error.clone()); }
        if self.cursor.is_none() { return Err(internal(0)); }
        if self.finished { return Ok(None); }
        let result = match jmax_cursor::walk::next(self.cursor_mut()?) {
            Ok(jmax_cursor::ScanStep::Emit(event)) => Ok(Some(event)),
            Ok(jmax_cursor::ScanStep::Finished) => { self.finished = true; Ok(None) }
            Ok(jmax_cursor::ScanStep::NeedMore) => Err(vm_error(
                ErrorCode::UnexpectedEof, self.cursor().map_or(0, |cursor| cursor.state().byte_offset),
                "complete schema input requested additional bytes")),
            Err(error) => Err(error),
        };
        if let Err(error) = &result { self.failure = Some(error.clone()); }
        result
    }
    fn take_buffers(&mut self) -> Result<InputBuffers> {
        if let Some(error) = &self.failure { return Err(error.clone()); }
        if !self.finished {
            return Err(vm_error(ErrorCode::UnexpectedEof, 0,
                "cursor buffers cannot be published before validated EOF"));
        }
        let owner = self.cursor.take().ok_or_else(|| internal(0))?;
        let cursor = match owner {
            CursorOwner::Owned(cursor) => cursor,
            CursorOwner::Borrowed(cursor) => core::mem::replace(cursor,
                jmax_cursor::Cursor::new(self.limits, jmax_cursor::SkipMode::Strict)),
        };
        let output = jmax_cursor::resume::finish(cursor)?;
        if !output.fully_validated { return Err(internal(0)); }
        Ok(InputBuffers { payload: output.payload, decoded: output.decoded.into_boxed_slice() })
    }
}

// SAFETY: Construction requires the entire input and an untouched strict cursor.
// Every event comes directly from the syntax parser. EOF is exposed only after
// Finished, and publication transfers its exact raw bytes and append-only arena.
unsafe impl EventInput for CursorInput<'_> {
    fn raw_bytes(&self) -> &[u8] { self.cursor().map_or(&[], jmax_cursor::Cursor::raw_bytes) }
    fn decoded_bytes(&self) -> &[u8] { self.cursor().map_or(&[], jmax_cursor::Cursor::decoded_bytes) }
    fn next_event(&mut self) -> Result<Option<Event>> { self.advance() }
    fn numeric_policy(&self) -> Option<jmax_abi::number::NumericPolicy> { Some(self.numeric) }
    fn into_buffers(mut self) -> Result<InputBuffers> { self.take_buffers() }
}

// SAFETY: Exclusive access prevents any concurrent cursor changes. Extraction
// empties the adapter once; reuse fails instead of exposing a second event stream.
unsafe impl EventInput for &mut CursorInput<'_> {
    fn raw_bytes(&self) -> &[u8] { (**self).raw_bytes() }
    fn decoded_bytes(&self) -> &[u8] { (**self).decoded_bytes() }
    fn next_event(&mut self) -> Result<Option<Event>> { (**self).advance() }
    fn numeric_policy(&self) -> Option<jmax_abi::number::NumericPolicy> { Some(self.numeric) }
    fn into_buffers(self) -> Result<InputBuffers> { self.take_buffers() }
}

/// Validate the complete input and retain its exact immutable owners.
pub fn run<I: EventInput>(
    program: &ValidatedProgram,
    mut input: I,
    selection: &Selection,
    limits: VmLimits,
) -> Result<ValidatedRun> {
    let result = (|| {
        let mut lengths = buffer_lengths(input.raw_bytes(), input.decoded_bytes(), &limits)?;
        let raw_owner = input.raw_bytes().as_ptr();
        let numeric = input.numeric_policy();
        let mut context = new_context(program, selection, limits)?;
        // This cursor exists only to locate raw key tokens. Key events carry a
        // cooked reference, which may have no raw offset at all.
        let mut token_end = 0u32;
        loop {
            let event = input.next_event().map_err(|error| annotate(
                program, active_field(&context), error,
            ))?;
            lengths = verify_owners(input.raw_bytes(), input.decoded_bytes(), lengths,
                raw_owner, &limits)?;
            let Some(event) = event else { break };
            let raw = input.raw_bytes();
            let decoded = input.decoded_bytes();
            if let (Some(policy), Event::Scalar { kind: ScalarKind::Number, span, .. }) = (numeric, event) {
                let result = constraints::check_input_number(
                    span.slice(raw).ok_or_else(|| internal(span.offset))?,
                    &policy, &mut context.budget, span.offset,
                );
                result.map_err(|error| annotate(program, active_field(&context), error))?;
            }
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
        // Ownership conversion may move an allocation. EventInput guarantees
        // exact contents; lengths and retained references are checked again.
        if buffer_lengths(buffers.payload.as_slice(), &buffers.decoded, &limits)? != lengths {
            return Err(internal(0));
        }
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
    let selection = selection_index(program, selection, &mut budget)?;
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

pub(crate) fn selection_index(program: &ValidatedProgram, selection: &Selection,
    budget: &mut VmBudget) -> Result<extract::SelectionIndex>
{
    if !selection.strict_validate_unselected {
        return Err(vm_error(ErrorCode::UnsupportedSchemaFeature, 0,
            "proof publication requires validation of unselected fields"));
    }
    let slots_len = program.program().slots.len();
    if selection.field_ids.len() > slots_len {
        return Err(vm_error(ErrorCode::ConstraintViolation, 0,
            "selection contains more fields than the compiled slots"));
    }
    let all = selection.field_ids.is_empty();
    let mut words = Vec::new();
    if !all {
        let word_count = slots_len.checked_add(63).ok_or_else(|| resource_error(0))? / 64;
        reserve(&mut words, word_count, budget, 0)?;
        words.resize(word_count, 0u64);
        for field in &selection.field_ids {
            budget.charge(2, 0)?;
            let spec = program.slot_for_field(*field).ok_or_else(|| vm_error(
                ErrorCode::UnknownObjectKey, 0, "selection field has no compiled slot")
                .with_schema(program.image().header.schema_id, field.0))?;
            let index = spec.slot as usize;
            let word = words.get_mut(index / 64).ok_or_else(|| internal(0))?;
            let bit = 1u64 << (index % 64);
            if *word & bit != 0 {
                return Err(vm_error(ErrorCode::ConstraintViolation, 0,
                    "selection contains a duplicate field")
                    .with_schema(program.image().header.schema_id, field.0));
            }
            *word |= bit;
        }
    }
    Ok(extract::SelectionIndex { all, words })
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
    raw: &[u8], decoded: &[u8], previous: (u32, u32),
    raw_owner: *const u8, limits: &VmLimits,
) -> Result<(u32, u32)> {
    let lengths = buffer_lengths(raw, decoded, limits)?;
    if lengths.0 != previous.0 || lengths.1 < previous.1
        || (!raw.is_empty() && raw.as_ptr() != raw_owner)
    {
        return Err(vm_error(ErrorCode::InternalError, 0,
            "event input changed raw storage or truncated the decoded arena"));
    }
    // Previously decoded prefixes are immutable by EventInput's contract.
    // Reallocation is permitted, so no decoded pointer is cached across steps.
    Ok(lengths)
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
    let pending = objects::key(context.program, object, text, &context.selection,
        &context.limits, &mut context.budget, offset)?;
    // Image validation already proved the EmitSlot operand and owning object.
    // Resolve its immutable side-table entry directly instead of scanning code.
    if let Some(slot) = pending.slot {
        context.budget.charge(1, offset)?;
        let spec = context.program.slot_for_field(pending.field_id).ok_or_else(|| internal(offset))?;
        if spec.slot != slot || Some(spec.state) != pending.state { return Err(internal(offset)); }
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
