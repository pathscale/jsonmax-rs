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
    // Key spans address only names. A compiled program appends its canonical
    // schema after them as a capsule (bytes, their length as u32 LE, magic); that
    // is the only suffix allowed.
    let owned = usize::try_from(byte_end).map_err(|_| invalid("key bytes contain an unowned suffix"))?;
    let tail = image.key_bytes.get(owned..).ok_or_else(|| invalid("key bytes contain an unowned suffix"))?;
    if !tail.is_empty() && !is_schema_capsule(tail) {
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

/// Magic that ends a schema capsule; it must equal jmax-schema-compile's.
const CAPSULE_MAGIC: &[u8; 8] = b"JMXSCM02";
const CAPSULE_FOOTER: usize = 12;

/// Whether `tail` is exactly one capsule: canonical bytes, their length, the magic.
fn is_schema_capsule(tail: &[u8]) -> bool {
    let Some(footer) = tail.len().checked_sub(CAPSULE_FOOTER) else { return false };
    if tail[footer + 4..] != CAPSULE_MAGIC[..] {
        return false;
    }
    let mut length = [0u8; 4];
    length.copy_from_slice(&tail[footer..footer + 4]);
    usize::try_from(u32::from_le_bytes(length)).ok() == Some(footer)
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
