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

/// A complete, strictly parsed JSON event source tied to immutable owned buffers.
///
/// # Safety
/// Implementors must validate UTF-8 and the complete JSON grammar, emit every event
/// in order exactly once, and report EOF only after one root and no trailing data.
/// Every span and decoded scalar count must describe those exact buffers. Buffers
/// must remain byte-for-byte unchanged, including through `into_buffers`, and that
/// method must return the owners exposed by `raw_bytes` and `decoded_bytes`.
/// Syntax failures, duplicate policy and all supplied limits must be enforced by
/// the VM or source before publication. An event adapter must never trust caller
/// supplied events or a proof for a different owner to establish these guarantees.
pub unsafe trait EventInput: Sized {
    fn raw_bytes(&self) -> &[u8];
    fn decoded_bytes(&self) -> &[u8];
    fn next_event(&mut self) -> Result<Option<Event>>;
    fn into_buffers(self) -> Result<InputBuffers>;
}

/// Extraction cannot weaken validation. False is a query-only request and is rejected.
pub fn check_selection(program: &ValidatedProgram, selection: &Selection) -> Result<()> {
    if !selection.strict_validate_unselected {
        return Err(vm_error(ErrorCode::UnsupportedSchemaFeature, 0,
            "proof publication requires validation of unselected fields")
            .with_schema(program.image().header.schema_id, 0));
    }
    for (index, field) in selection.field_ids.iter().enumerate() {
        if selection.field_ids[..index].contains(field) {
            return Err(vm_error(ErrorCode::ConstraintViolation, 0,
                "selection contains a duplicate field")
                .with_schema(program.image().header.schema_id, field.0));
        }
        if !program.program().slots.iter().any(|slot| slot.field_id == *field) {
            return Err(vm_error(ErrorCode::UnknownObjectKey, 0,
                "selection field has no compiled slot").with_schema(
                    program.image().header.schema_id, field.0));
        }
    }
    Ok(())
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

#[derive(Clone, Debug)]
pub struct ObjectState {
    pub table: Option<u32>,
    pub seen_words: Vec<u64>,
    pub unknown_keys: Vec<Vec<u8>>,
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
    pub selection: &'a Selection,
    pub limits: VmLimits,
    pub frames: Vec<VmFrame>,
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
