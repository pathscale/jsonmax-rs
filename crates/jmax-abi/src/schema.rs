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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SpanSource;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn identifiers_preserve_u32_layout_and_values() {
        assert_eq!(size_of::<SchemaId>(), 4);
        assert_eq!(size_of::<StateId>(), 4);
        assert_eq!(size_of::<FieldId>(), 4);
        assert_eq!(align_of::<SchemaId>(), align_of::<u32>());
        assert_eq!(align_of::<StateId>(), align_of::<u32>());
        assert_eq!(align_of::<FieldId>(), align_of::<u32>());
        assert_eq!(SchemaId::default(), SchemaId(0));
        assert_eq!(StateId::default(), StateId(0));
        assert_eq!(FieldId::default(), FieldId(0));
        assert_eq!(SchemaId(u32::MAX).0, u32::MAX);
        assert_eq!(StateId(u32::MAX).0, u32::MAX);
        assert_eq!(FieldId(u32::MAX).0, u32::MAX);
    }

    #[test]
    fn opcode_numbers_follow_the_frozen_order() {
        let opcodes = [
            Opcode::EnterObject, Opcode::LeaveObject, Opcode::EnterArray,
            Opcode::LeaveArray, Opcode::MatchKeySwar, Opcode::MatchKeyPhf,
            Opcode::CheckScalarKind, Opcode::CheckIntBounds,
            Opcode::CheckDecimalBounds, Opcode::CheckStringLen,
            Opcode::ExecPattern, Opcode::ExecFormat, Opcode::EmitSlot,
            Opcode::RejectUnknown,
        ];
        for (index, opcode) in opcodes.into_iter().enumerate() {
            assert_eq!(opcode as usize, index);
        }
    }

    #[test]
    fn image_records_have_the_expected_field_offsets() {
        assert_eq!(size_of::<InsnWord>(), 16);
        assert_eq!(offset_of!(InsnWord, opcode), 0);
        assert_eq!(offset_of!(InsnWord, flags), 1);
        assert_eq!(offset_of!(InsnWord, field_id), 2);
        assert_eq!(offset_of!(InsnWord, arg0), 4);
        assert_eq!(offset_of!(InsnWord, arg1), 8);
        assert_eq!(size_of::<KeyEntry>(), 24);
        assert_eq!(offset_of!(KeyEntry, swar_head), 0);
        assert_eq!(offset_of!(KeyEntry, span), 8);
        assert_eq!(offset_of!(KeyEntry, field_id), 16);
        assert_eq!(offset_of!(KeyEntry, required_bit), 18);
        assert_eq!(offset_of!(KeyEntry, target_state), 20);
        assert_eq!(offset_of!(SchemaImageHeader, magic), 0);
        assert_eq!(offset_of!(SchemaImageHeader, version), 4);
        assert_eq!(offset_of!(SchemaImageHeader, flags), 6);
        assert_eq!(offset_of!(SchemaImageHeader, schema_id), 8);
        assert_eq!(offset_of!(SchemaImageHeader, root_state), 12);
        assert_eq!(offset_of!(SchemaImageHeader, insn_count), 16);
        assert_eq!(offset_of!(SchemaImageHeader, key_count), 20);
        assert_eq!(offset_of!(SchemaImageHeader, slot_count), 24);
        // The final u64 follows the target's C alignment, not a wire offset.
        let alignment = align_of::<u64>();
        let fingerprint_offset = (28 + alignment - 1) & !(alignment - 1);
        assert_eq!(offset_of!(SchemaImageHeader, fingerprint), fingerprint_offset);
        assert_eq!(size_of::<SchemaImageHeader>(), fingerprint_offset + 8);
    }

    #[test]
    fn defaults_are_empty_and_zero_initialized() {
        assert_eq!(InsnWord::default(), InsnWord {
            opcode: 0, flags: 0, field_id: 0, arg0: 0, arg1: 0,
        });
        assert_eq!(KeyEntry::default(), KeyEntry {
            swar_head: 0, span: Span32::new(0, 0), field_id: 0,
            required_bit: 0, target_state: 0,
        });
        assert_eq!(SchemaImageHeader::default(), SchemaImageHeader {
            magic: 0, version: 0, flags: 0, schema_id: 0, root_state: 0,
            insn_count: 0, key_count: 0, slot_count: 0, fingerprint: 0,
        });
        let ir = SchemaIr::default();
        assert!(ir.name.is_empty());
        assert_eq!(ir.schema_id, SchemaId(0));
        assert_eq!(ir.required_count, 0);
        assert!(!ir.reject_unknown);
        assert!(ir.fields.is_empty());
        let selection = Selection::default();
        assert!(selection.field_ids.is_empty());
        assert!(!selection.strict_validate_unselected);
    }

    #[test]
    fn compile_limits_are_const_and_bounded() {
        const LIMITS: CompileLimits = CompileLimits::default_limits();
        assert_eq!(LIMITS, CompileLimits {
            max_states: 65_536, max_insns: 1_048_576,
            max_keys: 65_536, max_pattern_states: 4_096,
        });
    }

    #[test]
    fn schema_ir_clone_preserves_constraints_and_owns_strings() {
        let mut ir = SchemaIr {
            name: String::from("trade"), schema_id: SchemaId(9),
            required_count: 1, reject_unknown: true,
            fields: alloc::vec![SchemaFieldIr {
                field_id: FieldId(7), name: String::from("quantity"), required: true,
                scalar_kind: Some(ScalarKind::Number), format: Some(String::from("int64")),
                min_len: Some(1), max_len: Some(20),
                min_i64: Some(i64::MIN), max_i64: Some(i64::MAX),
            }],
        };
        let cloned = ir.clone();
        ir.name.clear();
        ir.fields[0].name.clear();
        ir.fields[0].format.as_mut().unwrap().clear();
        assert_eq!(cloned.name, "trade");
        assert_eq!(cloned.schema_id, SchemaId(9));
        assert_eq!(cloned.required_count, 1);
        assert!(cloned.reject_unknown);
        let field = &cloned.fields[0];
        assert_eq!(field.field_id, FieldId(7));
        assert_eq!(field.name, "quantity");
        assert!(field.required);
        assert_eq!(field.scalar_kind, Some(ScalarKind::Number));
        assert_eq!(field.format.as_deref(), Some("int64"));
        assert_eq!((field.min_len, field.max_len), (Some(1), Some(20)));
        assert_eq!((field.min_i64, field.max_i64), (Some(i64::MIN), Some(i64::MAX)));
    }

    #[test]
    fn image_and_selection_clone_own_their_tables() {
        let mut image = SchemaImage {
            header: SchemaImageHeader { insn_count: 1, key_count: 1, ..SchemaImageHeader::default() },
            insns: alloc::vec![InsnWord::default()],
            keys: alloc::vec![KeyEntry { span: Span32::new(0, 3), ..KeyEntry::default() }],
            key_bytes: b"key".to_vec(),
        };
        let cloned = image.clone();
        image.insns.clear();
        image.keys.clear();
        image.key_bytes.clear();
        assert_eq!(cloned.header.insn_count, 1);
        assert_eq!(cloned.header.key_count, 1);
        assert_eq!(cloned.insns, [InsnWord::default()]);
        assert_eq!(cloned.keys[0].span.slice(&cloned.key_bytes), Some(&b"key"[..]));
        let mut selection = Selection {
            field_ids: alloc::vec![FieldId(4), FieldId(8)], strict_validate_unselected: true,
        };
        let cloned = selection.clone();
        selection.field_ids.clear();
        assert_eq!(cloned.field_ids, [FieldId(4), FieldId(8)]);
        assert!(cloned.strict_validate_unselected);
    }

    #[test]
    fn slot_values_preserve_types_and_bitwise_equality() {
        let values = [
            SlotValue::Empty, SlotValue::Bool(false), SlotValue::Bool(true),
            SlotValue::U64(u64::MAX), SlotValue::I64(i64::MIN),
            SlotValue::F64Bits(0x7ff8_0000_0000_0001),
            SlotValue::Decimal(Decimal128::new(i128::MAX, 8)),
            SlotValue::Str(StrRef {
                span: Span32::new(1, 2), source: SpanSource::RawPayload, scalar_len: 2,
            }),
            SlotValue::RawSpan(Span32::new(1, 2)),
        ];
        for (index, value) in values.iter().enumerate() {
            assert_eq!(*value, values[index]);
            for other in &values[index + 1..] {
                assert_ne!(value, other);
            }
        }
        assert_ne!(SlotValue::F64Bits(0), SlotValue::F64Bits(1 << 63));
        assert_ne!(SlotValue::U64(1), SlotValue::I64(1));
    }

    #[test]
    fn validated_slots_clone_retains_independent_decoded_storage() {
        let string_ref = StrRef {
            span: Span32::new(0, 2), source: SpanSource::DecodedArena, scalar_len: 1,
        };
        // Fingerprint 7 identifies this test's single-string schema.
        // SAFETY: The eight-byte JSON literal decodes to one Unicode scalar
        // occupying two UTF-8 bytes. The only slot covers that exact string.
        let proof = unsafe { ValidationProof::issue_unchecked(7, 8, 2) };
        let slots = ValidatedSlots {
            payload: Payload::from_slice(b"\"\\u00e9\""),
            decoded: "é".as_bytes().to_vec().into_boxed_slice(),
            slots: alloc::vec![SlotValue::Str(string_ref)], proof,
        };
        let cloned = slots.clone();
        assert_eq!(cloned.payload.as_slice().as_ptr(), slots.payload.as_slice().as_ptr());
        assert_ne!(cloned.decoded.as_ptr(), slots.decoded.as_ptr());
        assert_eq!(cloned.slots, slots.slots);
        assert_eq!(cloned.proof, slots.proof);
        drop(slots);
        assert_eq!(cloned.payload.as_slice(), b"\"\\u00e9\"");
        assert_eq!(&*cloned.decoded, "é".as_bytes());
        assert_eq!(string_ref.span.slice(&cloned.decoded), Some("é".as_bytes()));
        assert_eq!(cloned.proof.payload_len() as usize, cloned.payload.len());
        assert_eq!(cloned.proof.decoded_len() as usize, cloned.decoded.len());
    }
}
