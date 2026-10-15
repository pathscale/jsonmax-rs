//! Compact document nodes and their separately owned backing tables.

use alloc::vec::Vec;

use crate::buffer::{Payload, ValidationProof};
use crate::number::ConvertedNumber;
use crate::span::Span32;

/// A packed node: kind in bits 0..8, auxiliary data in bits 8..32,
/// and a payload-table index in bits 32..64.
///
/// These are integer bit positions, independent of host byte order. This
/// representation alone does not specify a serialized byte order.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct NodeWord(pub u64);

impl NodeWord {
    /// Pack a node, retaining only the low 24 bits of `aux24`.
    pub const fn pack(kind: u8, aux24: u32, payload_index: u32) -> Self {
        Self((kind as u64) | (((aux24 & 0x00ff_ffff) as u64) << 8)
            | ((payload_index as u64) << 32))
    }

    pub const fn kind(self) -> u8 {
        self.0 as u8
    }

    pub const fn aux24(self) -> u32 {
        ((self.0 >> 8) & 0x00ff_ffff) as u32
    }

    pub const fn payload_index(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Indices and child count for a container node.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Container32 {
    pub first_child: u32,
    pub child_count: u32,
    pub end_node: u32,
    pub reserved: u32,
}

/// Owned tables backing a document's compact nodes.
#[derive(Clone, Debug, Default)]
pub struct DocumentParts {
    pub nodes: Vec<NodeWord>,
    pub spans: Vec<Span32>,
    pub containers: Vec<Container32>,
    pub numbers: Vec<ConvertedNumber>,
    pub decoded: Vec<u8>,
}

/// A document together with its payload and validation provenance.
///
/// Code that changes the public payload or parts must preserve the proof's
/// invariants before passing the document to consumers that rely on them.
#[derive(Clone, Debug)]
pub struct ValidatedDocument {
    pub payload: Payload,
    pub parts: DocumentParts,
    pub proof: ValidationProof,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn node_layout_and_const_accessors_are_stable() {
        const WORD: NodeWord = NodeWord::pack(0x12, 0x345678, 0x9abcdef0);
        const KIND: u8 = WORD.kind();
        const AUX: u32 = WORD.aux24();
        const INDEX: u32 = WORD.payload_index();
        assert_eq!(WORD.0, 0x9abcdef0_34567812);
        assert_eq!((KIND, AUX, INDEX), (0x12, 0x345678, 0x9abcdef0));
        assert_eq!(size_of::<NodeWord>(), size_of::<u64>());
        assert_eq!(align_of::<NodeWord>(), align_of::<u64>());
        assert_eq!(NodeWord::default(), NodeWord::pack(0, 0, 0));
    }

    #[test]
    fn packing_masks_auxiliary_bits_without_corrupting_other_fields() {
        for kind in [0, 1, 127, u8::MAX] {
            for aux in [0, 1, 0x0080_0000, 0x00ff_ffff, 0xff00_0000, u32::MAX] {
                for index in [0, 1, 0x8000_0000, u32::MAX] {
                    let word = NodeWord::pack(kind, aux, index);
                    assert_eq!(word.kind(), kind);
                    assert_eq!(word.aux24(), aux & 0x00ff_ffff);
                    assert_eq!(word.payload_index(), index);
                }
            }
        }
        assert_eq!(NodeWord::pack(u8::MAX, u32::MAX, u32::MAX).0, u64::MAX);
    }

    #[test]
    fn every_raw_bit_round_trips() {
        for bit in 0..64 {
            let word = NodeWord(1u64 << bit);
            assert_eq!(NodeWord::pack(word.kind(), word.aux24(), word.payload_index()), word);
        }
    }

    #[test]
    fn container_has_four_zero_initialized_words() {
        assert_eq!(size_of::<Container32>(), 16);
        assert_eq!(align_of::<Container32>(), align_of::<u32>());
        assert_eq!(Container32::default(), Container32 {
            first_child: 0, child_count: 0, end_node: 0, reserved: 0,
        });
    }

    #[test]
    fn document_tables_start_empty_and_clone_independently() {
        let mut parts = DocumentParts::default();
        assert!(parts.nodes.is_empty());
        assert!(parts.spans.is_empty());
        assert!(parts.containers.is_empty());
        assert!(parts.numbers.is_empty());
        assert!(parts.decoded.is_empty());
        parts.nodes.push(NodeWord::pack(1, 2, 3));
        parts.spans.push(Span32::new(4, 5));
        parts.containers.push(Container32::default());
        parts.numbers.push(ConvertedNumber::U64(6));
        parts.decoded.extend_from_slice(b"value");
        let cloned = parts.clone();
        parts.nodes.clear();
        parts.spans.clear();
        parts.containers.clear();
        parts.numbers.clear();
        parts.decoded.clear();
        assert_eq!(cloned.nodes, [NodeWord::pack(1, 2, 3)]);
        assert_eq!(cloned.spans, [Span32::new(4, 5)]);
        assert_eq!(cloned.containers, [Container32::default()]);
        assert_eq!(cloned.numbers, [ConvertedNumber::U64(6)]);
        assert_eq!(cloned.decoded, b"value");
    }

    #[test]
    fn validated_document_clone_retains_payload_and_proof() {
        // A projection of the valid JSON literal null retains no nodes or
        // spans. Fingerprint 0 identifies this test's null projection schema.
        // SAFETY: The literal is four bytes, its projection has empty tables,
        // and no decoded arena or references require further validation.
        let proof = unsafe { ValidationProof::issue_unchecked(0, 4, 0) };
        let document = ValidatedDocument {
            payload: Payload::from_slice(b"null"), parts: DocumentParts::default(), proof,
        };
        let cloned = document.clone();
        assert_eq!(cloned.payload.as_slice().as_ptr(), document.payload.as_slice().as_ptr());
        assert_eq!(cloned.proof, document.proof);
        drop(document);
        assert_eq!(cloned.payload.as_slice(), b"null");
        assert_eq!(cloned.proof.payload_len(), 4);
        assert_eq!(cloned.proof.decoded_len(), 0);
        assert!(cloned.parts.nodes.is_empty());
        assert!(cloned.parts.decoded.is_empty());
    }
}
