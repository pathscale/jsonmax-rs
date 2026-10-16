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
    for slot in &context.program.program().slots {
        if extract::selected(&context.selection, slot.slot) && !needed[slot.state.0 as usize] {
            needed[slot.state.0 as usize] = true;
            execute::reserve(queue, 1, &mut context.budget, 0)?;
            queue.push(slot.state);
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
        let byte = self.cursor.skip_whitespace()?.ok_or_else(|| {
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
                        let meta = self.cursor.scan_string_meta(self.decoded)?;
                        self.charge_decoded()?;
                        (ScalarKind::String, meta.raw_token, Some(meta.cooked))
                    }
                    b'-' | b'0'..=b'9' => {
                        (ScalarKind::Number, self.cursor.scan_number_span()?, None)
                    }
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
                    if bytes.iter().filter(|byte| byte.is_ascii_digit()).count()
                        > usize::from(self.context.limits.syntax.max_number_digits)
                    {
                        return Err(resource(offset));
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

    fn object_key(&mut self) -> Result<()> {
        if self.cursor.peek_byte() != Some(b'"') {
            return Err(invalid(self.cursor.offset(), "expected object key"));
        }
        let meta = self.cursor.scan_key_meta(
            self.decoded,
            self.context.limits.syntax.reject_duplicate_keys,
        )?;
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
        self.cursor.skip_whitespace()?;
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

    fn run(&mut self) -> Result<()> {
        self.value()?;
        while !self.context.root_done {
            let byte = self.cursor.skip_whitespace()?.ok_or_else(|| {
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
        self.cursor.finish_eof()
    }
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
}
