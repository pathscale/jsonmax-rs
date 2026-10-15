//! Slot writes and the final ownership boundary for schema validation proofs.
//!
//! Selection affects capture only. The interpreter validates the whole document
//! before handing this module its private completed-run capability.
use super::*;

/// Capture every compiled field for an empty selection, otherwise exact IDs.
///
/// This is a membership predicate, not selection validation. The execution
/// boundary checks IDs, duplicates and `strict_validate_unselected` and charges
/// selection work against its shared budget.
pub fn selected(selection: &Selection, field: FieldId) -> bool {
    selection.field_ids.is_empty() || selection.field_ids.contains(&field)
}

/// Write a completed value exactly once, leaving all slots unchanged on error.
///
/// `Empty` means absent or unselected and cannot represent a captured JSON null.
/// Reference validity and schema constraints are checked by the interpreter;
/// publication checks the references again against the retained owners.
pub fn store(slots: &mut [SlotValue], slot: u32, value: SlotValue, offset: u32) -> Result<()> {
    let index = usize::try_from(slot).map_err(|_| vm_error(
        ErrorCode::InternalError, offset, "capture slot index does not fit usize"))?;
    let target = slots.get_mut(index).ok_or_else(|| vm_error(
        ErrorCode::InternalError, offset, "capture slot index is out of bounds"))?;
    if value == SlotValue::Empty {
        return Err(vm_error(ErrorCode::ConstraintViolation, offset,
            "a captured value cannot be Empty"));
    }
    if *target != SlotValue::Empty {
        return Err(vm_error(ErrorCode::ConstraintViolation, offset,
            "a captured slot cannot be written more than once"));
    }
    *target = value;
    Ok(())
}

/// Consume a completed run and publish its exact buffers and slots with proof.
///
/// This allocation-free boundary does not accept caller assertions of validity.
/// Only execution can construct a completed run, after strict EOF and all schema,
/// resource and ownership checks. All fallible checks here precede proof issue.
/// The returned ABI fields are public; subsequent mutation must preserve their
/// validation invariants before using the proof with downstream consumers.
pub fn publish(run: ValidatedRun) -> Result<ValidatedSlots> {
    let ValidatedRun { buffers, slots, fingerprint } = run;
    let InputBuffers { payload, decoded } = buffers;
    let (payload_len, decoded_len) = owner_lengths(payload.len(), decoded.len())?;
    validate_references(&slots, payload.as_slice(), &decoded)?;

    // SAFETY: ValidatedRun is an unforgeable capability outside this crate.
    // execute::run constructs it only from a ValidatedProgram after complete
    // strict parsing, every schema constraint, and immutable-owner verification
    // under EventInput's audited contract. Its fingerprint comes from that
    // witnessed program. Above, both lengths are checked without truncation and
    // every retained reference is rechecked for bounds, UTF-8 and scalar count.
    // Below, those exact owners and unchanged slots move into the result. No
    // fallible operation or caller code runs between proof issue and assembly.
    let proof = unsafe {
        ValidationProof::issue_unchecked(fingerprint, payload_len, decoded_len)
    };
    Ok(ValidatedSlots { payload, decoded, slots, proof })
}

fn owner_lengths(payload_len: usize, decoded_len: usize) -> Result<(u32, u32)> {
    let payload_len = u32::try_from(payload_len).map_err(|_| vm_error(
        ErrorCode::PayloadTooLarge, 0, "published payload length exceeds u32"))?;
    let decoded_len = u32::try_from(decoded_len).map_err(|_| vm_error(
        ErrorCode::DecodedArenaOverflow, 0, "published decoded arena length exceeds u32"))?;
    Ok((payload_len, decoded_len))
}

fn validate_references(slots: &[SlotValue], raw: &[u8], decoded: &[u8]) -> Result<()> {
    for value in slots {
        match *value {
            SlotValue::Str(reference) => {
                // An arena offset cannot identify a raw token. Its raw location
                // is no longer retained here, so use zero for that diagnostic.
                let offset = match reference.source {
                    SpanSource::RawPayload => reference.span.offset,
                    SpanSource::DecodedArena => 0,
                };
                resolve_str(raw, decoded, reference)
                    .map_err(|error| Error { byte_offset: offset, ..error })?;
            }
            SlotValue::RawSpan(span) => {
                // A complete captured null or container is always nonempty.
                if span.is_empty() || span.slice(raw).is_none() {
                    return Err(vm_error(ErrorCode::InternalError, span.offset,
                        "published raw value span is empty or out of bounds"));
                }
            }
            SlotValue::Empty | SlotValue::Bool(_) | SlotValue::U64(_)
            | SlotValue::I64(_) | SlotValue::F64Bits(_) | SlotValue::Decimal(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec};
    use jmax_schema_compile::CompilerOptions;
    use jmax_schema_ir::{ArrayShape, ObjectShape, Property, ResolveLimits,
        ScalarConstraints, Schema, SchemaId, TypeId, TypeNode};

    fn selection(fields: &[u32]) -> Selection {
        Selection {
            field_ids: fields.iter().copied().map(FieldId).collect(),
            strict_validate_unselected: true,
        }
    }

    fn reference(source: SpanSource, offset: u32, len: u32, scalar_len: u32) -> StrRef {
        StrRef { source, span: Span32::new(offset, len), scalar_len }
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode, offset: u32) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.code, code, "{error:?}");
        assert_eq!(error.source, ErrorSource::SchemaVm, "{error:?}");
        assert_eq!(error.byte_offset, offset, "{error:?}");
        error
    }

    #[test]
    fn selection_is_exact_membership_over_the_full_field_id_range() {
        for field in [0, 1, 65_535, 65_536, u32::MAX] {
            assert!(selected(&selection(&[]), FieldId(field)));
        }
        let subset = selection(&[u32::MAX, 65_536, 0]);
        for field in [u32::MAX, 65_536, 0] { assert!(selected(&subset, FieldId(field))); }
        for field in [1, 65_535, u32::MAX - 1] { assert!(!selected(&subset, FieldId(field))); }
        let mut duplicate = selection(&[9, 9]);
        duplicate.strict_validate_unselected = false;
        assert!(selected(&duplicate, FieldId(9)));
        assert!(!selected(&duplicate, FieldId(8)));
        duplicate.field_ids.clear();
        assert!(selected(&duplicate, FieldId(8)));
    }

    fn present_values() -> Vec<SlotValue> {
        vec![
            SlotValue::Bool(false), SlotValue::Bool(true),
            SlotValue::U64(0), SlotValue::U64(u64::MAX),
            SlotValue::I64(i64::MIN), SlotValue::I64(i64::MAX),
            SlotValue::F64Bits(0), SlotValue::F64Bits(1u64 << 63),
            SlotValue::F64Bits(0x7ff8_0000_0000_0001),
            SlotValue::Decimal(Decimal128::new(i128::MIN, 0)),
            SlotValue::Decimal(Decimal128::new(i128::MAX, 18)),
            SlotValue::Str(reference(SpanSource::RawPayload, 1, 0, 0)),
            SlotValue::Str(reference(SpanSource::DecodedArena, 0, 2, 1)),
            SlotValue::RawSpan(Span32::new(0, 4)),
        ]
    }

    #[test]
    fn store_preserves_every_present_variant_and_all_untargeted_slots() {
        for value in present_values() {
            for slot in 0..3u32 {
                let mut slots = [SlotValue::Empty; 3];
                store(&mut slots, slot, value, u32::MAX).unwrap();
                for (index, actual) in slots.iter().enumerate() {
                    assert_eq!(*actual, if index == slot as usize { value } else { SlotValue::Empty });
                }
            }
        }
    }

    #[test]
    fn store_rejects_all_repeat_writes_and_empty_without_mutation() {
        for previous in present_values() {
            for incoming in present_values().into_iter().chain(core::iter::once(SlotValue::Empty)) {
                let original = [SlotValue::U64(99), previous, SlotValue::Empty];
                let mut slots = original;
                assert_error(store(&mut slots, 1, incoming, 17), ErrorCode::ConstraintViolation, 17);
                assert_eq!(slots, original);
            }
        }
        let mut slots = [SlotValue::Empty; 2];
        assert_error(store(&mut slots, 0, SlotValue::Empty, 23), ErrorCode::ConstraintViolation, 23);
        assert_eq!(slots, [SlotValue::Empty; 2]);
        store(&mut slots, 0, SlotValue::Bool(false), 23).unwrap();
        assert_eq!(slots, [SlotValue::Bool(false), SlotValue::Empty]);
    }

    #[test]
    fn store_checks_indices_before_value_and_never_changes_neighbors() {
        for slot in [2, 3, u32::MAX] {
            for value in [SlotValue::Empty, SlotValue::U64(7)] {
                let original = [SlotValue::Bool(true), SlotValue::Empty];
                let mut slots = original;
                assert_error(store(&mut slots, slot, value, u32::MAX), ErrorCode::InternalError, u32::MAX);
                assert_eq!(slots, original);
            }
        }
        assert_error(store(&mut [], 0, SlotValue::Bool(true), 0), ErrorCode::InternalError, 0);
    }

    #[test]
    fn length_conversion_accepts_inclusive_u32_boundaries() {
        for payload in [0u32, 1, u32::MAX] {
            for arena in [0u32, 1, u32::MAX] {
                if let (Ok(payload_len), Ok(decoded_len)) =
                    (usize::try_from(payload), usize::try_from(arena))
                {
                    assert_eq!(owner_lengths(payload_len, decoded_len).unwrap(), (payload, arena));
                }
            }
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn oversized_lengths_fail_without_allocating_large_buffers() {
        for length in [u32::MAX as usize + 1, usize::MAX] {
            assert_error(owner_lengths(length, 0), ErrorCode::PayloadTooLarge, 0);
            assert_error(owner_lengths(0, length), ErrorCode::DecodedArenaOverflow, 0);
            assert_error(owner_lengths(length, length), ErrorCode::PayloadTooLarge, 0);
        }
    }

    #[test]
    fn reference_validation_handles_empty_unicode_and_both_owners() {
        // This tests reference mechanics only; it does not issue a proof.
        let raw = "aé🦀".as_bytes();
        let arena = "\0e\u{301}".as_bytes();
        let slots = [
            SlotValue::Empty,
            SlotValue::RawSpan(Span32::new(0, 7)),
            SlotValue::RawSpan(Span32::new(6, 1)),
            SlotValue::Str(reference(SpanSource::RawPayload, 0, 7, 3)),
            SlotValue::Str(reference(SpanSource::RawPayload, 1, 2, 1)),
            SlotValue::Str(reference(SpanSource::RawPayload, 3, 4, 1)),
            SlotValue::Str(reference(SpanSource::RawPayload, 7, 0, 0)),
            SlotValue::Str(reference(SpanSource::DecodedArena, 0, 4, 3)),
            SlotValue::Str(reference(SpanSource::DecodedArena, 1, 3, 2)),
            SlotValue::Str(reference(SpanSource::DecodedArena, 4, 0, 0)),
        ];
        validate_references(&slots, raw, arena).unwrap();
        validate_references(&[], &[], &[]).unwrap();
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            validate_references(&[SlotValue::Str(reference(source, 0, 0, 0))], &[], &[]).unwrap();
        }
    }

    #[test]
    fn raw_span_validation_rejects_empty_out_of_bounds_and_overflow() {
        for span in [Span32::new(0, 0), Span32::new(4, 0), Span32::new(0, 5),
            Span32::new(3, 2), Span32::new(5, 1), Span32::new(u32::MAX, 1),
            Span32::new(1, u32::MAX)]
        {
            assert_error(validate_references(&[SlotValue::RawSpan(span)], b"null", b"larger arena"),
                ErrorCode::InternalError, span.offset);
        }
    }

    #[test]
    fn string_ranges_are_checked_against_the_designated_owner() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            let (raw, arena): (&[u8], &[u8]) = match source {
                SpanSource::RawPayload => (b"ab", b"longer"),
                SpanSource::DecodedArena => (b"longer", b"ab"),
            };
            for span in [Span32::new(0, 3), Span32::new(2, 1), Span32::new(3, 0),
                Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX)]
            {
                let value = SlotValue::Str(StrRef { source, span, scalar_len: 0 });
                let offset = if source == SpanSource::RawPayload { span.offset } else { 0 };
                assert_error(validate_references(&[value], raw, arena), ErrorCode::InternalError, offset);
            }
        }
    }

    #[test]
    fn string_validation_rejects_invalid_utf8_and_wrong_scalar_counts() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for invalid in [&b"\xff"[..], &b"\xc0\xaf"[..], &b"\xed\xa0\x80"[..], &b"\xf0\x9f"[..]] {
                let mut owner = b"prefix".to_vec();
                owner.extend_from_slice(invalid);
                let value = SlotValue::Str(reference(source, 6, invalid.len() as u32, 1));
                let offset = if source == SpanSource::RawPayload { 6 } else { 0 };
                assert_error(validate_references(&[value], &owner, &owner), ErrorCode::InvalidUtf8, offset);
            }
            for count in [0, 2, u32::MAX] {
                let value = SlotValue::Str(reference(source, 1, 2, count));
                let offset = if source == SpanSource::RawPayload { 1 } else { 0 };
                assert_error(validate_references(&[value], "xé".as_bytes(), "xé".as_bytes()),
                    ErrorCode::InternalError, offset);
            }
            let value = SlotValue::Str(reference(source, 0, 0, 1));
            assert_error(validate_references(&[value], &[], &[]), ErrorCode::InternalError, 0);
        }
    }

    // Closed fixture with one complete, inspected document. Neither arbitrary
    // bytes nor caller-supplied events can enter this EventInput implementation.
    const DOCUMENT: &str = r#"{"a":7,"s":"Aé","d":"\u00e9","n":null,"v":[true]}"#;

    struct FixtureInput {
        buffers: InputBuffers,
        next: usize,
        eof: bool,
        fail_eof: bool,
        fail_transfer: bool,
    }

    impl FixtureInput {
        fn new() -> Self {
            Self {
                buffers: InputBuffers {
                    payload: Payload::from_slice(DOCUMENT.as_bytes()),
                    decoded: "é".as_bytes().to_vec().into_boxed_slice(),
                },
                next: 0, eof: false, fail_eof: false, fail_transfer: false,
            }
        }
    }

    fn key(offset: u32) -> Event {
        Event::Key { str_ref: reference(SpanSource::RawPayload, offset, 1, 1) }
    }

    fn fixture_events() -> [Event; 14] {
        [
            Event::ObjectStart { offset: 0 }, key(2),
            Event::Scalar { kind: ScalarKind::Number, span: Span32::new(5, 1), decoded: None },
            key(8),
            Event::Scalar { kind: ScalarKind::String, span: Span32::new(11, 5),
                decoded: Some(reference(SpanSource::RawPayload, 12, 3, 2)) },
            key(18),
            Event::Scalar { kind: ScalarKind::String, span: Span32::new(21, 8),
                decoded: Some(reference(SpanSource::DecodedArena, 0, 2, 1)) },
            key(31),
            Event::Scalar { kind: ScalarKind::Null, span: Span32::new(34, 4), decoded: None },
            key(40), Event::ArrayStart { offset: 43 },
            Event::Scalar { kind: ScalarKind::True, span: Span32::new(44, 4), decoded: None },
            Event::ArrayEnd { offset: 48 }, Event::ObjectEnd { offset: 49 },
        ]
    }

    // SAFETY: DOCUMENT is fixed valid UTF-8 and strict JSON with exactly one
    // root and no trailing bytes. fixture_events covers its grammar once in
    // order with exact byte spans; Aé has two scalars and \u00e9 decodes to é.
    // The buffers are never mutated and their exact owners move on transfer.
    // EOF is reported only after all fourteen events. Failure switches can only
    // deny success. The VM enforces schema, duplicate and resource policies.
    unsafe impl EventInput for FixtureInput {
        fn raw_bytes(&self) -> &[u8] { self.buffers.payload.as_slice() }
        fn decoded_bytes(&self) -> &[u8] { &self.buffers.decoded }

        fn next_event(&mut self) -> Result<Option<Event>> {
            if let Some(event) = fixture_events().get(self.next).copied() {
                self.next = self.next.checked_add(1).ok_or_else(|| vm_error(
                    ErrorCode::InternalError, 0, "fixture event counter overflow"))?;
                return Ok(Some(event));
            }
            self.eof = true;
            if self.fail_eof {
                return Err(Error::new(ErrorCode::TrailingContent, ErrorSource::Syntax, 50,
                    "injected strict EOF failure"));
            }
            Ok(None)
        }

        fn into_buffers(self) -> Result<InputBuffers> {
            if !self.eof {
                return Err(vm_error(ErrorCode::InternalError, 0, "fixture EOF was not requested"));
            }
            if self.fail_transfer {
                return Err(Error::new(ErrorCode::Cancelled, ErrorSource::Cursor, 50,
                    "injected buffer transfer failure"));
            }
            Ok(self.buffers)
        }
    }

    fn program(maximum: Option<i64>, missing_required: bool) -> ValidatedProgram {
        let property = |name: &str, field: u32, ty: u32, slot: u32, required: bool| Property {
            name: String::from(name), field_id: FieldId(field), ty: TypeId(ty),
            slot: Some(slot), required,
        };
        let mut integer = ScalarConstraints::new(ValueKind::Integer);
        integer.max_i64 = maximum;
        let schema = Schema {
            name: String::from("extract-integration"), schema_id: SchemaId(418), root: TypeId(0),
            definitions: Vec::new(),
            nodes: vec![
                TypeNode::Object(ObjectShape {
                    properties: vec![
                        property("a", 11, 1, 0, true), property("s", 22, 2, 1, true),
                        property("d", 33, 2, 2, true), property("n", 44, 3, 3, true),
                        property("v", 55, 4, 4, true), property("z", 66, 1, 5, missing_required),
                    ],
                    reject_unknown: true, min_properties: Some(5), max_properties: Some(6),
                }),
                TypeNode::Scalar(integer),
                TypeNode::Scalar(ScalarConstraints::new(ValueKind::String)),
                TypeNode::Scalar(ScalarConstraints::new(ValueKind::Null)),
                TypeNode::Array(ArrayShape { items: TypeId(5), min_items: Some(1), max_items: Some(1) }),
                TypeNode::Scalar(ScalarConstraints::new(ValueKind::Boolean)),
            ],
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits::default()).unwrap();
        jmax_schema_compile::lower::compile(&resolved, &CompilerOptions::default()).unwrap()
    }

    fn completed(program: &ValidatedProgram) -> ValidatedRun {
        execute::run(program, FixtureInput::new(), &selection(&[]), VmLimits::default()).unwrap()
    }

    fn expected_slots() -> Vec<SlotValue> {
        vec![
            SlotValue::U64(7),
            SlotValue::Str(reference(SpanSource::RawPayload, 12, 3, 2)),
            SlotValue::Str(reference(SpanSource::DecodedArena, 0, 2, 1)),
            SlotValue::RawSpan(Span32::new(34, 4)),
            SlotValue::RawSpan(Span32::new(43, 6)),
            SlotValue::Empty,
        ]
    }

    #[test]
    fn fixed_document_vector_has_exact_token_offsets_and_scalar_counts() {
        let bytes = DOCUMENT.as_bytes();
        assert_eq!(bytes.len(), 50);
        for (span, expected) in [
            (Span32::new(5, 1), "7"), (Span32::new(11, 5), "\"Aé\""),
            (Span32::new(21, 8), r#""\u00e9""#),
            (Span32::new(34, 4), "null"), (Span32::new(43, 6), "[true]"),
        ] {
            assert_eq!(span.slice(bytes).unwrap(), expected.as_bytes());
        }
        for (offset, expected) in [(2, b'a'), (8, b's'), (18, b'd'), (31, b'n'), (40, b'v')] {
            assert_eq!(bytes.get(offset), Some(&expected));
        }
        assert_eq!(fixture_events().len(), 14);
        assert_eq!(resolve_str(bytes, "é".as_bytes(),
            reference(SpanSource::RawPayload, 12, 3, 2)).unwrap(), "Aé");
        assert_eq!(resolve_str(bytes, "é".as_bytes(),
            reference(SpanSource::DecodedArena, 0, 2, 1)).unwrap(), "é");
    }

    #[test]
    fn publication_preserves_exact_owners_slots_and_program_fingerprint() {
        let program = program(Some(7), false);
        let input = FixtureInput::new();
        let raw_owner = input.raw_bytes().as_ptr();
        let arena_owner = input.decoded_bytes().as_ptr();
        let run = execute::run(&program, input, &selection(&[]), VmLimits::default()).unwrap();
        assert_eq!(run.slots(), expected_slots().as_slice());
        let slots_owner = run.slots().as_ptr();
        let output = publish(run).unwrap();
        assert_eq!(output.payload.as_slice().as_ptr(), raw_owner);
        assert_eq!(output.decoded.as_ptr(), arena_owner);
        assert_eq!(output.slots.as_ptr(), slots_owner);
        assert_eq!(output.payload.as_slice(), DOCUMENT.as_bytes());
        assert_eq!(&*output.decoded, "é".as_bytes());
        assert_eq!(output.slots, expected_slots());
        assert_eq!(output.proof.schema_fingerprint(), program.image().header.fingerprint);
        assert_eq!(output.proof.payload_len(), 50);
        assert_eq!(output.proof.decoded_len(), 2);
    }

    #[test]
    fn validate_and_explicit_publication_agree_for_every_selection_shape() {
        let program = program(None, false);
        for fields in [&[][..], &[11][..], &[33, 22][..], &[44, 55][..], &[66][..],
            &[66, 55, 44, 33, 22, 11][..]]
        {
            let selection = selection(fields);
            let run = execute::run(&program, FixtureInput::new(), &selection, VmLimits::default()).unwrap();
            let explicit = publish(run).unwrap();
            let direct = execute::validate(&program, FixtureInput::new(), &selection, VmLimits::default()).unwrap();
            let expected: Vec<_> = [11, 22, 33, 44, 55, 66].into_iter()
                .zip(expected_slots()).map(|(field, value)| {
                    if selected(&selection, FieldId(field)) { value } else { SlotValue::Empty }
                }).collect();
            assert_eq!(explicit.slots, expected);
            assert_eq!(direct.slots, expected);
            assert_eq!(direct.slots.len(), 6);
            assert_eq!(direct.proof, explicit.proof);
            assert_eq!(direct.payload.as_slice(), explicit.payload.as_slice());
            assert_eq!(direct.decoded, explicit.decoded);
        }
    }

    #[test]
    fn a_schema_with_no_capture_slots_still_publishes_a_complete_proof() {
        let schema = Schema {
            name: String::from("extract-no-slots"), schema_id: SchemaId(419), root: TypeId(0),
            nodes: vec![TypeNode::Any], definitions: Vec::new(),
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits::default()).unwrap();
        let program = jmax_schema_compile::lower::compile(&resolved, &CompilerOptions::default()).unwrap();
        let output = execute::validate(&program, FixtureInput::new(), &selection(&[]),
            VmLimits::default()).unwrap();
        assert_eq!(program.image().header.slot_count, 0);
        assert!(output.slots.is_empty());
        assert_eq!(output.payload.as_slice(), DOCUMENT.as_bytes());
        assert_eq!(&*output.decoded, "é".as_bytes());
        assert_eq!(output.proof.schema_fingerprint(), program.image().header.fingerprint);
        assert_eq!(output.proof.payload_len(), 50);
        assert_eq!(output.proof.decoded_len(), 2);
    }

    #[test]
    fn unselected_constraints_and_required_fields_still_prevent_publication() {
        let selected = selection(&[33]);
        let bounded = program(Some(6), false);
        let error = assert_error(execute::validate(&bounded, FixtureInput::new(), &selected,
            VmLimits::default()), ErrorCode::ConstraintViolation, 5);
        assert_eq!((error.schema_id, error.field_id), (418, 11));
        let required = program(None, true);
        let error = assert_error(execute::validate(&required, FixtureInput::new(), &selected,
            VmLimits::default()), ErrorCode::MissingRequiredField, 49);
        assert_eq!((error.schema_id, error.field_id), (418, 66));
    }

    #[test]
    fn invalid_selections_cannot_obtain_proofs() {
        let program = program(None, false);
        for (selection, code, field) in [
            (selection(&[11, 11]), ErrorCode::ConstraintViolation, 11),
            (selection(&[u32::MAX]), ErrorCode::UnknownObjectKey, u32::MAX),
            (Selection { field_ids: Vec::new(), strict_validate_unselected: false },
                ErrorCode::UnsupportedSchemaFeature, 0),
        ] {
            let error = assert_error(execute::validate(&program, FixtureInput::new(), &selection,
                VmLimits::default()), code, 0);
            assert_eq!((error.schema_id, error.field_id), (418, field));
        }
    }

    #[test]
    fn strict_eof_and_owner_transfer_errors_prevent_publication() {
        let program = program(None, false);
        for transfer in [false, true] {
            let mut input = FixtureInput::new();
            input.fail_transfer = transfer;
            input.fail_eof = !transfer;
            let error = execute::validate(&program, input, &selection(&[]), VmLimits::default()).unwrap_err();
            assert_eq!(error.byte_offset, 50);
            assert_eq!(error.schema_id, 418);
            assert_eq!(error.code, if transfer { ErrorCode::Cancelled } else { ErrorCode::TrailingContent });
            assert_eq!(error.source, if transfer { ErrorSource::Cursor } else { ErrorSource::Syntax });
        }
    }

    #[test]
    fn runtime_resource_failures_cannot_obtain_proofs() {
        let program = program(None, false);
        let mut payload = VmLimits::default();
        payload.syntax.max_payload_bytes = 49;
        let mut decoded = VmLimits::default();
        decoded.syntax.max_decoded_bytes = 1;
        let work = VmLimits { max_steps: 0, ..VmLimits::default() };
        let scratch = VmLimits { max_scratch_bytes: 0, ..VmLimits::default() };
        for (limits, code) in [(payload, ErrorCode::PayloadTooLarge),
            (decoded, ErrorCode::DecodedArenaOverflow), (work, ErrorCode::BudgetExhausted),
            (scratch, ErrorCode::BudgetExhausted)]
        {
            assert_error(execute::validate(&program, FixtureInput::new(), &selection(&[]), limits), code, 0);
        }
    }

    #[test]
    fn publication_rechecks_corrupted_run_references_before_issuing_proof() {
        let program = program(None, false);
        // Crate-private access is used only to inject failures into genuine
        // completed runs. No fabricated capability reaches successful issuance.
        for (value, code, offset) in [
            (SlotValue::RawSpan(Span32::new(50, 1)), ErrorCode::InternalError, 50),
            (SlotValue::RawSpan(Span32::new(0, 0)), ErrorCode::InternalError, 0),
            (SlotValue::RawSpan(Span32::new(u32::MAX, 1)), ErrorCode::InternalError, u32::MAX),
            (SlotValue::Str(reference(SpanSource::RawPayload, 14, 1, 1)), ErrorCode::InvalidUtf8, 14),
            (SlotValue::Str(reference(SpanSource::RawPayload, 12, 3, 3)), ErrorCode::InternalError, 12),
            (SlotValue::Str(reference(SpanSource::DecodedArena, 2, 1, 1)), ErrorCode::InternalError, 0),
            (SlotValue::Str(reference(SpanSource::DecodedArena, 0, 2, 2)), ErrorCode::InternalError, 0),
        ] {
            let mut run = completed(&program);
            // Place the invalid value last so earlier valid values cannot cause
            // publication to return success before inspecting the entire table.
            *run.slots.last_mut().unwrap() = value;
            assert_error(publish(run), code, offset);
        }
        let mut run = completed(&program);
        run.buffers.decoded[0] = 0xff;
        assert_error(publish(run), ErrorCode::InvalidUtf8, 0);
        let mut run = completed(&program);
        run.buffers.decoded = Vec::new().into_boxed_slice();
        assert_error(publish(run), ErrorCode::InternalError, 0);
        let mut run = completed(&program);
        run.buffers.payload = Payload::from_slice(&DOCUMENT.as_bytes()[..48]);
        assert_error(publish(run), ErrorCode::InternalError, 43);
    }
}
