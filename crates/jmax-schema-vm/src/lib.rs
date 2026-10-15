//! Schema validation state and the ownership boundary for publishing ABI proofs.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
pub use jmax_abi::buffer::{Payload, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{ConvertedNumber, Decimal128};
pub use jmax_abi::schema::{FieldId, Selection, SlotValue, StateId, ValidatedSlots};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::{Event, Limits, ScalarKind};
pub use jmax_schema_compile::{CompiledProgram, ConstraintSet, KeyTable, PatternProgram,
    ProgramState, SlotSpec, StateKind, ValidatedProgram};
pub use jmax_schema_ir::ValueKind;

pub mod execute;
pub mod input { pub use crate::execute::CursorInput; }
pub use execute::CursorInput;
pub mod objects;
pub mod constraints;
pub mod extract;

pub const DEFAULT_MAX_VM_STEPS: u64 = 16_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmLimits {
    pub syntax: Limits,
    pub max_steps: u64,
    /// Includes seen masks, retained unknown keys, frames and slot scratch.
    pub max_scratch_bytes: u32,
}

impl Default for VmLimits {
    fn default() -> Self {
        Self { syntax: Limits::strict_default(), max_steps: DEFAULT_MAX_VM_STEPS,
            max_scratch_bytes: 16 * 1_024 * 1_024 }
    }
}

#[derive(Clone, Debug)]
pub struct InputBuffers { pub payload: Payload, pub decoded: Box<[u8]> }

#[derive(Clone, Copy, Debug)]
pub struct ScalarInput<'a> {
    pub kind: ScalarKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
    pub raw: &'a [u8],
    pub arena: &'a [u8],
}

/// A complete, strictly parsed JSON event source with stable raw bytes and an
/// append-only decoded arena.
///
/// # Safety
/// Implementors must validate UTF-8 and the complete JSON grammar, emit every event
/// in order exactly once, and report EOF only after one root and no trailing data.
/// Every span and decoded scalar count must describe the current buffers. Raw
/// bytes and their allocation remain unchanged during traversal. The decoded arena may append bytes and
/// reallocate during next_event, but all previously exposed bytes and offsets
/// remain unchanged. Accessors expose the current arena after every event.
/// into_buffers returns those exact contents, including all appended bytes;
/// conversion to owned storage may move either allocation.
/// Syntax failures, duplicate policy and all supplied limits must be enforced by
/// the VM or source before publication. An event adapter must never trust caller
/// supplied events or a proof for a different owner to establish these guarantees.
pub unsafe trait EventInput: Sized {
    fn raw_bytes(&self) -> &[u8];
    fn decoded_bytes(&self) -> &[u8];
    fn next_event(&mut self) -> Result<Option<Event>>;
    fn numeric_policy(&self) -> Option<jmax_abi::number::NumericPolicy> { None }
    fn into_buffers(self) -> Result<InputBuffers>;
}

/// Extraction cannot weaken validation. False is a query-only request and is rejected.
pub fn check_selection(program: &ValidatedProgram, selection: &Selection) -> Result<()> {
    extract::SelectionIndex::new(program, selection).map(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerKind { Object, Array }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingValue {
    /// None is an unconstrained unknown field.
    pub state: Option<StateId>,
    pub field_id: FieldId,
    pub slot: Option<u32>,
}

/// Open-addressed decoded-key identity with span-backed entries. Generations
/// allow sibling objects to reuse slots without clearing the previous keys.
#[derive(Clone, Debug)]
pub struct UnknownKeyTable {
    slots: Vec<UnknownKeySlot>,
    len: usize,
    generation: u32,
    /// Used only by standalone objects::key calls, never by event execution.
    arena: Vec<u8>,
    input_backed: bool,
}

#[derive(Clone, Copy, Debug)]
struct UnknownKeySlot {
    hash: u64,
    reference: StrRef,
    generation: u32,
}

impl UnknownKeySlot {
    const EMPTY: Self = Self {
        hash: 0,
        reference: StrRef {
            span: Span32::new(0, 0), source: SpanSource::RawPayload, scalar_len: 0,
        },
        generation: 0,
    };
}

#[derive(Clone, Debug)]
pub struct ObjectState {
    pub table: Option<u32>,
    pub seen_words: Vec<u64>,
    pub unknown_keys: UnknownKeyTable,
    pub property_count: u32,
    pub pending: Option<PendingValue>,
}

#[derive(Clone, Debug)]
pub enum FrameKind {
    Object(ObjectState),
    Array { count: u32, items: Option<StateId> },
}

#[derive(Clone, Debug)]
pub struct VmFrame {
    pub state: Option<StateId>,
    pub field_id: FieldId,
    pub start: u32,
    pub slot: Option<u32>,
    pub kind: FrameKind,
}

/// A completed run is a private capability, not a collection of caller assertions.
#[derive(Debug)]
pub struct ValidatedRun {
    pub(crate) buffers: InputBuffers,
    pub(crate) slots: Vec<SlotValue>,
    pub(crate) fingerprint: u64,
}

impl ValidatedRun {
    pub fn slots(&self) -> &[SlotValue] { &self.slots }
    pub fn payload(&self) -> &Payload { &self.buffers.payload }
    pub fn decoded_bytes(&self) -> &[u8] { &self.buffers.decoded }
    pub fn schema_fingerprint(&self) -> u64 { self.fingerprint }
}

pub(crate) struct VmContext<'a> {
    pub program: &'a ValidatedProgram,
    pub selection: extract::SelectionIndex,
    pub limits: VmLimits,
    pub frames: Vec<VmFrame>,
    /// Vacant while the corresponding depth's object owns its reusable table.
    pub unknown_key_tables: Vec<UnknownKeyTable>,
    pub seen_masks: Vec<Vec<u64>>,
    pub slots: Vec<SlotValue>,
    pub budget: VmBudget,
    pub root_started: bool,
    pub root_done: bool,
}

#[derive(Clone, Debug)]
pub struct VmBudget {
    pub(crate) steps_left: u64,
    pub(crate) scratch_bytes: u32,
    pub(crate) max_scratch_bytes: u32,
}

impl VmBudget {
    pub const fn new(limits: VmLimits) -> Self {
        Self { steps_left: limits.max_steps, scratch_bytes: 0,
            max_scratch_bytes: limits.max_scratch_bytes }
    }
    pub const fn steps_left(&self) -> u64 { self.steps_left }
    pub const fn scratch_bytes(&self) -> u32 { self.scratch_bytes }

    pub fn charge(&mut self, units: u64, offset: u32) -> Result<()> {
        self.steps_left = self.steps_left.checked_sub(units).ok_or_else(||
            vm_error(ErrorCode::BudgetExhausted, offset, "schema VM step budget exhausted"))?;
        Ok(())
    }

    pub fn retain(&mut self, bytes: u32, offset: u32) -> Result<()> {
        let next = self.scratch_bytes.checked_add(bytes).ok_or_else(||
            vm_error(ErrorCode::BudgetExhausted, offset, "schema VM scratch size overflow"))?;
        if next > self.max_scratch_bytes {
            return Err(vm_error(ErrorCode::BudgetExhausted, offset,
                "schema VM scratch limit exceeded"));
        }
        self.scratch_bytes = next;
        Ok(())
    }

    pub fn release(&mut self, bytes: u32) -> Result<()> {
        self.scratch_bytes = self.scratch_bytes.checked_sub(bytes).ok_or_else(||
            vm_error(ErrorCode::InternalError, 0, "schema VM scratch accounting underflow"))?;
        Ok(())
    }
}

/// Resolve a string with checked ranges, UTF-8 validation and exact scalar count.
pub fn resolve_str<'a>(raw: &'a [u8], decoded: &'a [u8], value: StrRef) -> Result<&'a str> {
    let (owner, error_offset) = match value.source {
        SpanSource::RawPayload => (raw, value.span.offset),
        SpanSource::DecodedArena => (decoded, 0),
    };
    let bytes = value.span.slice(owner).ok_or_else(||
        vm_error(ErrorCode::InternalError, error_offset, "string span is out of bounds"))?;
    let text = core::str::from_utf8(bytes).map_err(|_|
        vm_error(ErrorCode::InvalidUtf8, 0, "event string is not valid UTF-8"))?;
    if text.chars().count() != value.scalar_len as usize {
        return Err(vm_error(ErrorCode::InternalError, 0, "incorrect string scalar count"));
    }
    Ok(text)
}

pub(crate) fn vm_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaVm, offset, detail)
}

/// Reusable interpreter tables. Independent scratch values can run concurrently.
#[derive(Default)]
pub struct VmScratch {
    pub(crate) frames: Vec<VmFrame>,
    pub(crate) unknown_key_tables: Vec<UnknownKeyTable>,
    pub(crate) seen_masks: Vec<Vec<u64>>,
    pub(crate) slots: Vec<SlotValue>,
    pub(crate) selection_words: Vec<u64>,
}

/// Complete-input cursor, interpreter scratch and the last validated result.
/// A result borrows the scratch and is unavailable after a failed call.
pub struct ValidationScratch {
    pub(crate) cursor: jmax_cursor::Cursor,
    pub(crate) vm: VmScratch,
    pub(crate) output: Option<ValidatedSlots>,
    pub(crate) ordered: Vec<SlotValue>,
}
impl Default for ValidationScratch {
    fn default() -> Self { Self {
        cursor: jmax_cursor::Cursor::new(Limits::strict_default(), jmax_cursor::SkipMode::Strict),
        vm: VmScratch::default(), output: None, ordered: Vec::new(),
    } }
}
impl ValidationScratch {
    pub fn slots(&self) -> Option<&ValidatedSlots> { self.output.as_ref() }
    /// Reorder selected results using retained storage; the input slots remain
    /// indexed by compiled slot number until the copy is complete.
    pub fn order_selection(&mut self, program: &ValidatedProgram, selection: &Selection) -> Result<&ValidatedSlots> {
        let output = self.output.as_mut().ok_or_else(|| vm_error(
            ErrorCode::InternalError, 0, "no validated output to reorder"))?;
        self.ordered.clear();
        self.ordered.try_reserve(selection.field_ids.len()).map_err(|_| vm_error(
            ErrorCode::BudgetExhausted, 0, "selection allocation failed"))?;
        for field in &selection.field_ids {
            let spec = program.slot_for_field(*field).ok_or_else(|| vm_error(
                ErrorCode::SchemaImageInvalid, 0, "selected field has no slot"))?;
            self.ordered.push(*output.slots.get(spec.slot as usize).ok_or_else(|| vm_error(
                ErrorCode::SchemaImageInvalid, 0, "selected slot is out of bounds"))?);
        }
        core::mem::swap(&mut output.slots, &mut self.ordered);
        Ok(output)
    }
}
