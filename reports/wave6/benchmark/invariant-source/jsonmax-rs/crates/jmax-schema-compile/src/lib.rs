//! Bounded schema programs. ABI instructions and all side tables have explicit owners.
#![no_std]

extern crate alloc;

use alloc::{string::String, sync::Arc, vec::Vec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::{
    CompileLimits, FieldId, InsnWord, KeyEntry, Opcode, SchemaId, SchemaImage, SchemaImageHeader,
    StateId,
};
pub use jmax_abi::span::Span32;
pub use jmax_format::{FormatId, FormatOptions};
pub use jmax_schema_ir::{
    DecimalConstraints, ResolvedSchema, ScalarConstraints, SchemaPolicy, TypeId, ValueKind,
};

pub mod image;
pub mod keys;
pub mod lower;
pub mod patterns;

pub use lower::compile;

pub const IMAGE_MAGIC: u32 = 0x58414d4a;
/// Version two adds the standalone canonical capsule and two-level key tables.
pub const IMAGE_VERSION: u16 = 2;
pub const IMAGE_FLAGS: u16 = 0;
pub const NO_REQUIRED_BIT: u16 = u16::MAX;
pub const EMPTY_KEY_SLOT: u32 = u32::MAX;
pub const DEFAULT_MAX_IMAGE_BYTES: u32 = 64 * 1_024 * 1_024;
pub const PATTERN_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilerOptions {
    pub limits: CompileLimits,
    pub policy: SchemaPolicy,
    pub max_image_bytes: u32,
    pub max_pattern_steps: u32,
    /// Seed-search ceiling per hash level. Exhaustion returns a resource error
    /// when a table cannot use the bounded SWAR strategy.
    pub max_hash_attempts: u32,
    /// Preferred SWAR threshold, capped at keys::MAX_SWAR_KEYS.
    pub swar_threshold: u16,
}

impl Default for CompilerOptions {
    fn default() -> Self {
        Self {
            limits: CompileLimits::default_limits(),
            policy: SchemaPolicy::default(),
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_pattern_steps: 1_000_000,
            max_hash_attempts: 256,
            swar_threshold: 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateKind {
    Any,
    Scalar(ValueKind),
    Object {
        key_table: u32,
        required_count: u16,
        reject_unknown: bool,
    },
    Array {
        items: StateId,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConstraintSet {
    pub min_i64: Option<i64>,
    pub max_i64: Option<i64>,
    pub decimal: Option<DecimalConstraints>,
    /// String scalars, object member count, or array item count, by state kind.
    pub min_count: Option<u32>,
    pub max_count: Option<u32>,
    pub pattern: Option<u32>,
    pub format: Option<FormatId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramState {
    pub id: StateId,
    pub kind: StateKind,
    pub insns: Span32,
    pub constraints: ConstraintSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySpec {
    pub name: String,
    pub field_id: FieldId,
    pub required: bool,
    pub target: StateId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStrategy {
    Swar,
    /// Slots contain global image key indices, or `EMPTY_KEY_SLOT`.
    PerfectHash {
        seed: u64,
        slots: Vec<u32>,
    },
    /// Two-level perfect hashing with one bounded secondary probe.
    BucketedPerfectHash {
        seed: u64,
        buckets: Vec<HashBucket>,
        slots: Vec<u32>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashBucket {
    pub seed: u64,
    pub first_slot: u32,
    pub slot_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyTable {
    pub first_key: u32,
    pub key_count: u32,
    pub strategy: KeyStrategy,
}

/// Construction result has local key indices and spans, rebased once by lowering.
#[derive(Clone, Debug)]
pub struct BuiltKeys {
    pub entries: Vec<KeyEntry>,
    pub bytes: Vec<u8>,
    pub strategy: KeyStrategy,
    pub required_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternOp {
    Char {
        scalar: u32,
        next: u32,
    },
    Class {
        ranges: Vec<(u32, u32)>,
        negated: bool,
        next: u32,
    },
    Any {
        next: u32,
    },
    Split {
        first: u32,
        second: u32,
    },
    Jump {
        next: u32,
    },
    AssertStart {
        next: u32,
    },
    AssertEnd {
        next: u32,
    },
    Accept,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternProgram {
    pub version: u16,
    pub start: u32,
    pub ops: Vec<PatternOp>,
    pub max_steps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotSpec {
    pub slot: u32,
    pub field_id: FieldId,
    pub state: StateId,
}

#[derive(Clone, Debug)]
pub struct CompiledProgram {
    pub image: SchemaImage,
    pub states: Vec<ProgramState>,
    pub key_tables: Vec<KeyTable>,
    pub patterns: Vec<PatternProgram>,
    pub slots: Vec<SlotSpec>,
    pub policy: SchemaPolicy,
    pub format_options: FormatOptions,
    /// Exact canonical schema identity. Fingerprints alone are not cache identities.
    pub canonical_schema: Vec<u8>,
}

/// Immutable, shared program and indexes constructed only by image validation.
/// Cloning shares all tables and does not lower, decode, or copy the program.
#[derive(Clone, Debug)]
pub struct ValidatedProgram {
    pub(crate) program: Arc<CompiledProgram>,
    pub(crate) indexes: Arc<ProgramIndexes>,
}

#[derive(Debug)]
pub(crate) struct ProgramIndexes {
    pub field_slots: Vec<u32>,
    pub table_states: Vec<StateId>,
    pub dispatch: Vec<CompiledStateDispatch>,
    pub parents: Vec<Vec<StateId>>,
    pub required_masks: Vec<Vec<u64>>,
}

/// Immutable dispatch prepared after the canonical bytecode has been validated.
/// Scalar mask bits use `scalar_kind_bit`; container states have a zero mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledStateDispatch {
    pub scalar_mask: u8,
    /// A unique incoming capture slot, if one exists. Shared states may have
    /// multiple incoming slots; field dispatch remains authoritative there.
    pub slot_index: Option<u32>,
    pub has_slots: bool,
    pub has_constraints: bool,
    pub subtree_has_slots: bool,
    pub subtree_has_constraints: bool,
    pub unknown_fields_unconstrained: bool,
}

pub const fn scalar_kind_bit(kind: jmax_abi::syntax::ScalarKind) -> u8 {
    use jmax_abi::syntax::ScalarKind;
    match kind {
        ScalarKind::String => 1,
        ScalarKind::Number => 2,
        ScalarKind::True => 4,
        ScalarKind::False => 8,
        ScalarKind::Null => 16,
    }
}

impl ValidatedProgram {
    pub fn program(&self) -> &CompiledProgram {
        &self.program
    }
    /// Reverse schema edges, prepared once for selection reachability.
    pub fn parents(&self, id: StateId) -> Option<&[StateId]> {
        self.indexes.parents.get(id.0 as usize).map(Vec::as_slice)
    }
    /// Required membership uses the same table-local bits as duplicate tracking.
    pub fn required_mask(&self, table: u32) -> Option<&[u64]> {
        self.indexes
            .required_masks
            .get(table as usize)
            .map(Vec::as_slice)
    }
    pub fn dispatch(&self, id: StateId) -> Option<&CompiledStateDispatch> {
        self.indexes.dispatch.get(id.0 as usize)
    }
    pub fn image(&self) -> &SchemaImage {
        &self.program.image
    }
    pub fn state(&self, id: StateId) -> Option<&ProgramState> {
        self.program.states.get(id.0 as usize)
    }
    /// Constant-time field lookup; field identifiers outside the ABI domain miss.
    pub fn slot_for_field(&self, field: FieldId) -> Option<&SlotSpec> {
        let index = usize::try_from(field.0).ok()?;
        let slot = *self.indexes.field_slots.get(index)?;
        if slot == EMPTY_KEY_SLOT {
            return None;
        }
        self.program.slots.get(slot as usize)
    }
    /// The unique object state owning a key table, indexed at validation time.
    pub fn table_state(&self, table: u32) -> Option<&ProgramState> {
        let index = usize::try_from(table).ok()?;
        self.state(*self.indexes.table_states.get(index)?)
    }
    pub fn into_program(self) -> CompiledProgram {
        match Arc::try_unwrap(self.program) {
            Ok(program) => program,
            Err(program) => (*program).clone(),
        }
    }
}

/// Includes the complete deterministic encoded program to disambiguate hash collisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey {
    pub fingerprint: u64,
    pub image_version: u16,
    pub bytes: Vec<u8>,
}

/// The ABI opcode enum has no representation guarantee. Never cast it to bytes.
pub const fn opcode_byte(opcode: Opcode) -> u8 {
    match opcode {
        Opcode::EnterObject => 1,
        Opcode::LeaveObject => 2,
        Opcode::EnterArray => 3,
        Opcode::LeaveArray => 4,
        Opcode::MatchKeySwar => 5,
        Opcode::MatchKeyPhf => 6,
        Opcode::CheckScalarKind => 7,
        Opcode::CheckIntBounds => 8,
        Opcode::CheckDecimalBounds => 9,
        Opcode::CheckStringLen => 10,
        Opcode::ExecPattern => 11,
        Opcode::ExecFormat => 12,
        Opcode::EmitSlot => 13,
        Opcode::RejectUnknown => 14,
    }
}

pub const fn decode_opcode(byte: u8) -> Option<Opcode> {
    match byte {
        1 => Some(Opcode::EnterObject),
        2 => Some(Opcode::LeaveObject),
        3 => Some(Opcode::EnterArray),
        4 => Some(Opcode::LeaveArray),
        5 => Some(Opcode::MatchKeySwar),
        6 => Some(Opcode::MatchKeyPhf),
        7 => Some(Opcode::CheckScalarKind),
        8 => Some(Opcode::CheckIntBounds),
        9 => Some(Opcode::CheckDecimalBounds),
        10 => Some(Opcode::CheckStringLen),
        11 => Some(Opcode::ExecPattern),
        12 => Some(Opcode::ExecFormat),
        13 => Some(Opcode::EmitSlot),
        14 => Some(Opcode::RejectUnknown),
        _ => None,
    }
}

/// Copy, never overread, and use little-endian packing on every platform.
pub fn swar_head(bytes: &[u8]) -> u64 {
    let mut head = [0u8; 8];
    let len = bytes.len().min(head.len());
    head[..len].copy_from_slice(&bytes[..len]);
    u64::from_le_bytes(head)
}

pub fn key_hash(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = jmax_schema_ir::FINGERPRINT_OFFSET ^ seed;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(jmax_schema_ir::FINGERPRINT_PRIME);
    }
    // Avalanche all bits before masking a power-of-two bucket count. The seed
    // participates in both the byte hash and the final independent mixing.
    hash ^= seed.wrapping_mul(0x9e3779b97f4a7c15);
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
    hash ^ (hash >> 31)
}

pub(crate) fn compile_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, 0, detail)
}
