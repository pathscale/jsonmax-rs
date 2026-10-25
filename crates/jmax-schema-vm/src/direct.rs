//! Monomorphic complete-input execution. No events, cursor adapter, or token
//! buffers are constructed. Grammar state lives alongside the VM's open frames.
use super::*;
use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::{DuplicatePolicyOwner, FramePhase, SkipMode, TokenKind};
use jmax_schema_compile::scalar_kind_bit;
use jmax_syntax::block_cursor::BlockCursor;

fn invalid(offset: u32, detail: &'static str) -> Error {
    vm_error(ErrorCode::InvalidSyntax, offset, detail)
}
fn resource(offset: u32) -> Error {
    vm_error(
        ErrorCode::BudgetExhausted,
        offset,
        "direct schema VM resource limit exceeded",
    )
}

/// Fingerprint all requested policies, including numeric and VM work/storage
/// limits. Syntax-only capabilities cannot authorize a schema execution skip.
pub fn policy_fingerprint(limits: VmLimits, numeric: NumericPolicy) -> u64 {
    let mut hash = limits.syntax.policy_fingerprint();
    for word in [
        limits.max_steps,
        u64::from(limits.max_scratch_bytes),
        u64::from(numeric.allow_Quoted_int),
        u64::from(numeric.reject_non_finite_float),
        u64::from(numeric.reject_negative_zero),
        numeric.max_exponent_abs as u16 as u64,
    ] {
        hash = (hash ^ word).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn combined_numeric(program: &ValidatedProgram, requested: NumericPolicy) -> Result<NumericPolicy> {
    let schema = program.program().policy.numeric;
    if requested.max_exponent_abs < 0 || schema.max_exponent_abs < 0 {
        return Err(vm_error(
            ErrorCode::ConstraintViolation,
            0,
            "negative numeric exponent limit",
        ));
    }
    Ok(NumericPolicy {
        allow_Quoted_int: requested.allow_Quoted_int && schema.allow_Quoted_int,
        reject_non_finite_float: requested.reject_non_finite_float
            || schema.reject_non_finite_float,
        reject_negative_zero: requested.reject_negative_zero || schema.reject_negative_zero,
        max_exponent_abs: requested.max_exponent_abs.min(schema.max_exponent_abs),
    })
}

fn prepare_output(scratch: &mut ValidationScratch) {
    if let Some(output) = scratch.output.take() {
        scratch.vm.slots = output.slots;
        scratch.direct_decoded = output.decoded;
    }
    scratch.direct_decoded.clear();
}

fn recover(mut context: VmContext<'_>, scratch: &mut VmScratch) {
    // Restore per-depth tables on failures as well as successful EOF.
    for (depth, frame) in context.frames.drain(..).enumerate() {
        if let FrameKind::Object(object) = frame.kind {
            if let Some(table) = context.unknown_key_tables.get_mut(depth) {
                if object.unknown_keys.has_storage() {
                    *table = object.unknown_keys;
                }
            }
            if let Some(mask) = context.seen_masks.get_mut(depth) {
                if object.seen_words.capacity() != 0 {
                    *mask = object.seen_words;
                }
            }
        }
    }
    scratch.frames = context.frames;
    scratch.unknown_key_tables = context.unknown_key_tables;
    scratch.seen_masks = context.seen_masks;
    scratch.slots = context.slots;
    scratch.selection_words = context.selection.words;
}

fn selected_states(
    context: &mut VmContext<'_>,
    selection: &Selection,
    needed: &mut Vec<bool>,
    queue: &mut Vec<StateId>,
) -> Result<()> {
    needed.clear();
    queue.clear();
    execute::reserve(
        needed,
        context.program.program().states.len(),
        &mut context.budget,
        0,
    )?;
    needed.resize(context.program.program().states.len(), false);
    if selection.field_ids.is_empty() {
        for slot in &context.program.program().slots {
            if !needed[slot.state.0 as usize] {
                needed[slot.state.0 as usize] = true;
                execute::reserve(queue, 1, &mut context.budget, 0)?;
                queue.push(slot.state);
            }
        }
    } else {
        for field in &selection.field_ids {
            let slot = context
                .program
                .slot_for_field(*field)
                .ok_or_else(|| invalid(0, "missing selected field slot"))?;
            if !needed[slot.state.0 as usize] {
                needed[slot.state.0 as usize] = true;
                execute::reserve(queue, 1, &mut context.budget, 0)?;
                queue.push(slot.state);
            }
        }
    }
    let mut cursor = 0;
    while let Some(&child) = queue.get(cursor) {
        for &parent in context
            .program
            .parents(child)
            .ok_or_else(|| invalid(0, "missing state parents"))?
        {
            context.budget.charge(1, 0)?;
            if !needed[parent.0 as usize] {
                needed[parent.0 as usize] = true;
                execute::reserve(queue, 1, &mut context.budget, 0)?;
                queue.push(parent);
            }
        }
        cursor += 1;
    }
    Ok(())
}

struct Engine<'a, 'ctx, 'buf> {
    cursor: BlockCursor<'a>,
    context: &'buf mut VmContext<'ctx>,
    decoded: &'buf mut Vec<u8>,
    needed: &'buf [bool],
    query: bool,
    numeric: NumericPolicy,
    decoded_capacity: usize,
    // Only supplied by a live immutable BorrowedValidatedSlots guard, never
    // by accepting an arbitrary Copy pointer token from caller code.
    validated: Option<ValidationCapability>,
}

impl Engine<'_, '_, '_> {
    fn charge_decoded(&mut self) -> Result<()> {
        let actual = self.decoded.capacity();
        let growth = actual
            .checked_sub(self.decoded_capacity)
            .ok_or_else(|| resource(self.cursor.offset()))?;
        self.context.budget.retain(
            u32::try_from(growth).map_err(|_| resource(self.cursor.offset()))?,
            self.cursor.offset(),
        )?;
        self.decoded_capacity = actual;
        Ok(())
    }

    fn value(&mut self) -> Result<()> {
        let byte = self.cursor.skip_whitespace_uncached()?.ok_or_else(|| {
            vm_error(
                ErrorCode::UnexpectedEof,
                self.cursor.offset(),
                "missing value",
            )
        })?;
        let offset = self.cursor.offset();
        self.context.budget.charge(1, offset)?;
        let pending = execute::take_value(self.context, offset)?;
        let result = self.value_pending(byte, pending);
        result.map_err(|error| {
            let field = if error.field_id == 0 {
                pending.field_id.0
            } else {
                error.field_id
            };
            error.with_schema(self.context.program.image().header.schema_id, field)
        })
    }

    fn value_pending(&mut self, byte: u8, pending: PendingValue) -> Result<()> {
        let offset = self.cursor.offset();
        if byte == b'[' && !self.query {
            if let Some(span) = self
                .fast_fixed_struct_array(pending, offset)
                .or_else(|| self.fast_struct_array(pending, offset))
            {
                if let Some(slot) = pending.slot {
                    extract::store(
                        &mut self.context.slots,
                        slot,
                        SlotValue::RawSpan(span),
                        offset,
                    )?;
                }
                if self.context.frames.is_empty() {
                    self.context.root_done = true;
                }
                return Ok(());
            }
            if let Some(span) = self.fast_decimal_pairs(pending, offset) {
                if let Some(slot) = pending.slot {
                    extract::store(
                        &mut self.context.slots,
                        slot,
                        SlotValue::RawSpan(span),
                        offset,
                    )?;
                }
                if self.context.frames.is_empty() {
                    self.context.root_done = true;
                }
                return Ok(());
            }
        }
        if byte == b'{'
            && !self.query
            && let Some(len) = self.fast_fixed_object(pending, offset)
        {
            if let Some(slot) = pending.slot {
                extract::store(
                    &mut self.context.slots,
                    slot,
                    SlotValue::RawSpan(Span32::new(offset, len)),
                    offset,
                )?;
            }
            if self.context.frames.is_empty() {
                self.context.root_done = true;
            }
            return Ok(());
        }
        let no_capture = pending.slot.is_none();
        let unconstrained = match pending.state {
            None => true,
            Some(id) => self.context.program.state(id).is_some_and(|state| {
                state.kind == StateKind::Any && state.constraints == ConstraintSet::default()
            }),
        };
        let unselected = no_capture
            && pending
                .state
                .is_none_or(|id| !self.needed.get(id.0 as usize).copied().unwrap_or(false));
        if (self.query && unselected) || unconstrained {
            let mode = if self.query && unselected {
                SkipMode::QueryOnly
            } else if let Some(capability) = self.validated {
                SkipMode::ValidatedOwner(capability)
            } else {
                SkipMode::StrictValidate
            };
            let depth = u16::try_from(self.context.frames.len()).map_err(|_| resource(offset))?;
            let span = self.cursor.skip_value(self.decoded, depth, mode)?;
            self.charge_decoded()?;
            if let Some(slot) = pending.slot {
                extract::store(
                    &mut self.context.slots,
                    slot,
                    SlotValue::RawSpan(span),
                    offset,
                )?;
            }
            if self.context.frames.is_empty() {
                self.context.root_done = true;
            }
            return Ok(());
        }
        match byte {
            b'{' | b'[' => {
                let kind = if byte == b'{' {
                    ContainerKind::Object
                } else {
                    ContainerKind::Array
                };
                execute::open_pending(self.context, kind, pending, offset)?;
                self.cursor.advance(1);
            }
            _ => {
                let (kind, span, reference) = match byte {
                    b'"' => {
                        let meta = self.cursor.scan_string_meta_uncached(self.decoded)?;
                        self.charge_decoded()?;
                        (ScalarKind::String, meta.raw_token, Some(meta.cooked))
                    }
                    b'-' | b'0'..=b'9' => (
                        ScalarKind::Number,
                        self.cursor.scan_number_span_uncached()?,
                        None,
                    ),
                    b't' | b'f' | b'n' => {
                        let (kind, span) = self.cursor.scan_literal()?;
                        let kind = match kind {
                            TokenKind::True => ScalarKind::True,
                            TokenKind::False => ScalarKind::False,
                            TokenKind::Null => ScalarKind::Null,
                            _ => return Err(invalid(offset, "invalid scalar literal metadata")),
                        };
                        (kind, span, None)
                    }
                    _ => return Err(invalid(offset, "expected JSON value")),
                };
                if let Some(id) = pending.state {
                    let dispatch = self
                        .context
                        .program
                        .dispatch(id)
                        .ok_or_else(|| invalid(offset, "missing state dispatch"))?;
                    if dispatch.scalar_mask & scalar_kind_bit(kind) == 0 {
                        return Err(vm_error(
                            ErrorCode::TypeMismatch,
                            offset,
                            "scalar kind does not match schema",
                        ));
                    }
                }
                if kind == ScalarKind::String
                    && pending.state.is_some_and(|id| {
                        self.context.program.state(id).is_some_and(|state| {
                            matches!(
                                state.kind,
                                StateKind::Scalar(ValueKind::Integer | ValueKind::Decimal)
                            )
                        })
                    })
                {
                    let reference =
                        reference.ok_or_else(|| invalid(offset, "missing numeric string"))?;
                    let owner = match reference.source {
                        SpanSource::RawPayload => self.cursor.input(),
                        SpanSource::DecodedArena => self.decoded.as_slice(),
                    };
                    let bytes = reference
                        .span
                        .slice(owner)
                        .ok_or_else(|| invalid(offset, "invalid numeric string"))?;
                    self.context.budget.charge(bytes.len() as u64, offset)?;
                    let max_digits = u32::from(self.context.limits.syntax.max_number_digits);
                    let mut digits = 0u32;
                    for byte in bytes {
                        if byte.is_ascii_digit() {
                            digits += 1;
                            if digits > max_digits {
                                return Err(resource(offset));
                            }
                        }
                    }
                }
                if kind == ScalarKind::String
                    && !self.numeric.allow_Quoted_int
                    && pending.state.is_some_and(|id| {
                        self.context.program.state(id).is_some_and(|state| {
                            state.kind == StateKind::Scalar(ValueKind::Integer)
                        })
                    })
                {
                    return Err(vm_error(
                        ErrorCode::TypeMismatch,
                        offset,
                        "request numeric policy forbids quoted integers",
                    ));
                }
                let value = constraints::check_scalar_trusted(
                    self.context.program,
                    pending.state,
                    ScalarInput {
                        kind,
                        span,
                        decoded: reference,
                        raw: self.cursor.input(),
                        arena: self.decoded,
                    },
                    &mut self.context.budget,
                )?;
                if let Some(slot) = pending.slot {
                    extract::store(&mut self.context.slots, slot, value, offset)?;
                }
                if self.context.frames.is_empty() {
                    self.context.root_done = true;
                }
            }
        }
        Ok(())
    }

    /// Minified scalar rows share a schema-derived plan and the preceding
    /// row's key order. Every miss is transactional and reuses the exact VM.
    fn fast_fixed_struct_array(&mut self, pending: PendingValue, start: u32) -> Option<Span32> {
        if pending
            .slot
            .is_some_and(|s| self.context.slots.get(s as usize) != Some(&SlotValue::Empty))
        {
            return None;
        }
        let program = self.context.program;
        let outer = program.state(pending.state?)?;
        let StateKind::Array { items } = outer.kind else {
            return None;
        };
        let object = program.state(items)?;
        if object.constraints != ConstraintSet::default() {
            return None;
        }
        let StateKind::Object {
            key_table,
            reject_unknown: true,
            ..
        } = object.kind
        else {
            return None;
        };
        let table = program.program().key_tables.get(key_table as usize)?;
        let size = table.key_count as usize;
        if size == 0 || size > 64 {
            return None;
        }
        let depth = self.context.frames.len().checked_add(2)?;
        if depth > usize::from(self.context.limits.syntax.max_depth)
            || depth > self.context.frames.capacity()
        {
            return None;
        }
        let entries = program
            .image()
            .keys
            .get(table.first_key as usize..table.first_key as usize + size)?;
        let mut plan: [Option<FixedProperty<'_>>; 64] = [None; 64];
        for (index, entry) in entries.iter().enumerate() {
            if program
                .slot_for_field(FieldId(u32::from(entry.field_id)))
                .is_some_and(|s| extract::selected(&self.context.selection, s.slot))
            {
                return None;
            }
            let state = program.state(StateId(entry.target_state))?;
            let StateKind::Scalar(kind) = state.kind else {
                return None;
            };
            match kind {
                ValueKind::Decimal => {
                    let decimal = state.constraints.decimal.as_ref()?;
                    jmax_decimal::validate_policy(&decimal.policy).ok()?;
                    if decimal.minimum.is_some() || decimal.maximum.is_some() {
                        return None;
                    }
                }
                ValueKind::Integer
                    if state.constraints
                        == (ConstraintSet {
                            min_i64: state.constraints.min_i64,
                            max_i64: state.constraints.max_i64,
                            ..ConstraintSet::default()
                        }) => {}
                ValueKind::String | ValueKind::Boolean | ValueKind::Null
                    if state.constraints == ConstraintSet::default() => {}
                _ => return None,
            }
            let name = entry.span.slice(&program.image().key_bytes)?;
            if name.len() > self.context.limits.syntax.max_string_bytes as usize
                || name
                    .iter()
                    .any(|&b| b >= 0x80 || b < 0x20 || b == b'"' || b == b'\\')
            {
                return None;
            }
            let small_integer = matches!(state.constraints.min_i64, None | Some(i64::MIN))
                && matches!(state.constraints.max_i64, None | Some(i64::MAX));
            plan[index] = Some(FixedProperty {
                name,
                state: Some(state),
                kind,
                small_integer,
            });
        }
        let passes = match table.strategy {
            jmax_schema_compile::KeyStrategy::Swar => u64::from(table.key_count) + 1,
            jmax_schema_compile::KeyStrategy::PerfectHash { .. } => 2,
            jmax_schema_compile::KeyStrategy::BucketedPerfectHash { .. } => 3,
        };
        let required = program
            .required_mask(key_table)?
            .first()
            .copied()
            .unwrap_or(0);
        let input = self.cursor.input();
        let limits = self.context.limits.syntax;
        let mut at = start as usize + 1;
        let mut count = 0u32;
        let mut order = [0usize; 64];
        let mut order_len = 0usize;
        let mut work = 0u64;
        if input.get(at) == Some(&b']') {
            work = 1;
        } else {
            loop {
                if count == limits.max_array_items || input.get(at) != Some(&b'{') {
                    return None;
                }
                at += 1;
                let mut seen = 0u64;
                let mut fields = 0usize;
                while input.get(at) != Some(&b'}') {
                    if fields >= size
                        || fields >= limits.max_fields_per_object as usize
                        || input.get(at) != Some(&b'"')
                    {
                        return None;
                    }
                    let index = if count != 0 && fields < order_len {
                        order[fields]
                    } else {
                        let end = input[at + 1..].iter().position(|&b| b == b'"')? + at + 1;
                        let key = &input[at + 1..end];
                        let index = plan[..size]
                            .iter()
                            .position(|p| p.is_some_and(|p| p.name == key))?;
                        order[fields] = index;
                        index
                    };
                    let property = plan[index]?;
                    let name_end = at.checked_add(1 + property.name.len())?;
                    let matches = if property.name.len() == 1 {
                        input.get(at..at + 4)? == [b'"', property.name[0], b'"', b':']
                    } else {
                        input.get(at + 1..name_end)? == property.name
                            && input.get(name_end..name_end + 2)? == b"\":"
                    };
                    if !matches {
                        return None;
                    }
                    let bit = 1u64 << index;
                    // Repeated uncaptured fields are legal only under the request's
                    // explicit permissive policy; the exact engine handles them.
                    if seen & bit != 0 {
                        return None;
                    }
                    seen |= bit;
                    at = name_end + 2;
                    let (end, scalar_work) =
                        fixed_property_end(input, at, property, self.numeric, limits)?;
                    work += 7
                        + property.name.len() as u64
                        + (property.name.len() as u64 + 8) * passes
                        + scalar_work;
                    at = end;
                    fields += 1;
                    match input.get(at) {
                        Some(b',') => {
                            at += 1;
                            if input.get(at) == Some(&b'}') {
                                return None;
                            }
                        }
                        Some(b'}') => break,
                        _ => return None,
                    }
                }
                if seen & required != required || input.get(at) != Some(&b'}') {
                    return None;
                }
                work += if fields == 0 { 7 } else { 6 };
                if count == 0 {
                    order_len = fields;
                }
                at += 1;
                count += 1;
                match input.get(at) {
                    Some(b',') => at += 1,
                    Some(b']') => break,
                    _ => return None,
                }
            }
        }
        constraints::check_count(&outer.constraints, count, at as u32).ok()?;
        let len = u32::try_from(at + 1 - start as usize).ok()?;
        // All charges are nonnegative. If the total does not fit, replay from
        // untouched state to report the exact first budget failure and field.
        if work > self.context.budget.steps_left() {
            return None;
        }
        self.context.budget.charge(work, start).ok()?;
        self.cursor.advance(len);
        Some(Span32::new(start, len))
    }

    /// A minified object of scalar properties, captured or not. Every value
    /// is checked and converted privately: captured slots are written as they
    /// complete and cleared again on any miss, and the budget is charged only
    /// once the closing brace is seen, so each miss replays in the exact VM.
    /// The charge mirrors the exact engine's per-token work.
    fn fast_fixed_object(&mut self, pending: PendingValue, start: u32) -> Option<u32> {
        #[cfg(test)]
        if tests::force_exact() {
            return None;
        }
        if pending
            .slot
            .is_some_and(|s| self.context.slots.get(s as usize) != Some(&SlotValue::Empty))
        {
            return None;
        }
        let program = self.context.program;
        let StateKind::Object {
            key_table,
            reject_unknown: true,
            ..
        } = program.state(pending.state?)?.kind
        else {
            return None;
        };
        let plan = program.fixed_object(key_table)?;
        let table = program.program().key_tables.get(key_table as usize)?;
        let limits = self.context.limits.syntax;
        let depth = self.context.frames.len().checked_add(1)?;
        if plan.max_name_len > limits.max_string_bytes
            || depth > usize::from(limits.max_depth)
            || depth > self.context.frames.capacity()
        {
            return None;
        }
        let entries = plan.entries.as_slice();
        let size = entries.len();
        let key_bytes = program.image().key_bytes.as_slice();
        let passes = match table.strategy {
            jmax_schema_compile::KeyStrategy::Swar => u64::from(table.key_count) + 1,
            jmax_schema_compile::KeyStrategy::PerfectHash { .. } => 2,
            jmax_schema_compile::KeyStrategy::BucketedPerfectHash { .. } => 3,
        };
        let input = self.cursor.input();
        if u32::try_from(input.len()).is_err() {
            return None;
        }
        let numeric = self.numeric;
        let mut written = [0u32; 64];
        let mut written_len = 0usize;
        let slots = &mut self.context.slots;
        let selection = &self.context.selection;
        let snapshot = self.context.budget.clone();
        let steps_left = snapshot.steps_left();
        let attempt = (|| {
            let mut at = start as usize + 1;
            let mut seen = 0u64;
            let mut fields = 0usize;
            let mut work = 3u64;
            while input.get(at) != Some(&b'}') {
                if fields >= size
                    || fields >= limits.max_fields_per_object as usize
                    || input.get(at) != Some(&b'"')
                {
                    return None;
                }
                // A key's first byte usually names its entry outright.
                let word = input
                    .get(at..)
                    .and_then(|rest| rest.first_chunk::<8>())
                    .map(|chunk| u64::from_le_bytes(*chunk));
                let mut index = usize::from(plan.by_first_byte[usize::from(*input.get(at + 1)?)]);
                let mut name_len = 0usize;
                let mut hit = false;
                if let Some(guessed) = entries.get(index) {
                    name_len = guessed.name.len as usize;
                    hit = match word {
                        Some(word) if guessed.pattern_mask != 0 => {
                            (word ^ guessed.pattern) & guessed.pattern_mask == 0
                        }
                        _ => {
                            let name = guessed.name.slice(key_bytes)?;
                            input.get(at + 1..at + 1 + name_len)? == name
                                && input.get(at + 1 + name_len..at + 3 + name_len)? == b"\":"
                        }
                    };
                }
                if !hit {
                    let end = input[at + 1..].iter().position(|&b| b == b'"')? + at + 1;
                    let key = &input[at + 1..end];
                    index = entries
                        .iter()
                        .position(|e| e.name.slice(key_bytes) == Some(key))?;
                    name_len = key.len();
                    if input.get(end + 1) != Some(&b':') {
                        return None;
                    }
                }
                let name_end = at + 1 + name_len;
                let bit = 1u64 << index;
                // Repeated fields are the exact engine's to accept or reject.
                if seen & bit != 0 {
                    return None;
                }
                seen |= bit;
                let entry = &entries[index];
                let state = if entry.kind == ValueKind::Decimal
                    || (entry.kind == ValueKind::Integer && !entry.small_integer)
                {
                    Some(program.state(entry.state)?)
                } else {
                    None
                };
                let property = FixedProperty {
                    name: &[],
                    state,
                    kind: entry.kind,
                    small_integer: entry.small_integer,
                };
                at = name_end + 2;
                let (end, scalar_work) = fixed_property_end(input, at, property, numeric, limits)?;
                if let Some(slot) = entry.slot.filter(|&s| extract::selected(selection, s)) {
                    let value = match entry.kind {
                        ValueKind::Decimal => {
                            // SAFETY: fixed_decimal_end accepted only ASCII digits, a sign and a point.
                            let text =
                                unsafe { core::str::from_utf8_unchecked(&input[at + 1..end - 1]) };
                            let mut scratch = snapshot.clone();
                            SlotValue::Decimal(
                                constraints::decimal(
                                    text,
                                    state?.constraints.decimal.as_ref()?,
                                    &program.program().policy.numeric,
                                    &mut scratch,
                                    at as u32,
                                )
                                .ok()?,
                            )
                        }
                        ValueKind::String => SlotValue::Str(StrRef {
                            span: Span32::new(at as u32 + 1, (end - at - 2) as u32),
                            source: SpanSource::RawPayload,
                            scalar_len: (end - at - 2) as u32,
                        }),
                        ValueKind::Integer => {
                            let negative = input[at] == b'-';
                            constraints::integer(&input[at..end], negative, at as u32).ok()?
                        }
                        ValueKind::Boolean => SlotValue::Bool(input[at] == b't'),
                        _ => SlotValue::RawSpan(Span32::new(at as u32, 4)),
                    };
                    let target = slots.get_mut(slot as usize)?;
                    if *target != SlotValue::Empty {
                        return None;
                    }
                    *target = value;
                    written[written_len] = slot;
                    written_len += 1;
                }
                work += 7 + name_len as u64 + (name_len as u64 + 8) * passes + scalar_work;
                at = end;
                fields += 1;
                match input.get(at) {
                    Some(b',') => {
                        at += 1;
                        if input.get(at) == Some(&b'}') {
                            return None;
                        }
                    }
                    Some(b'}') => break,
                    _ => return None,
                }
            }
            if seen & plan.required != plan.required || input.get(at) != Some(&b'}') {
                return None;
            }
            if fields == 0 {
                work += 1;
            }
            // The total is charged all at once, and only when it fits.
            (work <= steps_left).then_some((u32::try_from(at + 1 - start as usize).ok()?, work))
        })();
        match attempt {
            Some((len, work)) => {
                self.context.budget.charge(work, start).ok()?;
                self.cursor.advance(len);
                Some(len)
            }
            None => {
                for &slot in &written[..written_len] {
                    self.context.slots[slot as usize] = SlotValue::Empty;
                }
                None
            }
        }
    }

    /// Fast path for arrays of schema objects containing scalar properties.
    /// All state, cursor, decoded bytes, and work accounting are private until
    /// the whole array succeeds, so unsupported values return to the VM.
    fn fast_struct_array(&mut self, pending: PendingValue, start: u32) -> Option<Span32> {
        if pending.slot.is_some_and(|slot| {
            usize::try_from(slot)
                .ok()
                .and_then(|index| self.context.slots.get(index))
                != Some(&SlotValue::Empty)
        }) {
            return None;
        }
        let outer = self.context.program.state(pending.state?)?;
        let item_id = match &outer.kind {
            StateKind::Array { items } => *items,
            _ => return None,
        };
        let item_state = self.context.program.state(item_id)?;
        if item_state.constraints != ConstraintSet::default() {
            return None;
        }
        let schema_reject_unknown = match &item_state.kind {
            StateKind::Object {
                key_table,
                reject_unknown,
                ..
            } => {
                if self
                    .context
                    .program
                    .program()
                    .key_tables
                    .get(*key_table as usize)?
                    .key_count
                    > 64
                {
                    return None;
                }
                *reject_unknown
            }
            _ => return None,
        };
        if !schema_reject_unknown {
            return None;
        }
        let base_depth = self.context.frames.len();
        if base_depth.checked_add(2)? > usize::from(self.context.limits.syntax.max_depth)
            || self.context.frames.capacity() < base_depth + 2
        {
            return None;
        }
        let mut cursor = BlockCursor::new(
            self.cursor.input(),
            self.cursor.limits(),
            Some(self.numeric),
            DuplicatePolicyOwner::SchemaVm,
        )
        .ok()?;
        cursor.advance(start);
        let mut budget = self.context.budget.clone();
        let mut decoded = Vec::new();
        let mut decoded_capacity = 0usize;
        let attempt = (|| {
            cursor.expect_byte(b'[', "expected array").ok()?;
            let mut count = 0u32;
            let mut next = cursor.skip_whitespace_uncached().ok()??;
            if next == b']' {
                budget.charge(1, start).ok()?;
            }
            while next != b']' {
                if count >= self.context.limits.syntax.max_array_items {
                    return None;
                }
                if base_depth.checked_add(2)? > usize::from(self.context.limits.syntax.max_depth) {
                    return None;
                }
                let object_start = cursor.offset();
                cursor.expect_byte(b'{', "expected object").ok()?;
                let mut object = objects::begin(
                    self.context.program,
                    Some(item_id),
                    &mut budget,
                    object_start,
                )
                .ok()?;
                budget.charge(2, object_start).ok()?;
                let mut key = cursor.skip_whitespace_uncached().ok()??;
                while key != b'}' {
                    if object.property_count >= self.context.limits.syntax.max_fields_per_object {
                        return None;
                    }
                    if key != b'"' {
                        return None;
                    }
                    let meta = cursor
                        .scan_key_meta_uncached(
                            &mut decoded,
                            self.context.limits.syntax.reject_duplicate_keys,
                        )
                        .ok()?;
                    let growth = decoded.capacity().checked_sub(decoded_capacity)?;
                    budget
                        .retain(u32::try_from(growth).ok()?, cursor.offset())
                        .ok()?;
                    decoded_capacity = decoded.capacity();
                    let value = objects::key_meta(
                        self.context.program,
                        &mut object,
                        meta,
                        cursor.input(),
                        &decoded,
                        &self.context.selection,
                        &self.context.limits,
                        &mut budget,
                    )
                    .ok()?;
                    if value.slot.is_some() {
                        return None;
                    }
                    cursor.skip_whitespace_uncached().ok()?;
                    cursor
                        .expect_byte(b':', "expected colon after object key")
                        .ok()?;
                    let byte = cursor.skip_whitespace_uncached().ok()??;
                    if matches!(byte, b'{' | b'[') {
                        return None;
                    }
                    budget.charge(4, cursor.offset()).ok()?;
                    let (kind, span, reference) = match byte {
                        b'"' => {
                            let meta = cursor.scan_string_meta_uncached(&mut decoded).ok()?;
                            let growth = decoded.capacity().checked_sub(decoded_capacity)?;
                            budget
                                .retain(u32::try_from(growth).ok()?, cursor.offset())
                                .ok()?;
                            decoded_capacity = decoded.capacity();
                            (ScalarKind::String, meta.raw_token, Some(meta.cooked))
                        }
                        b'-' | b'0'..=b'9' => (
                            ScalarKind::Number,
                            cursor.scan_number_span_uncached().ok()?,
                            None,
                        ),
                        b't' | b'f' | b'n' => {
                            let (kind, span) = cursor.scan_literal().ok()?;
                            let kind = match kind {
                                TokenKind::True => ScalarKind::True,
                                TokenKind::False => ScalarKind::False,
                                TokenKind::Null => ScalarKind::Null,
                                _ => return None,
                            };
                            (kind, span, None)
                        }
                        _ => return None,
                    };
                    let fixed_decimal = if kind == ScalarKind::String {
                        value
                            .state
                            .and_then(|id| self.context.program.state(id))
                            .filter(|state| state.kind == StateKind::Scalar(ValueKind::Decimal))
                            .and_then(|state| state.constraints.decimal.as_ref())
                            .and_then(|decimal| {
                                let (end, _) = fixed_decimal_end(
                                    cursor.input(),
                                    span.offset as usize,
                                    decimal,
                                    self.numeric,
                                    self.context.limits.syntax.max_string_bytes,
                                    self.context.limits.syntax.max_number_digits,
                                )?;
                                (end == span.offset as usize + span.len as usize).then_some(())
                            })
                    } else {
                        None
                    };
                    if kind == ScalarKind::String
                        && value.state.is_some_and(|id| {
                            self.context.program.state(id).is_some_and(|state| {
                                matches!(
                                    state.kind,
                                    StateKind::Scalar(ValueKind::Integer | ValueKind::Decimal)
                                )
                            })
                        })
                        && fixed_decimal.is_none()
                    {
                        let reference = reference?;
                        let owner = match reference.source {
                            SpanSource::RawPayload => cursor.input(),
                            SpanSource::DecodedArena => decoded.as_slice(),
                        };
                        let bytes = reference.span.slice(owner)?;
                        budget.charge(bytes.len() as u64, span.offset).ok()?;
                        let digits = bytes.iter().filter(|byte| byte.is_ascii_digit()).count();
                        if digits > usize::from(self.context.limits.syntax.max_number_digits) {
                            return None;
                        }
                    }
                    if kind == ScalarKind::String
                        && !self.numeric.allow_Quoted_int
                        && value.state.is_some_and(|id| {
                            self.context.program.state(id).is_some_and(|state| {
                                state.kind == StateKind::Scalar(ValueKind::Integer)
                            })
                        })
                    {
                        return None;
                    }
                    if fixed_decimal.is_some() {
                        let reference = reference?;
                        let owner = match reference.source {
                            SpanSource::RawPayload => cursor.input(),
                            SpanSource::DecodedArena => decoded.as_slice(),
                        };
                        let bytes = reference.span.slice(owner)?;
                        // Match number_grammar's and the string digit check's
                        // charges while avoiding a second decimal parse.
                        budget.charge(bytes.len() as u64, span.offset).ok()?;
                        let lexical = (bytes.len() as u64).checked_mul(4)?.checked_add(2_049)?;
                        budget.charge(lexical, span.offset).ok()?;
                    } else {
                        constraints::check_scalar_trusted(
                            self.context.program,
                            value.state,
                            ScalarInput {
                                kind,
                                span,
                                decoded: reference,
                                raw: cursor.input(),
                                arena: &decoded,
                            },
                            &mut budget,
                        )
                        .ok()?;
                    }
                    objects::take_value(&mut object, span.offset).ok()?;
                    cursor.skip_whitespace_uncached().ok()?;
                    let separator = cursor.peek_byte()?;
                    if separator == b'}' {
                        break;
                    }
                    if separator != b',' {
                        return None;
                    }
                    cursor.advance(1);
                    key = cursor.skip_whitespace_uncached().ok()??;
                    if key == b'}' {
                        return None;
                    }
                }
                let object_end = cursor.offset();
                budget
                    .charge(if object.property_count == 0 { 2 } else { 1 }, object_end)
                    .ok()?;
                objects::finish(
                    self.context.program,
                    Some(item_id),
                    &object,
                    &mut budget,
                    object_end,
                )
                .ok()?;
                cursor.advance(1);
                count = count.checked_add(1)?;
                next = cursor.skip_whitespace_uncached().ok()??;
                if next == b']' {
                    break;
                }
                if next != b',' {
                    return None;
                }
                cursor.advance(1);
                next = cursor.skip_whitespace_uncached().ok()??;
                if next == b']' {
                    return None;
                }
            }
            let end = cursor.offset();
            constraints::check_count(&outer.constraints, count, end).ok()?;
            cursor.advance(1);
            let len = cursor.offset().checked_sub(start)?;
            Some(Span32::new(start, len))
        })();
        let result = attempt?;
        self.cursor = cursor;
        self.context.budget = budget;
        Some(result)
    }

    /// Validate a price-level shaped `[["decimal", "decimal"], ..]` value
    /// without creating the array frames or running dynamic state dispatch.
    /// The independent cursor and budget make every unsuccessful attempt a
    /// transaction: its caller continues through the ordinary VM path.
    fn fast_decimal_pairs(&mut self, pending: PendingValue, start: u32) -> Option<Span32> {
        if pending.slot.is_some_and(|slot| {
            usize::try_from(slot)
                .ok()
                .and_then(|index| self.context.slots.get(index))
                != Some(&SlotValue::Empty)
        }) {
            return None;
        }
        let outer = pending
            .state
            .and_then(|id| self.context.program.state(id))?;
        let pair_id = match &outer.kind {
            StateKind::Array { items } => *items,
            _ => return None,
        };
        let pair = self.context.program.state(pair_id)?;
        let decimal_id = match &pair.kind {
            StateKind::Array { items } => *items,
            _ => return None,
        };
        let decimal_state = self.context.program.state(decimal_id)?;
        let decimal = match &decimal_state.kind {
            StateKind::Scalar(ValueKind::Decimal) => decimal_state.constraints.decimal.as_ref()?,
            _ => return None,
        };
        if decimal.minimum.is_some() || decimal.maximum.is_some() {
            return None;
        }
        jmax_decimal::validate_policy(&decimal.policy).ok()?;
        let depth = self.context.frames.len();
        if depth.checked_add(2)? > usize::from(self.context.limits.syntax.max_depth) {
            return None;
        }
        // Warm frames must already cover both array levels, so this shortcut
        // has exactly the reference engine's retained-capacity accounting.
        if self.context.frames.capacity() < depth + 2 {
            return None;
        }
        let mut tight_budget = self.context.budget.clone();
        if let Some(span) = fixed_pair_array(
            self.cursor.input(),
            start,
            decimal,
            &pair.constraints,
            &outer.constraints,
            self.numeric,
            self.context.limits.syntax,
            &mut tight_budget,
        ) {
            self.cursor.advance(span.len);
            self.context.budget = tight_budget;
            return Some(span);
        }
        let mut cursor = BlockCursor::new(
            self.cursor.input(),
            self.cursor.limits(),
            Some(self.numeric),
            DuplicatePolicyOwner::SchemaVm,
        )
        .ok()?;
        cursor.advance(start);
        let mut budget = self.context.budget.clone();

        let attempt = (|| {
            cursor.expect_byte(b'[', "expected array").ok()?;
            let mut count = 0u32;
            let mut next = cursor.skip_whitespace_uncached().ok()??;
            if next != b']' {
                loop {
                    if count >= self.context.limits.syntax.max_array_items {
                        return None;
                    }
                    cursor.expect_byte(b'[', "expected pair array").ok()?;
                    let mut item_count = 0u32;
                    loop {
                        if item_count >= self.context.limits.syntax.max_array_items {
                            return None;
                        }
                        fast_decimal_string(
                            &mut cursor,
                            decimal,
                            self.numeric,
                            self.context.limits.syntax.max_string_bytes,
                            self.context.limits.syntax.max_number_digits,
                            &mut budget,
                        )
                        .ok()?;
                        item_count += 1;
                        next = cursor.skip_whitespace_uncached().ok()??;
                        if next == b']' {
                            break;
                        }
                        if next != b',' || item_count != 1 {
                            return None;
                        }
                        cursor.advance(1);
                        cursor.skip_whitespace_uncached().ok()?;
                    }
                    let pair_end = cursor.offset();
                    cursor.advance(1);
                    if constraints::check_count(&pair.constraints, item_count, pair_end).is_err() {
                        return None;
                    }
                    if item_count != 2 {
                        return None;
                    }
                    count += 1;
                    next = cursor.skip_whitespace_uncached().ok()??;
                    if next == b']' {
                        break;
                    }
                    if next != b',' {
                        return None;
                    }
                    cursor.advance(1);
                    cursor.skip_whitespace_uncached().ok()?;
                }
            }
            let outer_end = cursor.offset();
            if constraints::check_count(&outer.constraints, count, outer_end).is_err() {
                return None;
            }
            cursor.advance(1);
            let len = cursor.offset().checked_sub(start)?;
            // Each pair has two entry charges, one comma and two closing/
            // separator iterations. Scalar charges are paid in the helper.
            budget
                .charge(
                    if count == 0 {
                        1
                    } else {
                        u64::from(count).checked_mul(5)?
                    },
                    start,
                )
                .ok()?;
            Some(Span32::new(start, len))
        })();
        let result = attempt?;
        self.cursor = cursor;
        self.context.budget = budget;
        Some(result)
    }

    fn object_key(&mut self) -> Result<()> {
        if self.cursor.peek_byte() != Some(b'"') {
            return Err(invalid(self.cursor.offset(), "expected object key"));
        }
        // The FNV hash feeds only the unknown-key table, which a table that
        // rejects unknown keys never fills.
        let hash = self.context.limits.syntax.reject_duplicate_keys
            && !self.context.limits.syntax.reject_unknown_keys
            && !matches!(
                self.context.frames.last(),
                Some(VmFrame {
                    kind: FrameKind::Object(ObjectState {
                        rejects_unknown: true,
                        ..
                    }),
                    ..
                })
            );
        let meta = self.cursor.scan_key_meta_uncached(self.decoded, hash)?;
        self.charge_decoded()?;
        let frame = self
            .context
            .frames
            .last_mut()
            .ok_or_else(|| invalid(self.cursor.offset(), "key outside object"))?;
        let object = match &mut frame.kind {
            FrameKind::Object(object) => object,
            _ => return Err(invalid(self.cursor.offset(), "key inside array")),
        };
        objects::key_meta(
            self.context.program,
            object,
            meta,
            self.cursor.input(),
            self.decoded,
            &self.context.selection,
            &self.context.limits,
            &mut self.context.budget,
        )?;
        frame.phase = FramePhase::ObjectValue;
        self.cursor.skip_whitespace_uncached()?;
        self.cursor
            .expect_byte(b':', "expected colon after object key")?;
        Ok(())
    }

    fn close(&mut self, kind: ContainerKind) -> Result<()> {
        let offset = self.cursor.offset();
        execute::close(self.context, kind, self.cursor.input(), offset)?;
        self.cursor.advance(1);
        Ok(())
    }

    fn run_value(&mut self) -> Result<()> {
        self.value()?;
        while !self.context.root_done {
            let byte = self.cursor.skip_whitespace_uncached()?.ok_or_else(|| {
                vm_error(
                    ErrorCode::UnexpectedEof,
                    self.cursor.offset(),
                    "unclosed container",
                )
            })?;
            self.context.budget.charge(1, self.cursor.offset())?;
            let frame = self
                .context
                .frames
                .last_mut()
                .ok_or_else(|| invalid(self.cursor.offset(), "missing container frame"))?;
            match frame.phase {
                FramePhase::ObjectStart if byte == b'}' => self.close(ContainerKind::Object)?,
                FramePhase::ObjectStart | FramePhase::ObjectKeyOrEnd => self.object_key()?,
                FramePhase::ObjectValue => {
                    frame.phase = FramePhase::ObjectCommaOrEnd;
                    self.value()?;
                }
                FramePhase::ObjectCommaOrEnd => match byte {
                    b'}' => self.close(ContainerKind::Object)?,
                    b',' => {
                        frame.phase = FramePhase::ObjectKeyOrEnd;
                        self.cursor.advance(1);
                    }
                    _ => {
                        return Err(invalid(
                            self.cursor.offset(),
                            "expected object comma or close",
                        ));
                    }
                },
                FramePhase::ArrayStart if byte == b']' => self.close(ContainerKind::Array)?,
                FramePhase::ArrayStart | FramePhase::ArrayValueOrEnd => {
                    frame.phase = FramePhase::ArrayCommaOrEnd;
                    self.value()?;
                }
                FramePhase::ArrayCommaOrEnd => match byte {
                    b']' => self.close(ContainerKind::Array)?,
                    b',' => {
                        frame.phase = FramePhase::ArrayValueOrEnd;
                        self.cursor.advance(1);
                    }
                    _ => {
                        return Err(invalid(
                            self.cursor.offset(),
                            "expected array comma or close",
                        ));
                    }
                },
                _ => {
                    return Err(invalid(
                        self.cursor.offset(),
                        "invalid direct grammar phase",
                    ));
                }
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        self.run_value()?;
        self.cursor.finish_eof_uncached()
    }
}

#[derive(Clone, Copy)]
struct FixedProperty<'a> {
    name: &'a [u8],
    /// Needed only by decimal and bounded integer properties.
    state: Option<&'a jmax_schema_compile::ProgramState>,
    kind: ValueKind,
    small_integer: bool,
}

#[inline(always)]
fn fixed_property_end(
    input: &[u8],
    start: usize,
    property: FixedProperty<'_>,
    numeric: NumericPolicy,
    limits: Limits,
) -> Option<(usize, u64)> {
    match property.kind {
        ValueKind::Decimal => {
            let (end, _) = fixed_decimal_end(
                input,
                start,
                property.state?.constraints.decimal.as_ref()?,
                numeric,
                limits.max_string_bytes,
                limits.max_number_digits,
            )?;
            Some((end, (end - start - 2) as u64 * 5 + 2048))
        }
        ValueKind::String => {
            if input.get(start) != Some(&b'"') {
                return None;
            }
            let mut at = start + 1;
            loop {
                match *input.get(at)? {
                    b'"' => break,
                    b'\\' | 0..=0x1f | 0x80..=0xff => return None,
                    _ => at += 1,
                }
            }
            (at - start - 1 <= limits.max_string_bytes as usize).then_some((at + 1, 0))
        }
        ValueKind::Integer => {
            let negative = input.get(start) == Some(&b'-');
            let first = start + usize::from(negative);
            let at = decimal_digits_end(input, first);
            if at == first
                || at - first > usize::from(limits.max_number_digits)
                || (at - first > 1 && input[first] == b'0')
                || (negative && input[first] == b'0' && numeric.reject_negative_zero)
            {
                return None;
            }
            if property.small_integer && at - first < 19 {
                return Some((at, (at - start) as u64 * 4));
            }
            let mut value = 0u64;
            for &b in &input[first..at] {
                value = value.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
            }
            if negative && value > (1u64 << 63) {
                return None;
            }
            let signed = if negative {
                -(i128::from(value))
            } else {
                i128::from(value)
            };
            if property
                .state?
                .constraints
                .min_i64
                .is_some_and(|v| signed < i128::from(v))
                || property
                    .state?
                    .constraints
                    .max_i64
                    .is_some_and(|v| signed > i128::from(v))
            {
                return None;
            }
            Some((at, (at - start) as u64 * 4))
        }
        ValueKind::Boolean => {
            let len = if input.get(start..start + 4) == Some(b"true") {
                4
            } else if input.get(start..start + 5) == Some(b"false") {
                5
            } else {
                return None;
            };
            Some((start + len, 0))
        }
        ValueKind::Null => (input.get(start..start + 4) == Some(b"null")).then_some((start + 4, 0)),
        _ => None,
    }
}

/// Minified fixed decimal pairs avoid repeated cursor dispatch. Any whitespace,
/// exceptional decimal, limit failure or shape mismatch takes the exact path.
#[inline(always)]
fn fixed_pair_array(
    raw: &[u8],
    start: u32,
    decimal: &jmax_schema_ir::DecimalConstraints,
    pair: &ConstraintSet,
    outer: &ConstraintSet,
    numeric: NumericPolicy,
    limits: Limits,
    budget: &mut VmBudget,
) -> Option<Span32> {
    let mut at = start as usize;
    if raw.get(at) != Some(&b'[') {
        return None;
    }
    at += 1;
    let mut count = 0u32;
    let mut scalar_work = 0u64;
    if raw.get(at) != Some(&b']') {
        if limits.max_array_items < 2 {
            return None;
        }
        constraints::check_count(pair, 2, start).ok()?;
        loop {
            if count >= limits.max_array_items || raw.get(at) != Some(&b'[') {
                return None;
            }
            at += 1;
            let (end, work) = fixed_decimal_end(
                raw,
                at,
                decimal,
                numeric,
                limits.max_string_bytes,
                limits.max_number_digits,
            )?;
            scalar_work += work;
            at = end;
            if raw.get(at) != Some(&b',') {
                return None;
            }
            at += 1;
            let (end, work) = fixed_decimal_end(
                raw,
                at,
                decimal,
                numeric,
                limits.max_string_bytes,
                limits.max_number_digits,
            )?;
            scalar_work += work;
            at = end;
            if raw.get(at) != Some(&b']') {
                return None;
            }
            at += 1;
            count += 1;
            if raw.get(at) == Some(&b']') {
                break;
            }
            if raw.get(at) != Some(&b',') {
                return None;
            }
            at += 1;
        }
    }
    constraints::check_count(outer, count, at as u32).ok()?;
    budget
        .charge(
            scalar_work + if count == 0 { 1 } else { u64::from(count) * 5 },
            start,
        )
        .ok()?;
    at += 1;
    Some(Span32::new(start, u32::try_from(at - start as usize).ok()?))
}

/// A unit-tick decimal whose precision including scale padding fits i128.
/// Exact zero-padding needs no coefficient construction in validation-only
/// execution. Other rescalings retain the general decimal implementation.
#[inline(always)]
fn fixed_decimal_end(
    raw: &[u8],
    start: usize,
    constraints: &jmax_schema_ir::DecimalConstraints,
    numeric: NumericPolicy,
    max_string_bytes: u32,
    max_digits: u16,
) -> Option<(usize, u64)> {
    let policy = &constraints.policy;
    if policy.tick_step != 1
        || constraints.minimum.is_some()
        || constraints.maximum.is_some()
        || raw.get(start) != Some(&b'"')
    {
        return None;
    }
    let content = start + 1;
    let negative = raw.get(content) == Some(&b'-');
    let integer = content + usize::from(negative);
    let dot = decimal_digits_end(raw, integer);
    if dot == integer || (dot - integer > 1 && raw[integer] == b'0') {
        return None;
    }
    let (end, fractional_digits) = if raw.get(dot) == Some(&b'.') {
        let fraction = dot + 1;
        let end = decimal_digits_end(raw, fraction);
        if end == fraction {
            return None;
        }
        (end, end - fraction)
    } else {
        (dot, 0)
    };
    let padding = usize::from(policy.target_scale).checked_sub(fractional_digits)?;
    let digits = dot - integer + fractional_digits;
    if raw.get(end) != Some(&b'"')
        || digits.checked_add(padding)? > usize::from(policy.max_precision).min(38)
        || digits > usize::from(max_digits)
        || end - content > max_string_bytes as usize
    {
        return None;
    }
    if negative
        && numeric.reject_negative_zero
        && raw[integer..end].iter().all(|&b| b == b'0' || b == b'.')
    {
        return None;
    }
    Some((end + 1, (end - content) as u64 * 5 + 2051))
}

#[inline(always)]
fn decimal_digits_end(raw: &[u8], mut at: usize) -> usize {
    const LOW: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    while raw.len() - at >= 8 {
        let word = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap_or_default());
        let mask = (word.wrapping_sub(LOW * 0x30) | word.wrapping_add(LOW * 0x46)) & HIGH;
        if mask != 0 {
            return at + (mask.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    while raw.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    at
}

fn fast_decimal_string(
    cursor: &mut BlockCursor<'_>,
    constraints: &jmax_schema_ir::DecimalConstraints,
    numeric: NumericPolicy,
    max_string_bytes: u32,
    max_digits: u16,
    budget: &mut VmBudget,
) -> Result<()> {
    if let Some((end, work)) = fixed_decimal_end(
        cursor.input(),
        cursor.offset() as usize,
        constraints,
        numeric,
        max_string_bytes,
        max_digits,
    ) {
        budget.charge(work, cursor.offset())?;
        cursor.advance((end - cursor.offset() as usize) as u32);
        return Ok(());
    }
    let offset = cursor.offset();
    let raw = cursor.input();
    let mut at = offset as usize;
    let fail = || invalid(offset, "decimal is outside fast lexical subset");
    if raw.get(at) != Some(&b'"') {
        return Err(fail());
    }
    at += 1;
    let start = at;
    let negative = raw.get(at) == Some(&b'-');
    at += usize::from(negative);
    let integer_start = at;
    let mut coefficient = 0u64;
    let mut digits = 0usize;
    while let Some(&byte @ b'0'..=b'9') = raw.get(at) {
        if digits == 18 {
            return Err(fail());
        }
        coefficient = coefficient * 10 + u64::from(byte - b'0');
        digits += 1;
        at += 1;
    }
    if at == integer_start
        || (at - integer_start > 1 && raw[integer_start] == b'0')
        || raw.get(at) != Some(&b'.')
    {
        return Err(fail());
    }
    at += 1;
    let fraction_start = at;
    while let Some(&byte @ b'0'..=b'9') = raw.get(at) {
        if digits == 18 {
            return Err(fail());
        }
        coefficient = coefficient * 10 + u64::from(byte - b'0');
        digits += 1;
        at += 1;
    }
    if at == fraction_start
        || raw.get(at) != Some(&b'"')
        || at - start > max_string_bytes as usize
        || digits > usize::from(max_digits)
        || (negative && coefficient == 0 && numeric.reject_negative_zero)
    {
        return Err(fail());
    }
    let fractional_digits = at - fraction_start;
    // This subset has at most eighteen digits, so unsigned accumulation cannot
    // overflow. Wider coefficients retain the established checked i128 path.
    let coefficient = if negative {
        -i128::from(coefficient)
    } else {
        i128::from(coefficient)
    };
    let work = ((at - start) as u64)
        .checked_mul(5)
        .and_then(|n| n.checked_add(2051))
        .ok_or_else(|| resource(offset))?;
    budget.charge(work, offset)?;
    if fractional_digits != usize::from(constraints.policy.target_scale)
        || digits > usize::from(constraints.policy.max_precision)
        || constraints.policy.tick_step != 1
    {
        let value = jmax_decimal::bounds::rescale(
            jmax_decimal::AccumulatedDecimal {
                coefficient,
                fractional_digits: fractional_digits as u32,
                exponent: 0,
                precision: jmax_decimal::coefficient_precision(coefficient),
            },
            &constraints.policy,
            offset,
        )?;
        jmax_decimal::bounds::validate(
            value,
            &constraints.policy,
            jmax_decimal::DecimalBounds::default(),
            offset,
        )?;
    }
    cursor.advance((at + 1 - offset as usize) as u32);
    Ok(())
}

fn execute_direct(
    program: &ValidatedProgram,
    raw: &[u8],
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &mut ValidationScratch,
    capture: bool,
    query: bool,
    validated: Option<ValidationCapability>,
) -> Result<()> {
    prepare_output(scratch);
    let numeric = combined_numeric(program, numeric)?;
    let owner = if limits.syntax.reject_duplicate_keys {
        DuplicatePolicyOwner::SchemaVm
    } else {
        DuplicatePolicyOwner::Disabled
    };
    // A live matching schema guard already proves numeric policy for every
    // byte. Narrow it privately to syntax scope for BlockCursor's owner skip.
    let cursor_numeric = if validated.is_some() {
        None
    } else {
        Some(numeric)
    };
    let validated = validated.map(|_| {
        // SAFETY: only extract_validated_borrowed can pass this private argument;
        // it retains the original borrow and verifies schema and every policy.
        unsafe {
            ValidationCapability::issue_unchecked(
                raw.as_ptr() as usize,
                raw.as_ptr() as usize,
                raw.len() as u32,
                0,
                0,
                limits.syntax.policy_fingerprint(),
            )
        }
    });
    let cursor = BlockCursor::with_scratch(
        raw,
        limits.syntax,
        cursor_numeric,
        owner,
        &mut scratch.direct_skip,
    )?;
    let mut context =
        execute::new_context_capture(program, selection, limits, &mut scratch.vm, capture)?;
    let result = (|| {
        // Prepay input traversal, including strict-discard and query-jump work.
        context.budget.charge(raw.len() as u64, 0)?;
        context.budget.retain(
            u32::try_from(scratch.direct_decoded.capacity()).map_err(|_| resource(0))?,
            0,
        )?;
        context.budget.retain(
            u32::try_from(scratch.direct_needed.capacity()).map_err(|_| resource(0))?,
            0,
        )?;
        context.budget.retain(
            u32::try_from(
                scratch
                    .direct_queue
                    .capacity()
                    .checked_mul(core::mem::size_of::<StateId>())
                    .ok_or_else(|| resource(0))?,
            )
            .map_err(|_| resource(0))?,
            0,
        )?;
        if query {
            selected_states(
                &mut context,
                selection,
                &mut scratch.direct_needed,
                &mut scratch.direct_queue,
            )?;
        }
        let decoded_capacity = scratch.direct_decoded.capacity();
        let mut engine = Engine {
            cursor,
            context: &mut context,
            decoded: &mut scratch.direct_decoded,
            needed: &scratch.direct_needed,
            query,
            numeric,
            decoded_capacity,
            validated,
        };
        engine.run()?;
        Ok(())
    })();
    recover(context, &mut scratch.vm);
    if result.is_err() {
        scratch.vm.slots.clear();
        scratch.direct_decoded.clear();
    }
    result.map_err(|error: Error| {
        let field = error.field_id;
        error.with_schema(program.image().header.schema_id, field)
    })
}

fn strict_selection(selection: &Selection) -> Result<()> {
    if !selection.strict_validate_unselected {
        return Err(vm_error(
            ErrorCode::UnsupportedSchemaFeature,
            0,
            "strict extraction requires strict_validate_unselected; use query extraction",
        ));
    }
    Ok(())
}
fn no_selection() -> Selection {
    Selection {
        field_ids: Vec::new(),
        strict_validate_unselected: true,
    }
}

// Error-only preflight retains the root-array syntax/limit diagnostics and
// gives a malformed suffix priority over an earlier schema/resource failure.
fn strict_root_input(
    raw: &[u8],
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &mut ValidationScratch,
) -> Result<()> {
    let mut cursor = BlockCursor::with_scratch(
        raw,
        limits.syntax,
        Some(numeric),
        DuplicatePolicyOwner::Syntax,
        &mut scratch.direct_skip,
    )?;
    cursor.skip_whitespace_uncached()?;
    if limits.syntax.max_depth == 0 {
        return Err(Error::new(
            ErrorCode::DepthLimitExceeded,
            ErrorSource::Syntax,
            cursor.offset(),
            "container depth exceeds its limit",
        ));
    }
    cursor.expect_byte(b'[', "root value is not an array")?;
    let mut count = 0u32;
    if cursor.skip_whitespace_uncached()? == Some(b']') {
        cursor.advance(1);
    } else {
        loop {
            if count >= limits.syntax.max_array_items {
                return Err(Error::new(
                    ErrorCode::BudgetExhausted,
                    ErrorSource::Syntax,
                    cursor.offset(),
                    "array item count exceeds its limit",
                ));
            }
            cursor.skip_value(&mut scratch.direct_decoded, 1, SkipMode::StrictValidate)?;
            count += 1;
            let Some(next) = cursor.skip_whitespace_uncached()? else {
                return Err(Error::new(
                    ErrorCode::UnexpectedEof,
                    ErrorSource::Syntax,
                    cursor.offset(),
                    "unclosed root array",
                ));
            };
            match next {
                b']' => {
                    cursor.advance(1);
                    break;
                }
                b',' => {
                    cursor.advance(1);
                    cursor.skip_whitespace_uncached()?;
                }
                _ => {
                    return Err(Error::new(
                        ErrorCode::InvalidSyntax,
                        ErrorSource::Syntax,
                        cursor.offset(),
                        "expected array comma or close",
                    ));
                }
            }
        }
    }
    cursor.finish_eof_uncached()
}

/// Map a root array through the direct schema VM, publishing callbacks only
/// after the complete input has passed strict syntax validation.
pub fn map_root_array_borrowed<T>(
    program: &ValidatedProgram,
    raw: &[u8],
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &mut ValidationScratch,
    mut map: impl FnMut(&[u8], &[u8], &[SlotValue]) -> T,
) -> Result<Vec<T>> {
    prepare_output(scratch);
    if let Err(error) = strict_selection(selection) {
        scratch.vm.slots.clear();
        return Err(error);
    }
    let numeric = combined_numeric(program, numeric)?;
    let mut element_syntax = limits.syntax;
    element_syntax.max_depth = element_syntax.max_depth.saturating_sub(1);
    let mut cursor = BlockCursor::with_scratch(
        raw,
        element_syntax,
        Some(numeric),
        DuplicatePolicyOwner::SchemaVm,
        &mut scratch.direct_skip,
    )?;
    let mut element_limits = limits;
    element_limits.syntax = element_syntax;
    let context =
        execute::new_context_capture(program, selection, element_limits, &mut scratch.vm, true);
    let mut context = match context {
        Ok(context) => context,
        Err(primary) => {
            drop(cursor);
            scratch.direct_decoded.clear();
            if let Err(error) = strict_root_input(raw, limits, numeric, scratch) {
                scratch.direct_decoded.clear();
                return Err(error);
            }
            let error = primary;
            let field = error.field_id;
            return Err(error.with_schema(program.image().header.schema_id, field));
        }
    };
    let mut staged = core::mem::take(&mut scratch.ordered);
    staged.clear();
    let row_width = context.slots.len();
    let mut row_count = 0usize;
    let result = (|| {
        cursor.skip_whitespace_uncached()?;
        if limits.syntax.max_depth == 0 {
            return Err(Error::new(
                ErrorCode::DepthLimitExceeded,
                ErrorSource::Syntax,
                cursor.offset(),
                "container depth exceeds its limit",
            ));
        }
        cursor.expect_byte(b'[', "root value is not an array")?;
        let mut count = 0u32;
        if cursor.skip_whitespace_uncached()? != Some(b']') {
            loop {
                if count >= limits.syntax.max_array_items {
                    return Err(Error::new(
                        ErrorCode::BudgetExhausted,
                        ErrorSource::Syntax,
                        cursor.offset(),
                        "array item count exceeds its limit",
                    ));
                }
                let offset = cursor.offset();
                context.budget.retain(
                    u32::try_from(
                        staged
                            .capacity()
                            .checked_mul(core::mem::size_of::<SlotValue>())
                            .ok_or_else(|| resource(offset))?,
                    )
                    .map_err(|_| resource(offset))?,
                    offset,
                )?;
                context.budget.retain(
                    u32::try_from(scratch.direct_decoded.capacity())
                        .map_err(|_| resource(offset))?,
                    offset,
                )?;
                context.budget.retain(
                    u32::try_from(scratch.direct_needed.capacity())
                        .map_err(|_| resource(offset))?,
                    offset,
                )?;
                context.budget.retain(
                    u32::try_from(
                        scratch
                            .direct_queue
                            .capacity()
                            .checked_mul(core::mem::size_of::<StateId>())
                            .ok_or_else(|| resource(offset))?,
                    )
                    .map_err(|_| resource(offset))?,
                    offset,
                )?;
                let decoded_capacity = scratch.direct_decoded.capacity();
                let engine = Engine {
                    cursor,
                    context: &mut context,
                    decoded: &mut scratch.direct_decoded,
                    needed: &scratch.direct_needed,
                    query: false,
                    numeric,
                    decoded_capacity,
                    validated: None,
                };
                let (next_cursor, row_result) = {
                    let mut engine = engine;
                    let row_result = engine.run_value();
                    let Engine {
                        cursor,
                        context: _,
                        decoded: _,
                        needed: _,
                        ..
                    } = engine;
                    (cursor, row_result)
                };
                cursor = next_cursor;
                row_result?;
                context
                    .budget
                    .charge(u64::from(cursor.offset().saturating_sub(offset)), offset)?;
                count = count.checked_add(1).ok_or_else(|| resource(offset))?;
                let old_capacity = staged.capacity();
                staged
                    .try_reserve(row_width)
                    .map_err(|_| resource(offset))?;
                let growth = staged
                    .capacity()
                    .checked_sub(old_capacity)
                    .and_then(|n| n.checked_mul(core::mem::size_of::<SlotValue>()))
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| resource(offset))?;
                context.budget.retain(growth, offset)?;
                staged.extend_from_slice(&context.slots);
                row_count += 1;
                let Some(next) = cursor.skip_whitespace_uncached()? else {
                    return Err(Error::new(
                        ErrorCode::UnexpectedEof,
                        ErrorSource::Syntax,
                        cursor.offset(),
                        "unclosed root array",
                    ));
                };
                if next == b']' {
                    cursor.advance(1);
                    break;
                }
                if next != b',' {
                    return Err(Error::new(
                        ErrorCode::InvalidSyntax,
                        ErrorSource::Syntax,
                        cursor.offset(),
                        "expected array comma or close",
                    ));
                }
                cursor.advance(1);
                cursor.skip_whitespace_uncached()?;
                execute::reset_capture(&mut context)?;
            }
        } else {
            cursor.advance(1);
        }
        cursor.finish_eof_uncached()
    })();
    recover(context, &mut scratch.vm);
    if let Err(primary) = result {
        scratch.vm.slots.clear();
        scratch.direct_decoded.clear();
        staged.clear();
        scratch.ordered = staged;
        let strict = strict_root_input(raw, limits, numeric, scratch);
        scratch.direct_decoded.clear();
        if let Err(error) = strict {
            return Err(error);
        }
        let error = primary;
        let field = error.field_id;
        scratch.direct_decoded.clear();
        return Err(error.with_schema(program.image().header.schema_id, field));
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(row_count).map_err(|_| resource(0))?;
    for row in 0..row_count {
        let start = row * row_width;
        rows.push(map(
            raw,
            &scratch.direct_decoded,
            &staged[start..start + row_width],
        ));
    }
    scratch.direct_decoded.clear();
    staged.clear();
    scratch.ordered = staged;
    Ok(rows)
}

fn proofs(
    program: &ValidatedProgram,
    raw: &[u8],
    decoded: &[u8],
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<(ValidationProof, ValidationCapability)> {
    let raw_len = u32::try_from(raw.len()).map_err(|_| resource(0))?;
    let decoded_len = u32::try_from(decoded.len()).map_err(|_| resource(0))?;
    let fingerprint = program.image().header.fingerprint;
    // SAFETY: private caller reached complete strict EOF under this schema and
    // every policy, with spans emitted only by BlockCursor on these exact bytes.
    Ok(unsafe {
        (
            ValidationProof::issue_unchecked(fingerprint, raw_len, decoded_len),
            ValidationCapability::issue_unchecked(
                raw.as_ptr() as usize,
                raw.as_ptr() as usize,
                raw_len,
                decoded_len,
                fingerprint,
                policy_fingerprint(limits, numeric),
            ),
        )
    })
}

/// Strict validation without capture or input copying. No SlotValue vector is
/// allocated or populated. The capability may be inspected but naked pointer
/// capabilities are never accepted by this crate's safe skipping APIs.
pub fn validate_borrowed(
    program: &ValidatedProgram,
    raw: &[u8],
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<(ValidationProof, ValidationCapability)> {
    validate_borrowed_into(
        program,
        raw,
        limits,
        numeric,
        &mut ValidationScratch::default(),
    )
}

pub fn validate_borrowed_into(
    program: &ValidatedProgram,
    raw: &[u8],
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &mut ValidationScratch,
) -> Result<(ValidationProof, ValidationCapability)> {
    execute_direct(
        program,
        raw,
        &no_selection(),
        limits,
        numeric,
        scratch,
        false,
        false,
        None,
    )?;
    proofs(program, raw, &scratch.direct_decoded, limits, numeric)
}

pub fn validate_payload_direct(
    program: &ValidatedProgram,
    payload: &Payload,
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<(ValidationProof, ValidationCapability)> {
    validate_into(
        program,
        payload,
        limits,
        numeric,
        &mut ValidationScratch::default(),
    )
}

/// Warm strict validation only. Extraction has its own explicit entry point.
pub fn validate_into(
    program: &ValidatedProgram,
    payload: &Payload,
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &mut ValidationScratch,
) -> Result<(ValidationProof, ValidationCapability)> {
    let (proof, _) = validate_borrowed_into(program, payload.as_slice(), limits, numeric, scratch)?;
    // SAFETY: validation above covered the exact immutable Payload slice.
    let capability = unsafe {
        ValidationCapability::issue_for_payload(
            payload,
            proof.decoded_len(),
            program.image().header.fingerprint,
            policy_fingerprint(limits, numeric),
        )
    };
    Ok((proof, capability))
}

pub fn extract_borrowed<'a>(
    program: &ValidatedProgram,
    raw: &'a [u8],
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<BorrowedValidatedSlots<'a>> {
    let mut scratch = ValidationScratch::default();
    strict_selection(selection)?;
    execute_direct(
        program,
        raw,
        selection,
        limits,
        numeric,
        &mut scratch,
        true,
        false,
        None,
    )?;
    let (proof, capability) = proofs(program, raw, &scratch.direct_decoded, limits, numeric)?;
    Ok(BorrowedValidatedSlots {
        raw,
        decoded: scratch.direct_decoded,
        slots: scratch.vm.slots,
        proof,
        capability,
        validated_raw: raw,
    })
}

pub fn extract_borrowed_with_scratch<'raw, 'scratch>(
    program: &ValidatedProgram,
    raw: &'raw [u8],
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &'scratch mut ValidationScratch,
) -> Result<BorrowedValidatedSlotsRef<'raw, 'scratch>> {
    if let Err(error) = strict_selection(selection) {
        prepare_output(scratch);
        scratch.vm.slots.clear();
        return Err(error);
    }
    execute_direct(
        program, raw, selection, limits, numeric, scratch, true, false, None,
    )?;
    let (proof, capability) = proofs(program, raw, &scratch.direct_decoded, limits, numeric)?;
    Ok(BorrowedValidatedSlotsRef {
        raw,
        decoded: &scratch.direct_decoded,
        slots: &scratch.vm.slots,
        proof,
        capability,
    })
}

pub fn extract_payload_direct(
    program: &ValidatedProgram,
    payload: Payload,
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<ValidatedSlots> {
    let mut scratch = ValidationScratch::default();
    extract_payload_into(program, payload, selection, limits, numeric, &mut scratch)?;
    scratch
        .output
        .take()
        .ok_or_else(|| invalid(0, "missing strict output"))
}

pub fn extract_payload_into<'a>(
    program: &ValidatedProgram,
    payload: Payload,
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
    scratch: &'a mut ValidationScratch,
) -> Result<&'a ValidatedSlots> {
    if let Err(error) = strict_selection(selection) {
        prepare_output(scratch);
        scratch.vm.slots.clear();
        return Err(error);
    }
    execute_direct(
        program,
        payload.as_slice(),
        selection,
        limits,
        numeric,
        scratch,
        true,
        false,
        None,
    )?;
    let (proof, _) = proofs(
        program,
        payload.as_slice(),
        &scratch.direct_decoded,
        limits,
        numeric,
    )?;
    scratch.output = Some(ValidatedSlots {
        payload,
        decoded: core::mem::take(&mut scratch.direct_decoded),
        slots: core::mem::take(&mut scratch.vm.slots),
        proof,
    });
    scratch
        .output
        .as_ref()
        .ok_or_else(|| invalid(0, "missing strict output"))
}

/// Query skipping never returns a ValidationProof, even if the particular
/// input happened to contain only selected values.
pub fn extract_query_borrowed<'a>(
    program: &ValidatedProgram,
    raw: &'a [u8],
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<BorrowedQuerySlots<'a>> {
    let mut scratch = ValidationScratch::default();
    execute_direct(
        program,
        raw,
        selection,
        limits,
        numeric,
        &mut scratch,
        true,
        !selection.strict_validate_unselected,
        None,
    )?;
    Ok(BorrowedQuerySlots {
        raw,
        decoded: scratch.direct_decoded,
        slots: scratch.vm.slots,
    })
}

pub fn extract_query_payload(
    program: &ValidatedProgram,
    payload: Payload,
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<QuerySlots> {
    let result = extract_query_borrowed(program, payload.as_slice(), selection, limits, numeric)?;
    let BorrowedQuerySlots { decoded, slots, .. } = result;
    Ok(QuerySlots {
        payload,
        decoded,
        slots,
    })
}

/// Re-extract using a live immutable validation guard. The original slice is
/// retained privately, so a stale/copyable pointer capability cannot authorize
/// skipping bytes that the caller has since mutated or freed.
pub fn extract_validated_borrowed<'a>(
    program: &ValidatedProgram,
    source: &BorrowedValidatedSlots<'a>,
    selection: &Selection,
    limits: VmLimits,
    numeric: NumericPolicy,
) -> Result<BorrowedValidatedSlots<'a>> {
    strict_selection(selection)?;
    let raw = source.validated_raw;
    let mut scratch = ValidationScratch::default();
    let capability = source
        .capability
        .matches_slice(
            raw,
            source.capability.decoded_len() as usize,
            program.image().header.fingerprint,
            policy_fingerprint(limits, numeric),
        )
        .then_some(source.capability);
    execute_direct(
        program,
        raw,
        selection,
        limits,
        numeric,
        &mut scratch,
        true,
        false,
        capability,
    )?;
    let (proof, capability) = proofs(program, raw, &scratch.direct_decoded, limits, numeric)?;
    Ok(BorrowedValidatedSlots {
        raw,
        decoded: scratch.direct_decoded,
        slots: scratch.vm.slots,
        proof,
        capability,
        validated_raw: raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use jmax_abi::schema::{SchemaFieldIr, SchemaId, SchemaIr};

    use core::cell::Cell;
    std::thread_local! {
        static FORCE_EXACT: Cell<bool> = const { Cell::new(false) };
    }
    /// True while a test compares the fast paths against the exact engine.
    pub(super) fn force_exact() -> bool {
        FORCE_EXACT.with(Cell::get)
    }

    fn trade_program(
        decimal_exponent: bool,
        object_count_bound: bool,
    ) -> Result<(ValidatedProgram, Selection)> {
        use jmax_schema_ir::{ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode};
        let decimal = || {
            let mut c = ScalarConstraints::new(ValueKind::Decimal);
            c.decimal = Some(jmax_schema_ir::DecimalConstraints {
                policy: jmax_abi::DecimalPolicy {
                    allow_exponent: decimal_exponent,
                    ..jmax_abi::DecimalPolicy::spot_8()
                },
                minimum: None,
                maximum: None,
            });
            c
        };
        let mut bounded = ScalarConstraints::new(ValueKind::Integer);
        bounded.min_i64 = Some(-5);
        bounded.max_i64 = Some(1_000);
        let fields: [(&str, ScalarConstraints, bool); 8] = [
            ("e", ScalarConstraints::new(ValueKind::String), true),
            ("E", ScalarConstraints::new(ValueKind::Integer), true),
            ("p", decimal(), true),
            ("b", bounded, true),
            ("m", ScalarConstraints::new(ValueKind::Boolean), true),
            ("n", ScalarConstraints::new(ValueKind::Null), false),
            ("M", ScalarConstraints::new(ValueKind::Boolean), false),
            (
                "long_name",
                ScalarConstraints::new(ValueKind::String),
                false,
            ),
        ];
        let mut properties = alloc::vec::Vec::new();
        let mut nodes = alloc::vec::Vec::new();
        for (slot, (name, constraints, required)) in fields.into_iter().enumerate() {
            properties.push(Property {
                field_id: FieldId(slot as u32 + 1),
                name: name.into(),
                ty: TypeId(slot as u32 + 1),
                required,
                slot: Some(slot as u32),
            });
            nodes.push(TypeNode::Scalar(constraints));
        }
        nodes.insert(
            0,
            TypeNode::Object(ObjectShape {
                nullable: false,
                properties,
                reject_unknown: true,
                min_properties: object_count_bound.then_some(0),
                max_properties: None,
            }),
        );
        let resolved = jmax_schema_ir::resolve::resolve(
            Schema {
                name: "fixed_object".into(),
                schema_id: SchemaId(9_001),
                root: TypeId(0),
                nodes,
                definitions: vec![],
            },
            Default::default(),
        )?;
        let program = jmax_schema_compile::compile(&resolved, &Default::default())?;
        let selection = Selection {
            field_ids: (1..=8).map(FieldId).collect(),
            strict_validate_unselected: true,
        };
        Ok((program, selection))
    }

    fn run(
        program: &ValidatedProgram,
        selection: &Selection,
        input: &[u8],
        limits: VmLimits,
        scratch: &mut ValidationScratch,
        exact: bool,
    ) -> core::result::Result<Vec<SlotValue>, (ErrorCode, u32, u32)> {
        FORCE_EXACT.with(|f| f.set(exact));
        let result = extract_borrowed_with_scratch(
            program,
            input,
            selection,
            limits,
            NumericPolicy::strict_default(),
            scratch,
        );
        FORCE_EXACT.with(|f| f.set(false));
        match result {
            Ok(slots) => Ok(slots.slots.to_vec()),
            Err(error) => Err((error.code, error.byte_offset, error.field_id)),
        }
    }

    #[test]
    fn fixed_object_matches_the_exact_engine() -> Result<()> {
        let (program, selection) = trade_program(false, false)?;
        // The bounded-count schema never takes the fast path: a second reference.
        let (reference, _) = trade_program(false, true)?;
        let good: &[u8] = br#"{"e":"aggTrade","E":123,"p":"0.50000000","b":7,"m":true,"n":null,"M":false,"long_name":"x"}"#;
        let inputs: [&[u8]; 24] = [
            good,
            br#"{"e":"a","E":-9223372036854775808,"p":"-12.5","b":-5,"m":false}"#,
            br#"{"m":true,"b":1000,"p":"1","E":18446744073709551615,"e":""}"#,
            br#"{"e":"a","E":1,"p":"1","b":1,"m":true,"M":true}"#,
            br#"{"e":"a","E":1,"p":"1","b":1}"#,
            br#"{"e":"a","E":1,"p":"1","b":1001,"m":true}"#,
            br#"{"e":"a","E":1,"p":"1","b":-6,"m":true}"#,
            br#"{"e":"a","E":1,"p":"1.123456789","b":1,"m":true}"#,
            br#"{"e":"a","E":1,"p":"1e2","b":1,"m":true}"#,
            br#"{"e":"a","E":01,"p":"1","b":1,"m":true}"#,
            br#"{"e":"a","E":1.5,"p":"1","b":1,"m":true}"#,
            br#"{"e":"a","E":1,"p":"1","b":1,"m":true,"x":1}"#,
            br#"{"e":"a","e":"b","E":1,"p":"1","b":1,"m":true}"#,
            br#"{"e":"a\u0062","E":1,"p":"1","b":1,"m":true}"#,
            br#"{"e":"caf\u00e9","E":1,"p":"1","b":1,"m":true}"#,
            br#"{"\u0065":"a","E":1,"p":"1","b":1,"m":true}"#,
            br#"{ "e":"a","E":1,"p":"1","b":1,"m":true}"#,
            br#"{"e":"a","E":1,"p":"1","b":1,"m":true,}"#,
            br#"{"e":"a","E":1,"p":"1","b":1,"m":true"#,
            br#"{"e":"a","E":1,"p":"1","b":1,"m":true} x"#,
            br#"{"e":"a","E":1,"p":"1","b":1,"m":null}"#,
            br#"{"e":"\u00e9a","E":-0,"p":"1","b":1,"m":true}"#,
            br#"{}"#,
            br#"{"E":12345678901234567890123,"e":"a","p":"1","b":1,"m":true}"#,
        ];
        let mut fast = ValidationScratch::default();
        let mut exact = ValidationScratch::default();
        let mut other = ValidationScratch::default();
        for input in inputs {
            // Warm each scratch so the fast path sees reusable frame storage.
            for _ in 0..2 {
                run(
                    &program,
                    &selection,
                    input,
                    VmLimits::default(),
                    &mut fast,
                    false,
                )
                .ok();
            }
            let want = run(
                &program,
                &selection,
                input,
                VmLimits::default(),
                &mut exact,
                true,
            );
            let got = run(
                &program,
                &selection,
                input,
                VmLimits::default(),
                &mut fast,
                false,
            );
            assert_eq!(
                got,
                want,
                "{}",
                alloc::string::String::from_utf8_lossy(input)
            );
            let via_reference = run(
                &reference,
                &selection,
                input,
                VmLimits::default(),
                &mut other,
                false,
            );
            assert_eq!(got.is_ok(), via_reference.is_ok());
            // Every budget threshold gives the same verdict and diagnostic.
            for max_steps in (1..1_200).step_by(7) {
                let limits = VmLimits {
                    max_steps,
                    ..VmLimits::default()
                };
                let want = run(&program, &selection, input, limits, &mut exact, true);
                let got = run(&program, &selection, input, limits, &mut fast, false);
                assert_eq!(got, want, "steps {max_steps}");
            }
        }
        Ok(())
    }

    #[test]
    fn fixed_object_fast_path_is_taken_and_leaves_no_slots_on_a_miss() -> Result<()> {
        let (program, selection) = trade_program(false, false)?;
        let mut scratch = ValidationScratch::default();
        let good: &[u8] = br#"{"e":"aggTrade","E":123,"p":"0.50000000","b":7,"m":true,"n":null,"M":false,"long_name":"x"}"#;
        for _ in 0..2 {
            run(
                &program,
                &selection,
                good,
                VmLimits::default(),
                &mut scratch,
                false,
            )
            .ok();
        }
        let slots = run(
            &program,
            &selection,
            good,
            VmLimits::default(),
            &mut scratch,
            false,
        )
        .map_err(|e| vm_error(e.0, e.1, "fast path rejected a valid object"))?;
        assert!(slots.iter().all(|s| *s != SlotValue::Empty));
        // An unknown key after captured fields: the miss must clear them.
        let bad: &[u8] = br#"{"e":"a","E":1,"p":"1","b":1,"m":true,"zz":1}"#;
        let error = run(
            &program,
            &selection,
            bad,
            VmLimits::default(),
            &mut scratch,
            false,
        )
        .unwrap_err();
        assert_eq!(error.0, ErrorCode::UnknownObjectKey);
        Ok(())
    }

    fn mask_program(count: u16) -> Result<ValidatedProgram> {
        let ir = SchemaIr {
            name: alloc::format!("mask-{count}"),
            schema_id: SchemaId(65),
            required_count: 1,
            reject_unknown: true,
            fields: (0..count)
                .map(|index| SchemaFieldIr {
                    field_id: FieldId(u32::from(index)),
                    name: alloc::format!("k{index}"),
                    required: index + 1 == count,
                    scalar_kind: Some(ScalarKind::Number),
                    format: None,
                    min_len: None,
                    max_len: None,
                    min_i64: Some(0),
                    max_i64: Some(10),
                })
                .collect(),
        };
        let resolved = jmax_schema_ir::resolve::resolve(
            jmax_schema_ir::model::normalize(&ir)?,
            Default::default(),
        )?;
        jmax_schema_compile::compile(&resolved, &Default::default())
    }

    #[test]
    fn selection_uses_inline_words_through_256_slots() -> Result<()> {
        for count in [1u16, 64, 65, 256, 257] {
            let program = mask_program(count)?;
            let field = FieldId(u32::from(count - 1));
            let mut selection = Selection {
                field_ids: vec![field],
                strict_validate_unselected: true,
            };
            let index = extract::SelectionIndex::new(&program, &selection)?;
            assert_eq!(index.words.capacity() == 0, count <= 256);
            for slot in 0..u32::from(count) {
                assert_eq!(extract::selected(&index, slot), slot == field.0);
            }
            selection.field_ids.push(field);
            assert!(extract::SelectionIndex::new(&program, &selection).is_err());
        }
        Ok(())
    }

    #[test]
    fn small_objects_need_no_heap_mask_or_unknown_table_and_wide_masks_reuse() -> Result<()> {
        for count in [64u16, 65] {
            let program = mask_program(count)?;
            let raw = alloc::format!("{{\"k{}\":7}}", count - 1);
            let mut scratch = ValidationScratch::default();
            let numeric = NumericPolicy::strict_default();
            validate_borrowed_into(
                &program,
                raw.as_bytes(),
                VmLimits::default(),
                numeric,
                &mut scratch,
            )?;
            assert!(scratch.vm.unknown_key_tables.is_empty());
            assert_eq!(scratch.vm.seen_masks.is_empty(), count <= 64);
            let wide_mask = scratch.vm.seen_masks.first().map(|mask| mask.as_ptr());
            // A failed run must return the active mask before the next call.
            assert_eq!(
                validate_borrowed_into(&program, b"{}", VmLimits::default(), numeric, &mut scratch)
                    .unwrap_err()
                    .code,
                ErrorCode::MissingRequiredField
            );
            validate_borrowed_into(
                &program,
                raw.as_bytes(),
                VmLimits::default(),
                numeric,
                &mut scratch,
            )?;
            assert_eq!(
                scratch.vm.seen_masks.first().map(|mask| mask.as_ptr()),
                wide_mask
            );
        }
        Ok(())
    }

    #[test]
    fn validation_only_never_allocates_slot_values() -> Result<()> {
        let ir = SchemaIr {
            name: "no-capture".into(),
            schema_id: SchemaId(64),
            required_count: 1,
            reject_unknown: false,
            fields: vec![SchemaFieldIr {
                field_id: FieldId(0),
                name: "s".into(),
                required: true,
                scalar_kind: Some(ScalarKind::String),
                format: None,
                min_len: Some(2),
                max_len: None,
                min_i64: None,
                max_i64: None,
            }],
        };
        let resolved = jmax_schema_ir::resolve::resolve(
            jmax_schema_ir::model::normalize(&ir)?,
            Default::default(),
        )?;
        let program = jmax_schema_compile::compile(&resolved, &Default::default())?;
        let mut scratch = ValidationScratch::default();
        let mut limits = VmLimits::default();
        limits.syntax.reject_unknown_keys = false;
        let raw = br#"{"s":"ok","x":{"long":"\n\n\n"}}"#;
        validate_borrowed_into(
            &program,
            raw,
            limits,
            NumericPolicy::strict_default(),
            &mut scratch,
        )?;
        assert_eq!(scratch.vm.slots.capacity(), 0);
        assert!(scratch.vm.slots.is_empty());
        assert_eq!(scratch.vm.selection_words.capacity(), 0);
        assert!(scratch.direct_decoded.is_empty());
        let frames = scratch.vm.frames.as_ptr();
        assert!(scratch.vm.seen_masks.is_empty());
        validate_borrowed_into(
            &program,
            raw,
            limits,
            NumericPolicy::strict_default(),
            &mut scratch,
        )?;
        assert_eq!(scratch.vm.slots.capacity(), 0);
        assert_eq!(scratch.vm.frames.as_ptr(), frames);
        assert!(scratch.vm.seen_masks.is_empty());
        let mut permissive_scratch = ValidationScratch::default();
        limits.syntax.reject_duplicate_keys = false;
        validate_borrowed_into(
            &program,
            raw,
            limits,
            NumericPolicy::strict_default(),
            &mut permissive_scratch,
        )?;
        assert!(permissive_scratch.vm.unknown_key_tables.is_empty());
        assert!(permissive_scratch.vm.seen_masks.is_empty());
        Ok(())
    }

    #[test]
    fn root_array_map_reuses_vm_scratch_and_keeps_syntax_error_precedence() -> Result<()> {
        let program = mask_program(1)?;
        let selection = Selection {
            field_ids: vec![FieldId(0)],
            strict_validate_unselected: true,
        };
        let numeric = NumericPolicy::strict_default();
        let mut scratch = ValidationScratch::default();
        let values = map_root_array_borrowed(
            &program,
            br#"[{"k0":1},{"k0":2}]"#,
            &selection,
            VmLimits::default(),
            numeric,
            &mut scratch,
            |_, _, slots| match slots[0] {
                SlotValue::I64(value) => value,
                SlotValue::U64(value) => value as i64,
                _ => -1,
            },
        )?;
        assert_eq!(values, vec![1, 2]);

        let error = map_root_array_borrowed(
            &program,
            br#"[{"k0":"wrong"},]"#,
            &selection,
            VmLimits::default(),
            numeric,
            &mut scratch,
            |_, _, _| (),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidSyntax);
        Ok(())
    }
}
