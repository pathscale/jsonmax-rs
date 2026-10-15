//! Slot writes and the final ownership boundary for schema validation proofs.
//!
//! Selection affects capture only. The interpreter validates the whole document
//! before handing this module its private completed-run capability.
use super::*;

/// A request-local selection bitset indexed by compiled slot, not field order.
/// Construction and its retained storage are charged once at VM entry.
#[derive(Clone, Debug)]
pub struct SelectionIndex {
    pub(crate) all: bool,
    pub(crate) words: Vec<u64>,
}

impl SelectionIndex {
    /// Validate IDs and duplicates once and prepare constant-time membership.
    pub fn new(program: &ValidatedProgram, selection: &Selection) -> Result<Self> {
        let mut budget = VmBudget::new(VmLimits {
            syntax: Limits::permissive_default(), max_steps: u64::MAX,
            max_scratch_bytes: u32::MAX,
        });
        execute::selection_index(program, selection, &mut budget)
            .map_err(|error| Error { schema_id: program.image().header.schema_id, ..error })
    }
}

/// Constant-time capture membership after field-to-slot resolution.
pub fn selected(selection: &SelectionIndex, slot: u32) -> bool {
    selection.all || selection.words.get(slot as usize / 64)
        .is_some_and(|word| word & (1u64 << (slot % 64)) != 0)
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
