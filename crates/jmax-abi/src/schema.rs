//! Canonical schema intermediate representation, image records, and slots.

use alloc::{boxed::Box, string::String, vec::Vec};

use crate::buffer::{Payload, ValidationProof};
use crate::number::Decimal128;
use crate::span::{Span32, StrRef};
use crate::syntax::ScalarKind;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct SchemaId(pub u32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct StateId(pub u32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct FieldId(pub u32);

/// Operations understood by the schema virtual machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Opcode {
    EnterObject,
    LeaveObject,
    EnterArray,
    LeaveArray,
    MatchKeySwar,
    MatchKeyPhf,
    CheckScalarKind,
    CheckIntBounds,
    CheckDecimalBounds,
    CheckStringLen,
    ExecPattern,
    ExecFormat,
    EmitSlot,
    RejectUnknown,
}

/// Fixed-width instruction record. Validate the opcode before interpreting it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct InsnWord {
    pub opcode: u8,
    pub flags: u8,
    pub field_id: u16,
    pub arg0: u32,
    pub arg1: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct KeyEntry {
    pub swar_head: u64,
    pub span: Span32,
    pub field_id: u16,
    pub required_bit: u16,
    pub target_state: u32,
}

/// In-memory schema image header.
///
/// C layout may include padding; serializers must encode fields explicitly
/// rather than copying the struct's memory representation.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SchemaImageHeader {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,
    pub schema_id: u32,
    pub root_state: u32,
    pub insn_count: u32,
    pub key_count: u32,
    pub slot_count: u32,
    pub fingerprint: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SchemaIr {
    pub name: String,
    pub schema_id: SchemaId,
    pub required_count: u16,
    pub reject_unknown: bool,
    pub fields: Vec<SchemaFieldIr>,
}

#[derive(Clone, Debug)]
pub struct SchemaFieldIr {
    pub field_id: FieldId,
    pub name: String,
    pub required: bool,
    pub scalar_kind: Option<ScalarKind>,
    pub format: Option<String>,
    pub min_len: Option<u32>,
    pub max_len: Option<u32>,
    pub min_i64: Option<i64>,
    pub max_i64: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileLimits {
    pub max_states: u32,
    pub max_insns: u32,
    pub max_keys: u32,
    pub max_pattern_states: u32,
}

impl CompileLimits {
    /// Bound compilation to 65,536 states, 1,048,576 instructions,
    /// 65,536 keys, and 4,096 pattern states.
    pub const fn default_limits() -> Self {
        Self {
            max_states: 65_536,
            max_insns: 1_048_576,
            max_keys: 65_536,
            max_pattern_states: 4_096,
        }
    }
}

/// Owned schema image. Construction alone does not validate its contents.
#[derive(Clone, Debug)]
pub struct SchemaImage {
    pub header: SchemaImageHeader,
    pub insns: Vec<InsnWord>,
    pub keys: Vec<KeyEntry>,
    pub key_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub field_ids: Vec<FieldId>,
    pub strict_validate_unselected: bool,
}

/// A materialized schema slot; floating-point equality compares raw bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotValue {
    Empty,
    Bool(bool),
    U64(u64),
    I64(i64),
    F64Bits(u64),
    Decimal(Decimal128),
    Str(StrRef),
    RawSpan(Span32),
}

/// Validated slots with the storage referenced by their spans.
///
/// Mutating these public fields requires preserving the proof's invariants
/// before handing the slots to consumers that rely on the proof.
#[derive(Clone, Debug)]
pub struct ValidatedSlots {
    pub payload: Payload,
    pub decoded: Box<[u8]>,
    pub slots: Vec<SlotValue>,
    pub proof: ValidationProof,
}
