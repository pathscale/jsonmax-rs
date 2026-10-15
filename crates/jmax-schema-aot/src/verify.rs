//! Structural and identity assertions for borrowed ABI projections.
//!
//! A projection omits executable side tables and canonical schema bytes. These
//! checks neither authenticate an image nor construct a program witness or a
//! validation proof. Execute only a complete, compiler-validated program.

use alloc::vec::Vec;
use jmax_schema_compile::{
    decode_opcode, swar_head, FormatId, IMAGE_FLAGS, IMAGE_MAGIC, IMAGE_VERSION,
};

use crate::{
    aot_error, fingerprint_schema, Error, ErrorCode, ErrorSource, ImageIdentity,
    InsnWord, Opcode, Result, SchemaProvider, StaticSchemaImage,
};

/// Check the header, exact instruction grammar, visible references and keys.
///
/// State identifiers are dense state ordinals, not instruction offsets. Key
/// names and required bits are not checked globally: their scopes belong to
/// omitted object tables. Constraints, pattern programs and perfect hash
/// layouts likewise require complete program validation.
///
/// Temporary storage is proportional to actual keys and slot instructions,
/// never an unchecked header count. Reservations are fallible. Sorting takes
/// O(k log k + s log s); instruction and key-byte traversal is linear.
pub fn verify_structure(image: StaticSchemaImage<'_>) -> Result<()> {
    let header = image.header;
    let invalid = |detail| projection_error(image, 0, detail);
    if header.magic != IMAGE_MAGIC || header.version != IMAGE_VERSION
        || header.flags != IMAGE_FLAGS
    {
        return Err(invalid("unsupported schema image header"));
    }
    if usize::try_from(header.insn_count).ok() != Some(image.insns.len())
        || usize::try_from(header.key_count).ok() != Some(image.keys.len())
    {
        return Err(invalid("schema image counts differ from arrays"));
    }
    if image.insns.is_empty() {
        return Err(invalid("schema image has no instructions"));
    }
    if u32::try_from(image.key_bytes.len()).is_err() {
        return Err(invalid("key bytes exceed the ABI span address space"));
    }

    let mut emit_count = 0usize;
    for insn in image.insns {
        let opcode = decode_opcode(insn.opcode).ok_or_else(|| {
            projection_error(image, insn.field_id, "unknown instruction opcode")
        })?;
        if insn.flags != 0 || (opcode != Opcode::EmitSlot && insn.field_id != 0) {
            return Err(projection_error(
                image, insn.field_id, "nonzero instruction flags or reserved field",
            ));
        }
        if opcode == Opcode::EmitSlot {
            // At most image.insns.len() increments, so usize cannot overflow.
            emit_count += 1;
        }
    }
    if usize::try_from(header.slot_count).ok() != Some(emit_count) {
        return Err(invalid("slot count differs from EmitSlot count"));
    }

    let state_count = verify_templates(image)?;
    if header.root_state >= state_count {
        return Err(invalid("root state is out of bounds"));
    }
    for insn in image.insns {
        if matches!(decode_opcode(insn.opcode), Some(Opcode::EnterArray | Opcode::EmitSlot))
            && insn.arg1 >= u64::from(state_count)
        {
            return Err(projection_error(image, insn.field_id, "target state is out of bounds"));
        }
    }

    let mut byte_end = 0u32;
    for key in image.keys {
        let invalid_key = |detail| projection_error(image, key.field_id, detail);
        if key.span.offset != byte_end {
            return Err(invalid_key("key byte spans are not sequential"));
        }
        byte_end = key.span.offset.checked_add(key.span.len)
            .ok_or_else(|| invalid_key("key byte span overflows"))?;
        let bytes = key.span.slice(image.key_bytes)
            .ok_or_else(|| invalid_key("key byte span is out of bounds"))?;
        if core::str::from_utf8(bytes).is_err() {
            return Err(invalid_key("key bytes are not UTF-8"));
        }
        if swar_head(bytes) != key.swar_head {
            return Err(invalid_key("key SWAR head differs from its bytes"));
        }
        if key.target_state >= state_count {
            return Err(invalid_key("key target state is out of bounds"));
        }
    }
    if usize::try_from(byte_end).ok() != Some(image.key_bytes.len()) {
        return Err(invalid("key bytes contain an unowned suffix"));
    }

    let mut keys: Vec<&crate::KeyEntry> = scratch(image, image.keys.len())?;
    keys.extend(image.keys.iter());
    keys.sort_unstable_by_key(|key| key.field_id);
    for pair in keys.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(projection_error(image, pair[1].field_id, "duplicate key field identifier"));
        }
    }

    let mut emits: Vec<&InsnWord> = scratch(image, emit_count)?;
    emits.extend(image.insns.iter().filter(|insn| {
        decode_opcode(insn.opcode) == Some(Opcode::EmitSlot)
    }));
    emits.sort_unstable_by_key(|insn| insn.arg0);
    for (slot, insn) in emits.iter().enumerate() {
        if usize::try_from(insn.arg0).ok() != Some(slot) {
            return Err(projection_error(image, insn.field_id, "slot indices are not unique and dense"));
        }
        // Global FieldIds allow this consistency check without inventing
        // object ownership or requiring unselected properties to have slots.
        let key = keys.binary_search_by_key(&insn.field_id, |key| key.field_id)
            .ok().map(|index| keys[index])
            .ok_or_else(|| projection_error(image, insn.field_id, "slot field has no key"))?;
        if insn.arg1 != u64::from(key.target_state) {
            return Err(projection_error(image, insn.field_id, "slot and key target states differ"));
        }
    }
    emits.sort_unstable_by_key(|insn| insn.field_id);
    for pair in emits.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(projection_error(image, pair[1].field_id, "field is emitted into multiple slots"));
        }
    }
    Ok(())
}

/// Check projection structure and every supplied identity component exactly.
/// Hash equality is an assertion about metadata, not schema equivalence.
pub fn verify_image(image: StaticSchemaImage<'_>, expected: ImageIdentity) -> Result<()> {
    verify_structure(image)?;
    let header = image.header;
    if header.magic != expected.magic || header.version != expected.version
        || header.schema_id != expected.schema_id.0 || header.fingerprint != expected.fingerprint
    {
        return Err(projection_error(image, 0, "schema image identity mismatch"));
    }
    Ok(())
}

/// Check projection structure and exact equality with the expected fingerprint.
/// The omitted canonical bytes cannot be reconstructed from ABI tables.
pub fn verify_fingerprint(image: StaticSchemaImage<'_>, expected: u64) -> Result<()> {
    verify_structure(image)?;
    if image.header.fingerprint != expected {
        return Err(projection_error(image, 0, "schema image fingerprint mismatch"));
    }
    Ok(())
}

/// Recompute the provider's authoritative canonical fingerprint, compare both
/// advertised fingerprints and the schema identifier, then check the projection.
/// Canonicalization errors propagate unchanged. No executable witness results.
pub fn assert_provider<P: SchemaProvider>(image: StaticSchemaImage<'_>) -> Result<()> {
    let schema = P::schema_ir();
    let canonical = fingerprint_schema(&schema)?;
    if canonical != P::schema_fingerprint() || canonical != image.header.fingerprint {
        return Err(projection_error(image, 0, "provider schema fingerprint mismatch"));
    }
    if schema.schema_id.0 != image.header.schema_id {
        return Err(projection_error(image, 0, "provider schema identifier mismatch"));
    }
    verify_structure(image)
}

fn projection_error(image: StaticSchemaImage<'_>, field: u16, detail: &'static str) -> Error {
    aot_error(ErrorCode::SchemaImageInvalid, ErrorSource::SchemaCompile, detail)
        .with_schema(image.header.schema_id, u32::from(field))
}

fn scratch<T>(image: StaticSchemaImage<'_>, count: usize) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(count).map_err(|_| {
        aot_error(ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
            "cannot allocate schema projection validation scratch")
            .with_schema(image.header.schema_id, 0)
    })?;
    Ok(values)
}

// Parsing the frozen start/end templates recovers state boundaries without
// pretending that a StateId is an instruction index. No recursion is needed,
// including for recursive array/object graphs.
struct Instructions<'a> {
    image: StaticSchemaImage<'a>,
    next: usize,
}

impl Instructions<'_> {
    fn peek(&self) -> Option<Opcode> {
        self.image.insns.get(self.next).and_then(|insn| decode_opcode(insn.opcode))
    }

    fn take(&mut self) -> Result<InsnWord> {
        let insn = self.image.insns.get(self.next).copied()
            .ok_or_else(|| projection_error(self.image, 0, "truncated state instruction template"))?;
        self.next += 1;
        Ok(insn)
    }

    fn exact(&mut self, opcode: Opcode, arg0: u32, arg1: u64) -> Result<()> {
        let insn = self.take()?;
        if decode_opcode(insn.opcode) != Some(opcode) || insn.arg0 != arg0 || insn.arg1 != arg1 {
            return Err(projection_error(self.image, insn.field_id, "invalid state instruction template"));
        }
        Ok(())
    }

    fn scalar(&mut self, state: u32, tag: u64) -> Result<()> {
        match tag {
            0 | 1 | 3 | 255 => {}
            2 => {
                if self.peek() == Some(Opcode::CheckIntBounds) {
                    self.exact(Opcode::CheckIntBounds, state, 0)?;
                }
            }
            4 => self.exact(Opcode::CheckDecimalBounds, state, 0)?,
            5 => {
                if self.peek() == Some(Opcode::CheckStringLen) {
                    self.exact(Opcode::CheckStringLen, state, 0)?;
                }
                if self.peek() == Some(Opcode::ExecPattern) {
                    let insn = self.take()?;
                    if insn.arg0 != state || u32::try_from(insn.arg1).is_err() {
                        return Err(projection_error(self.image, 0, "invalid pattern instruction operands"));
                    }
                }
                if self.peek() == Some(Opcode::ExecFormat) {
                    let insn = self.take()?;
                    let format = u16::try_from(insn.arg1).ok().and_then(FormatId::from_u16);
                    if insn.arg0 != state || format.is_none() {
                        return Err(projection_error(self.image, 0, "invalid format instruction operands"));
                    }
                }
            }
            _ => return Err(projection_error(self.image, 0, "unknown scalar kind tag")),
        }
        Ok(())
    }
}

fn verify_templates(image: StaticSchemaImage<'_>) -> Result<u32> {
    let mut cursor = Instructions { image, next: 0 };
    let mut states = 0u32;
    let mut objects = 0u32;
    while cursor.peek().is_some() {
        let first = cursor.take()?;
        if first.arg0 != states {
            return Err(projection_error(image, first.field_id, "state identifiers are not dense and ordered"));
        }
        match decode_opcode(first.opcode) {
            Some(Opcode::CheckScalarKind) => cursor.scalar(states, first.arg1)?,
            Some(Opcode::EnterArray) => cursor.exact(Opcode::LeaveArray, states, 0)?,
            Some(Opcode::EnterObject) => {
                if first.arg1 != u64::from(objects) {
                    return Err(projection_error(image, 0, "object table indices are not dense and ordered"));
                }
                let matcher = cursor.take()?;
                if !matches!(decode_opcode(matcher.opcode), Some(Opcode::MatchKeySwar | Opcode::MatchKeyPhf))
                    || matcher.arg0 != objects || matcher.arg1 != 0
                {
                    return Err(projection_error(image, matcher.field_id, "invalid object key matcher"));
                }
                if cursor.peek() == Some(Opcode::RejectUnknown) {
                    cursor.exact(Opcode::RejectUnknown, states, 0)?;
                }
                while cursor.peek() == Some(Opcode::EmitSlot) {
                    cursor.take()?;
                }
                cursor.exact(Opcode::LeaveObject, states, 0)?;
                objects = objects.checked_add(1).ok_or_else(|| {
                    projection_error(image, 0, "object table count exceeds the ABI range")
                })?;
            }
            _ => return Err(projection_error(image, first.field_id, "instruction cannot start a state")),
        }
        states = states.checked_add(1)
            .ok_or_else(|| projection_error(image, 0, "state count exceeds the ABI range"))?;
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec};
    use crate::{
        compile_schema, load_program, FieldId, KeyEntry, ScalarKind, SchemaFieldIr,
        SchemaId, SchemaImage, SchemaImageHeader, SchemaIr, Span32, TableOptions,
    };
    use jmax_schema_compile::{opcode_byte, CompilerOptions};
    use jmax_schema_ir::{
        ArrayShape, DecimalConstraints, DecimalPolicy, ObjectShape, Property,
        ResolveLimits, ScalarConstraints, Schema, SchemaPolicy, TypeId, TypeNode,
        ValueKind,
    };

    fn insn(opcode: Opcode, field_id: u16, arg0: u32, arg1: u64) -> InsnWord {
        InsnWord { opcode: opcode_byte(opcode), flags: 0, field_id, arg0, arg1 }
    }

    fn image(insns: Vec<InsnWord>) -> SchemaImage {
        SchemaImage {
            header: SchemaImageHeader {
                magic: IMAGE_MAGIC, version: IMAGE_VERSION, flags: IMAGE_FLAGS,
                schema_id: 7, root_state: 0,
                insn_count: u32::try_from(insns.len()).unwrap(),
                key_count: 0, slot_count: 0, fingerprint: 0x0123_4567_89ab_cdef,
            },
            insns, keys: Vec::new(), key_bytes: Vec::new(),
        }
    }

    fn any_image() -> SchemaImage {
        image(vec![insn(Opcode::CheckScalarKind, 0, 0, 255)])
    }

    fn object_image() -> SchemaImage {
        let mut image = image(vec![
            insn(Opcode::EnterObject, 0, 0, 0),
            insn(Opcode::MatchKeySwar, 0, 0, 0),
            insn(Opcode::RejectUnknown, 0, 0, 0),
            insn(Opcode::EmitSlot, u16::MAX, 1, 1),
            insn(Opcode::EmitSlot, 42, 0, 2),
            insn(Opcode::LeaveObject, 0, 0, 0),
            insn(Opcode::CheckScalarKind, 0, 1, 1),
            insn(Opcode::CheckScalarKind, 0, 2, 5),
        ]);
        image.header.key_count = 2;
        image.header.slot_count = 2;
        image.key_bytes = "abcdefghé".as_bytes().to_vec();
        image.keys = vec![
            KeyEntry {
                swar_head: 0, span: Span32::new(0, 0), field_id: u16::MAX,
                required_bit: 0, target_state: 1,
            },
            KeyEntry {
                swar_head: 0x6867_6665_6463_6261, span: Span32::new(0, 10),
                field_id: 42, required_bit: u16::MAX, target_state: 2,
            },
        ];
        image
    }

    fn identity(image: &SchemaImage) -> ImageIdentity {
        ImageIdentity {
            magic: image.header.magic, version: image.header.version,
            schema_id: SchemaId(image.header.schema_id), fingerprint: image.header.fingerprint,
        }
    }

    fn rejected(image: &SchemaImage, field: u32) -> Error {
        let error = verify_structure(image.into()).unwrap_err();
        assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.schema_id, image.header.schema_id);
        assert_eq!(error.field_id, field);
        assert_eq!(error.byte_offset, 0);
        assert!(!error.detail.is_empty());
        error
    }

    fn refresh_counts(image: &mut SchemaImage) {
        image.header.insn_count = u32::try_from(image.insns.len()).unwrap();
        image.header.key_count = u32::try_from(image.keys.len()).unwrap();
        image.header.slot_count = u32::try_from(image.insns.iter().filter(|word| {
            decode_opcode(word.opcode) == Some(Opcode::EmitSlot)
        }).count()).unwrap();
    }

    #[test]
    fn minimal_projection_and_full_width_identity_vectors() {
        for fingerprint in [0, 1, 0x0123_4567_89ab_cdef, u64::MAX] {
            let mut image = any_image();
            image.header.schema_id = u32::MAX;
            image.header.fingerprint = fingerprint;
            verify_structure((&image).into()).unwrap();
            verify_image((&image).into(), identity(&image)).unwrap();
            verify_fingerprint((&image).into(), fingerprint).unwrap();
            let failure = verify_fingerprint((&image).into(), fingerprint ^ 1).unwrap_err();
            assert_eq!(failure, projection_error((&image).into(), 0,
                "schema image fingerprint mismatch"));
        }
    }

    #[test]
    fn every_expected_identity_component_is_compared() {
        let image = object_image();
        let good = identity(&image);
        for expected in [
            ImageIdentity { magic: good.magic ^ 1, ..good },
            ImageIdentity { version: good.version + 1, ..good },
            ImageIdentity { schema_id: SchemaId(8), ..good },
            ImageIdentity { fingerprint: good.fingerprint ^ (1 << 63), ..good },
        ] {
            let error = verify_image((&image).into(), expected).unwrap_err();
            assert_eq!(error, projection_error((&image).into(), 0, "schema image identity mismatch"));
        }
    }

    #[test]
    fn matching_identity_never_bypasses_structure() {
        let mut image = object_image();
        image.insns[1].flags = 1;
        let error = rejected(&image, 0);
        assert_eq!(verify_image((&image).into(), identity(&image)).unwrap_err(), error);
        assert_eq!(verify_fingerprint((&image).into(), image.header.fingerprint).unwrap_err(), error);
        assert_eq!(crate::tables::to_owned((&image).into()).unwrap_err(), error);
        assert_eq!(crate::tables::emit_tables((&image).into(), &TableOptions::default()).unwrap_err(), error);
    }

    #[test]
    fn header_counts_flags_and_empty_instruction_vectors_are_rejected() {
        let mutations: [fn(&mut SchemaImageHeader); 11] = [
            |h: &mut SchemaImageHeader| h.magic = 0,
            |h: &mut SchemaImageHeader| h.version = 0,
            |h: &mut SchemaImageHeader| h.version = u16::MAX,
            |h: &mut SchemaImageHeader| h.flags = 1,
            |h: &mut SchemaImageHeader| h.flags = u16::MAX,
            |h: &mut SchemaImageHeader| h.insn_count = 0,
            |h: &mut SchemaImageHeader| h.insn_count = u32::MAX,
            |h: &mut SchemaImageHeader| h.key_count = 1,
            |h: &mut SchemaImageHeader| h.key_count = u32::MAX,
            |h: &mut SchemaImageHeader| h.slot_count = 1,
            |h: &mut SchemaImageHeader| h.slot_count = u32::MAX,
        ];
        for mutate in mutations {
            let mut image = any_image();
            mutate(&mut image.header);
            rejected(&image, 0);
        }
        rejected(&image(Vec::new()), 0);
    }

    #[test]
    fn opcode_bytes_and_reserved_fields_are_checked_without_enum_casts() {
        for byte in 0..=u8::MAX {
            if decode_opcode(byte).is_none() {
                let mut image = any_image();
                image.insns[0].opcode = byte;
                rejected(&image, 0);
            }
        }
        for flags in [1, 128, u8::MAX] {
            let mut image = object_image();
            image.insns[3].flags = flags;
            rejected(&image, u32::from(u16::MAX));
        }
        let mut image = any_image();
        image.insns[0].field_id = u16::MAX;
        rejected(&image, u32::from(u16::MAX));
    }

    #[test]
    fn scalar_templates_accept_only_applicable_ordered_checks() {
        for tag in [0, 1, 2, 3, 4, 5, 255] {
            let mut words = vec![insn(Opcode::CheckScalarKind, 0, 0, tag)];
            if tag == 4 {
                words.push(insn(Opcode::CheckDecimalBounds, 0, 0, 0));
            }
            verify_structure((&image(words)).into()).unwrap();
        }
        for tag in [6, 254, 256, u64::MAX] {
            rejected(&image(vec![insn(Opcode::CheckScalarKind, 0, 0, tag)]), 0);
        }
        let checks = [Opcode::CheckStringLen, Opcode::ExecPattern, Opcode::ExecFormat];
        for mask in 0..8 {
            let mut words = vec![insn(Opcode::CheckScalarKind, 0, 0, 5)];
            for (index, opcode) in checks.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    let arg1 = if *opcode == Opcode::ExecFormat { 10 } else { 0 };
                    words.push(insn(*opcode, 0, 0, arg1));
                }
            }
            verify_structure((&image(words)).into()).unwrap();
        }
        verify_structure((&image(vec![
            insn(Opcode::CheckScalarKind, 0, 0, 2),
            insn(Opcode::CheckIntBounds, 0, 0, 0),
        ])).into()).unwrap();
        for words in [
            vec![insn(Opcode::CheckScalarKind, 0, 0, 4)],
            vec![insn(Opcode::CheckScalarKind, 0, 0, 3), insn(Opcode::CheckIntBounds, 0, 0, 0)],
            vec![insn(Opcode::CheckScalarKind, 0, 0, 255), insn(Opcode::ExecPattern, 0, 0, 0)],
            vec![insn(Opcode::CheckScalarKind, 0, 0, 5), insn(Opcode::CheckDecimalBounds, 0, 0, 0)],
            vec![insn(Opcode::CheckScalarKind, 0, 0, 5), insn(Opcode::ExecPattern, 0, 0, 0),
                insn(Opcode::CheckStringLen, 0, 0, 0)],
            vec![insn(Opcode::CheckScalarKind, 0, 0, 2), insn(Opcode::CheckIntBounds, 0, 0, 0),
                insn(Opcode::CheckIntBounds, 0, 0, 0)],
            vec![insn(Opcode::CheckScalarKind, 0, 0, 5), insn(Opcode::ExecFormat, 0, 0, 1),
                insn(Opcode::ExecPattern, 0, 0, 0)],
        ] {
            rejected(&image(words), 0);
        }
    }

    #[test]
    fn scalar_operands_are_not_truncated_or_confused_with_side_table_validation() {
        for format in [1, 10] {
            verify_structure((&image(vec![insn(Opcode::CheckScalarKind, 0, 0, 5),
                insn(Opcode::ExecFormat, 0, 0, format)])).into()).unwrap();
        }
        for format in [0, 11, u64::from(u16::MAX), 65537, u64::MAX] {
            rejected(&image(vec![insn(Opcode::CheckScalarKind, 0, 0, 5),
                insn(Opcode::ExecFormat, 0, 0, format)]), 0);
        }
        // A representable pattern index cannot be range-checked without the
        // omitted pattern vector. This deliberately does not certify it.
        verify_structure((&image(vec![insn(Opcode::CheckScalarKind, 0, 0, 5),
            insn(Opcode::ExecPattern, 0, 0, u64::from(u32::MAX))])).into()).unwrap();
        for (tag, opcode, arg0, arg1) in [
            (2, Opcode::CheckIntBounds, 0, 1),
            (4, Opcode::CheckDecimalBounds, 0, 1),
            (5, Opcode::CheckStringLen, 1, 0),
            (5, Opcode::ExecPattern, 1, 0),
            (5, Opcode::ExecPattern, 0, u64::from(u32::MAX) + 1),
            (5, Opcode::ExecFormat, 1, 1),
        ] {
            rejected(&image(vec![insn(Opcode::CheckScalarKind, 0, 0, tag),
                insn(opcode, 0, arg0, arg1)]), 0);
        }
    }

    #[test]
    fn state_references_use_dense_ordinals_and_allow_recursive_edges() {
        let mut image = image(vec![
            insn(Opcode::EnterArray, 0, 0, 1), insn(Opcode::LeaveArray, 0, 0, 0),
            insn(Opcode::EnterArray, 0, 1, 0), insn(Opcode::LeaveArray, 0, 1, 0),
        ]);
        verify_structure((&image).into()).unwrap();
        image.insns[0].arg1 = 0;
        verify_structure((&image).into()).unwrap();
        for target in [2, 3, u64::from(u32::MAX), u64::MAX] {
            image.insns[0].arg1 = target;
            rejected(&image, 0);
        }
        image.insns[0].arg1 = 1;
        for root in [2, 3, u32::MAX] {
            image.header.root_state = root;
            rejected(&image, 0);
        }
        image.header.root_state = 0;
        image.insns[2].arg0 = 2; // Instruction offset, not state ordinal.
        rejected(&image, 0);
    }

    #[test]
    fn container_templates_reject_truncation_nesting_and_bad_operands() {
        let full = object_image();
        for end in 0..full.insns.len() {
            let mut partial = full.clone();
            partial.insns.truncate(end);
            refresh_counts(&mut partial);
            let error = verify_structure((&partial).into()).unwrap_err();
            assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
        }
        for index in [0, 1, 2, 5] {
            let mut changed = full.clone();
            changed.insns[index].arg0 = u32::MAX;
            rejected(&changed, 0);
            let mut changed = full.clone();
            changed.insns[index].arg1 = u64::MAX;
            rejected(&changed, 0);
        }
        for words in [
            vec![insn(Opcode::EnterArray, 0, 0, 0)],
            vec![insn(Opcode::LeaveArray, 0, 0, 0)],
            vec![insn(Opcode::EnterArray, 0, 0, 0), insn(Opcode::LeaveObject, 0, 0, 0)],
            vec![insn(Opcode::EnterArray, 0, 0, 0), insn(Opcode::LeaveArray, 0, 1, 0)],
            vec![insn(Opcode::EnterArray, 0, 0, 0), insn(Opcode::LeaveArray, 0, 0, 1)],
            vec![insn(Opcode::EnterObject, 0, 0, 0), insn(Opcode::EnterObject, 0, 1, 1)],
            vec![insn(Opcode::EnterObject, 0, 0, 0), insn(Opcode::MatchKeySwar, 0, 0, 0),
                insn(Opcode::RejectUnknown, 0, 0, 0), insn(Opcode::RejectUnknown, 0, 0, 0),
                insn(Opcode::LeaveObject, 0, 0, 0)],
        ] {
            rejected(&image(words), 0);
        }
    }

    #[test]
    fn empty_objects_both_matchers_and_unselected_maximum_field_ids_are_valid() {
        for matcher in [Opcode::MatchKeySwar, Opcode::MatchKeyPhf] {
            let mut image = object_image();
            image.insns.retain(|word| decode_opcode(word.opcode) != Some(Opcode::EmitSlot));
            image.insns[1].opcode = opcode_byte(matcher);
            refresh_counts(&mut image);
            assert_eq!(image.header.slot_count, 0);
            verify_structure((&image).into()).unwrap();
        }
        verify_structure((&image(vec![
            insn(Opcode::EnterObject, 0, 0, 0), insn(Opcode::MatchKeySwar, 0, 0, 0),
            insn(Opcode::LeaveObject, 0, 0, 0),
        ])).into()).unwrap();
    }

    #[test]
    fn key_span_utf8_swar_and_target_errors_carry_field_context() {
        let mutations: [fn(&mut KeyEntry); 6] = [
            |key: &mut KeyEntry| key.span.offset = 1,
            |key: &mut KeyEntry| key.span.len = 11,
            |key: &mut KeyEntry| key.span.len = u32::MAX,
            |key: &mut KeyEntry| key.swar_head ^= 1,
            |key: &mut KeyEntry| key.target_state = 3,
            |key: &mut KeyEntry| key.target_state = u32::MAX,
        ];
        for mutate in mutations {
            let mut image = object_image();
            mutate(&mut image.keys[1]);
            rejected(&image, 42);
        }
        let mut invalid_utf8 = object_image();
        invalid_utf8.key_bytes[9] = 0xff;
        rejected(&invalid_utf8, 42);
        let mut split_utf8 = object_image();
        split_utf8.keys[1].span.len = 9;
        rejected(&split_utf8, 42);
        let mut trailing = object_image();
        trailing.key_bytes.push(0);
        rejected(&trailing, 0);
        let mut orphan = any_image();
        orphan.key_bytes.push(b'x');
        rejected(&orphan, 0);
        let mut duplicate = object_image();
        duplicate.keys[1].field_id = u16::MAX;
        rejected(&duplicate, u32::from(u16::MAX));
    }

    #[test]
    fn key_bytes_have_exact_coverage_including_empty_and_unicode_names() {
        let mut image = object_image();
        for name in ["", "a", "abcdefg", "abcdefgh", "abcdefghi", "é🦀", "\0\"\\\n"] {
            image.key_bytes = name.as_bytes().to_vec();
            image.keys[1].span.len = u32::try_from(name.len()).unwrap();
            image.keys[1].swar_head = swar_head(name.as_bytes());
            verify_structure((&image).into()).unwrap();
        }
        // Even a valid UTF-8 suffix with an unchanged eight-byte head is part
        // of the key span. The verifier must check its complete bounds.
        image.key_bytes = b"abcdefghx".to_vec();
        image.keys[1].span = Span32::new(0, 8);
        image.keys[1].swar_head = 0x6867_6665_6463_6261;
        rejected(&image, 0);
        // Make two nonempty spans, then overlap or leave a gap between them.
        image.key_bytes = b"ab".to_vec();
        image.keys[0].span = Span32::new(0, 1);
        image.keys[0].swar_head = u64::from(b'a');
        image.keys[1].span = Span32::new(1, 1);
        image.keys[1].swar_head = u64::from(b'b');
        verify_structure((&image).into()).unwrap();
        for offset in [0, 2, u32::MAX] {
            image.keys[1].span.offset = offset;
            rejected(&image, 42);
        }
        image.keys[1].span = Span32::new(1, u32::MAX);
        rejected(&image, 42);
    }

    #[test]
    fn duplicate_names_and_required_bits_are_not_global_invariants() {
        let mut image = image(vec![
            insn(Opcode::EnterObject, 0, 0, 0), insn(Opcode::MatchKeySwar, 0, 0, 0),
            insn(Opcode::LeaveObject, 0, 0, 0),
            insn(Opcode::EnterObject, 0, 1, 1), insn(Opcode::MatchKeyPhf, 0, 1, 0),
            insn(Opcode::LeaveObject, 0, 1, 0),
            insn(Opcode::CheckScalarKind, 0, 2, 255),
        ]);
        image.keys = vec![
            KeyEntry { span: Span32::new(0, 1), swar_head: 120, field_id: u16::MAX,
                required_bit: 0, target_state: 1 },
            KeyEntry { span: Span32::new(1, 1), swar_head: 120, field_id: 42,
                required_bit: 0, target_state: 2 },
        ];
        image.key_bytes = b"xx".to_vec();
        refresh_counts(&mut image);
        verify_structure((&image).into()).unwrap();
        image.key_bytes.clear();
        for key in &mut image.keys {
            key.span = Span32::new(0, 0);
            key.swar_head = 0;
            key.required_bit = u16::MAX - 1;
        }
        verify_structure((&image).into()).unwrap();
    }

    #[test]
    fn slots_are_dense_unique_and_consistent_with_visible_keys() {
        verify_structure((&object_image()).into()).unwrap();
        for slot in [0, 2, u32::MAX] {
            let mut image = object_image();
            image.insns[3].arg0 = slot;
            let error = verify_structure((&image).into()).unwrap_err();
            assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
        }
        for target in [0, 2, 3, u64::from(u32::MAX) + 2, u64::MAX] {
            let mut image = object_image();
            image.insns[3].arg1 = target;
            rejected(&image, u32::from(u16::MAX));
        }
        let mut missing = object_image();
        missing.insns[3].field_id = 43;
        rejected(&missing, 43);
        let mut duplicate = object_image();
        duplicate.insns[3].field_id = 42;
        duplicate.insns[3].arg1 = 2;
        rejected(&duplicate, 42);
    }

    #[test]
    fn reservation_capacity_overflow_is_a_contextual_error() {
        let image = any_image();
        let error = scratch::<u64>((&image).into(), usize::MAX).unwrap_err();
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.schema_id, error.field_id), (7, 0));
    }

    struct EmptyProvider;

    impl SchemaProvider for EmptyProvider {
        fn schema_ir() -> SchemaIr {
            SchemaIr { schema_id: SchemaId(7), ..SchemaIr::default() }
        }

        fn schema_fingerprint() -> u64 {
            fingerprint_schema(&Self::schema_ir()).unwrap()
        }
    }

    struct StaleProvider;

    impl SchemaProvider for StaleProvider {
        fn schema_ir() -> SchemaIr { EmptyProvider::schema_ir() }
        fn schema_fingerprint() -> u64 { EmptyProvider::schema_fingerprint() ^ 1 }
    }

    struct InvalidProvider;

    impl SchemaProvider for InvalidProvider {
        fn schema_ir() -> SchemaIr {
            SchemaIr { required_count: 1, ..EmptyProvider::schema_ir() }
        }

        fn schema_fingerprint() -> u64 { EmptyProvider::schema_fingerprint() }
    }

    struct UnknownFormatProvider;

    impl SchemaProvider for UnknownFormatProvider {
        fn schema_ir() -> SchemaIr {
            let mut schema = flat_schema();
            schema.fields[0].format = Some(String::from("not-a-format"));
            schema
        }

        fn schema_fingerprint() -> u64 { EmptyProvider::schema_fingerprint() }
    }

    #[test]
    fn canonical_empty_schema_and_fnv_reference_vectors() {
        for (bytes, expected) in [
            (&b""[..], 0xcbf2_9ce4_8422_2325),
            (&b"a"[..], 0xaf63_dc4c_8601_ec8c),
            (&b"foobar"[..], 0x8594_4171_f739_67e8),
        ] {
            assert_eq!(jmax_schema_ir::canonical::fingerprint_bytes(bytes), expected);
        }
        assert_eq!(crate::FINGERPRINT_OFFSET, jmax_schema_ir::FINGERPRINT_OFFSET);
        assert_eq!(crate::FINGERPRINT_PRIME, jmax_schema_ir::FINGERPRINT_PRIME);
        let schema = EmptyProvider::schema_ir();
        let normalized = jmax_schema_ir::model::normalize(&schema).unwrap();
        let resolved = jmax_schema_ir::resolve::resolve(normalized, ResolveLimits::default()).unwrap();
        let canonical = jmax_schema_ir::canonical::canonicalize(&resolved, &SchemaPolicy::default()).unwrap();
        // Frozen grammar: prefix/version, default policy, empty name, schema 7,
        // one root object, no bounds/properties, and unknown keys permitted.
        let expected = concat!(
            "JMAX-SCHEMA\0", "\x01\x00",
            "\x00\x00\x01\x01\x34\x01\x00\x01\x00\x10\x00\x00",
            "\x00\x00\x00\x00", "\x07\x00\x00\x00", "\x01\x00\x00\x00",
            "\x02\x00\x00\x00\x00\x00\x00\x00",
        ).as_bytes();
        assert_eq!(canonical.bytes, expected);
        assert_eq!(canonical.fingerprint, jmax_schema_ir::canonical::fingerprint_bytes(expected));
        assert_eq!(fingerprint_schema(&schema).unwrap(), canonical.fingerprint);
    }

    #[test]
    fn provider_requires_canonical_agreement_identifier_and_valid_structure() {
        let program = compile_schema(&EmptyProvider::schema_ir()).unwrap();
        assert_provider::<EmptyProvider>(program.image().into()).unwrap();
        let mut image = program.image().clone();
        image.header.fingerprint = StaleProvider::schema_fingerprint();
        // Agreement between two stale advertisements is insufficient.
        let error = assert_provider::<StaleProvider>((&image).into()).unwrap_err();
        assert_eq!(error, projection_error((&image).into(), 0, "provider schema fingerprint mismatch"));
        assert!(assert_provider::<EmptyProvider>((&image).into()).is_err());
        image.header.fingerprint = EmptyProvider::schema_fingerprint();
        assert!(assert_provider::<StaleProvider>((&image).into()).is_err());
        image.header.schema_id = 8;
        assert_eq!(assert_provider::<EmptyProvider>((&image).into()).unwrap_err(),
            projection_error((&image).into(), 0, "provider schema identifier mismatch"));
        image.header.schema_id = 7;
        image.insns[0].flags = 1;
        assert_eq!(assert_provider::<EmptyProvider>((&image).into()).unwrap_err(), rejected(&image, 0));
    }

    #[test]
    fn provider_normalization_and_policy_errors_propagate_unchanged() {
        let mut image = any_image();
        image.header.magic = 0;
        assert_eq!(assert_provider::<InvalidProvider>((&image).into()).unwrap_err(),
            fingerprint_schema(&InvalidProvider::schema_ir()).unwrap_err());
        assert_eq!(assert_provider::<UnknownFormatProvider>((&image).into()).unwrap_err(),
            fingerprint_schema(&UnknownFormatProvider::schema_ir()).unwrap_err());
    }

    fn field(id: u32, name: &str, kind: Option<ScalarKind>) -> SchemaFieldIr {
        SchemaFieldIr {
            field_id: FieldId(id), name: String::from(name), required: false,
            scalar_kind: kind, format: None, min_len: None, max_len: None,
            min_i64: None, max_i64: None,
        }
    }

    fn flat_schema() -> SchemaIr {
        SchemaIr {
            name: String::from("aot-vectors"), schema_id: SchemaId(71),
            required_count: 1, reject_unknown: true,
            fields: vec![
                field(65535, "zé", Some(ScalarKind::String)),
                SchemaFieldIr { required: true, min_i64: Some(i64::MIN), max_i64: Some(i64::MAX),
                    ..field(42, "", Some(ScalarKind::Number)) },
                field(7, "a", Some(ScalarKind::True)),
                field(9, "raw", None),
            ],
        }
    }

    #[test]
    fn compiler_tables_and_complete_image_round_trip_share_identity() {
        let schema = flat_schema();
        let program = compile_schema(&schema).unwrap();
        let projection = StaticSchemaImage::from(program.image());
        verify_fingerprint(projection, fingerprint_schema(&schema).unwrap()).unwrap();
        assert_eq!(projection.keys[0].field_id, 42);
        assert_eq!(projection.keys[3].field_id, u16::MAX);
        assert_eq!(program.program().slots[0].field_id, FieldId(65535));
        let owned = crate::tables::to_owned(projection).unwrap();
        assert_eq!(owned.header, projection.header);
        assert_eq!(owned.insns, projection.insns);
        assert_eq!(owned.keys, projection.keys);
        assert_eq!(owned.key_bytes, projection.key_bytes);
        let source = crate::tables::emit_program(&program, &TableOptions::default()).unwrap();
        assert_eq!(source, crate::tables::emit_program(&program, &TableOptions::default()).unwrap());
        assert!(source.contains("pub static JMAX_SCHEMA_PROGRAM: &[u8]"));
        let bytes = jmax_schema_compile::image::encode(&program,
            jmax_schema_compile::DEFAULT_MAX_IMAGE_BYTES).unwrap();
        let loaded = load_program(&bytes).unwrap();
        verify_image(loaded.image().into(), identity(program.image())).unwrap();
        assert_eq!(loaded.program().canonical_schema, program.program().canonical_schema);
        assert_eq!(jmax_schema_compile::image::encode(&loaded,
            jmax_schema_compile::DEFAULT_MAX_IMAGE_BYTES).unwrap(), bytes);
        // ABI declaration order assigns explicit slots, so reversing fields
        // changes identity even though canonical key order stays sorted.
        let mut reordered = schema;
        reordered.fields.reverse();
        assert_ne!(fingerprint_schema(&reordered).unwrap(), projection.header.fingerprint);
    }

    fn property(id: u32, name: &str, target: u32) -> Property {
        Property { field_id: FieldId(id), name: String::from(name), ty: TypeId(target),
            required: false, slot: None }
    }

    fn object(properties: Vec<Property>) -> TypeNode {
        TypeNode::Object(ObjectShape { properties, reject_unknown: false,
            min_properties: None, max_properties: None })
    }

    #[test]
    fn compiler_graph_vectors_cover_all_scalar_checks_arrays_and_repeated_names() {
        let mut integer = ScalarConstraints::new(ValueKind::Integer);
        integer.min_i64 = Some(i64::MIN);
        let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
        decimal.decimal = Some(DecimalConstraints {
            policy: DecimalPolicy::spot_8(), minimum: None, maximum: None,
        });
        let mut string = ScalarConstraints::new(ValueKind::String);
        string.min_length = Some(0);
        string.max_length = Some(100);
        string.pattern = Some(String::from("^[0-9a-f]*$"));
        string.format = Some(String::from("hex"));
        let graph = Schema {
            name: String::from("graph-vectors"), schema_id: SchemaId(72), root: TypeId(0),
            definitions: Vec::new(), nodes: vec![
                object(vec![property(1, "same", 1), property(2, "array", 2),
                    property(3, "integer", 3), property(4, "decimal", 4),
                    property(5, "string", 5), property(6, "null", 6),
                    property(7, "boolean", 7), property(8, "number", 8), property(9, "any", 9)]),
                object(vec![property(65535, "same", 5)]),
                TypeNode::Array(ArrayShape { items: TypeId(5), min_items: Some(0), max_items: Some(3) }),
                TypeNode::Scalar(integer), TypeNode::Scalar(decimal), TypeNode::Scalar(string),
                TypeNode::Scalar(ScalarConstraints::new(ValueKind::Null)),
                TypeNode::Scalar(ScalarConstraints::new(ValueKind::Boolean)),
                TypeNode::Scalar(ScalarConstraints::new(ValueKind::Number)), TypeNode::Any,
            ],
        };
        let resolved = jmax_schema_ir::resolve::resolve(graph.clone(), ResolveLimits::default()).unwrap();
        for threshold in [0, u16::MAX] {
            let options = CompilerOptions { swar_threshold: threshold, ..CompilerOptions::default() };
            let program = jmax_schema_compile::lower::compile(&resolved, &options).unwrap();
            verify_structure(program.image().into()).unwrap();
            assert_eq!(program.image().header.slot_count, 0);
            assert_eq!(program.image().header.fingerprint,
                jmax_schema_ir::canonical::fingerprint(&resolved, &options.policy).unwrap());
        }
        // Graph property order has no identity effect when explicit slots and
        // FieldIds remain unchanged.
        let mut reordered = graph;
        if let TypeNode::Object(root) = &mut reordered.nodes[0] {
            root.properties.reverse();
        }
        let reordered = jmax_schema_ir::resolve::resolve(reordered, ResolveLimits::default()).unwrap();
        assert_eq!(jmax_schema_ir::canonical::canonicalize(&resolved, &SchemaPolicy::default()).unwrap(),
            jmax_schema_ir::canonical::canonicalize(&reordered, &SchemaPolicy::default()).unwrap());
    }
}
