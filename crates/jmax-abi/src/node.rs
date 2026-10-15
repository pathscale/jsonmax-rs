//! Compact document nodes and their separately owned backing tables.

use alloc::vec::Vec;

use crate::buffer::{Payload, ValidationProof};
use crate::number::ConvertedNumber;
use crate::error::{Error, ErrorCode, ErrorSource, Result};
use crate::span::{NodeId, Span32, SpanSource, StrRef};

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
    /// Parallel to `nodes`; object keys are attached to their value nodes.
    pub node_metadata: Vec<NodeMeta>,
    /// Maximum number of simultaneously open containers, with scalar depth zero.
    pub max_depth: u16,
}

/// Owner-relative links and cooked references for one preorder node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeMeta {
    pub parent: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub key: Option<StrRef>,
    pub key_span: Option<Span32>,
    pub string: Option<StrRef>,
    pub number_index: Option<u32>,
}

/// Constant-time summary and borrowed access to the parallel node metadata.
///
/// The borrow keeps node references tied to their document, including references
/// obtained from a temporary metadata view. This is an in-process view, not a
/// serialized or foreign-function wire record.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DocumentMetadata<'a> {
    pub root: NodeId,
    pub node_count: u32,
    pub span_count: u32,
    pub container_count: u32,
    pub number_count: u32,
    pub decoded_bytes: u32,
    pub max_depth: u16,
    nodes: &'a [NodeMeta],
}

impl<'a> DocumentMetadata<'a> {
    pub fn get(self, index: usize) -> Option<&'a NodeMeta> { self.nodes.get(index) }
    pub fn first(self) -> Option<&'a NodeMeta> { self.nodes.first() }
    pub fn last(self) -> Option<&'a NodeMeta> { self.nodes.last() }
    pub const fn len(self) -> usize { self.nodes.len() }
    pub const fn is_empty(self) -> bool { self.nodes.is_empty() }
    pub const fn as_slice(self) -> &'a [NodeMeta] { self.nodes }
    pub fn iter(self) -> core::slice::Iter<'a, NodeMeta> { self.nodes.iter() }
}

impl core::ops::Deref for DocumentMetadata<'_> {
    type Target = [NodeMeta];
    fn deref(&self) -> &Self::Target { self.nodes }
}

impl<'a> IntoIterator for DocumentMetadata<'a> {
    type Item = &'a NodeMeta;
    type IntoIter = core::slice::Iter<'a, NodeMeta>;
    fn into_iter(self) -> Self::IntoIter { self.iter() }
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

impl ValidatedDocument {
    pub fn payload(&self) -> &Payload { &self.payload }
    pub fn parts(&self) -> &DocumentParts { &self.parts }
    pub const fn proof(&self) -> ValidationProof { self.proof }
    pub const fn root(&self) -> NodeId { NodeId(0) }
    pub fn node_count(&self) -> usize { self.parts.nodes.len() }

    /// Counters saturate if a caller has broken the public tables' u32 limits.
    /// Such a document is rejected by `validate_layout` before conversion.
    pub fn metadata(&self) -> DocumentMetadata<'_> {
        let count = |len| u32::try_from(len).unwrap_or(u32::MAX);
        DocumentMetadata {
            root: self.root(),
            node_count: count(self.parts.nodes.len()),
            span_count: count(self.parts.spans.len()),
            container_count: count(self.parts.containers.len()),
            number_count: count(self.parts.numbers.len()),
            decoded_bytes: count(self.parts.decoded.len()),
            max_depth: self.parts.max_depth,
            nodes: &self.parts.node_metadata,
        }
    }

    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        let storage = match reference.source {
            SpanSource::RawPayload => self.payload.as_slice(),
            SpanSource::DecodedArena => &self.parts.decoded,
        };
        core::str::from_utf8(reference.span.slice(storage)?).ok()
    }

    /// DOM and ABI documents share one representation and one ownership model.
    pub fn into_validated(self) -> Self { self }

    /// Checks owner lengths and the DOM table layout without copying buffers.
    /// The caller must preserve validation invariants when editing public data.
    pub fn from_validated(document: Self) -> Result<Self> {
        document.validate_layout()?;
        Ok(document)
    }

    /// Rejects inconsistent public tables before constructing navigation views.
    /// This checks the representation; it does not issue a new syntax or schema
    /// proof for payload bytes modified after validation.
    pub fn validate_layout(&self) -> Result<()> {
        let invalid = || Error::new(ErrorCode::ConstraintViolation, ErrorSource::Dom, 0,
            "document tables or proof lengths are inconsistent");
        let parts = &self.parts;
        let count = u32::try_from(parts.nodes.len()).map_err(|_| invalid())?;
        if count == 0
            || parts.spans.len() != parts.nodes.len()
            || parts.node_metadata.len() != parts.nodes.len()
            || usize::try_from(self.proof.payload_len()).ok() != Some(self.payload.len())
            || usize::try_from(self.proof.decoded_len()).ok() != Some(parts.decoded.len())
            || u32::try_from(parts.containers.len()).is_err()
            || u32::try_from(parts.numbers.len()).is_err()
        {
            return Err(invalid());
        }
        // Each frame records its node, exclusive subtree end, and remaining
        // immediate children. Every node is visited once, independent of depth.
        let mut stack: Vec<(u32, u32, u32)> = Vec::new();
        let mut containers = 0usize;
        let mut depth = 0u16;
        for (index, word) in parts.nodes.iter().copied().enumerate() {
            let id = index as u32;
            while stack.last().is_some_and(|frame| frame.1 == id) {
                if stack.pop().is_some_and(|frame| frame.2 != 0) { return Err(invalid()); }
            }
            let meta = parts.node_metadata[index];
            let span = parts.spans[index];
            if span.is_empty() || span.slice(self.payload.as_slice()).is_none() || word.kind() > 6 {
                return Err(invalid());
            }
            let end = if matches!(word.kind(), 5 | 6) {
                let record = parts.containers.get(word.payload_index() as usize).ok_or_else(invalid)?;
                if word.payload_index() as usize != containers || record.reserved != 0
                    || record.end_node <= id || record.end_node > count
                    || (record.child_count == 0 && (record.first_child != u32::MAX || record.end_node != id + 1))
                    || (record.child_count != 0 && record.first_child != id + 1)
                { return Err(invalid()); }
                containers += 1;
                record.end_node
            } else {
                if word.payload_index() != id { return Err(invalid()); }
                id + 1
            };
            match stack.last_mut() {
                Some(frame) => {
                    if frame.2 == 0 || id >= frame.1 || end > frame.1
                        || meta.parent != Some(NodeId(frame.0))
                    { return Err(invalid()); }
                    frame.2 -= 1;
                    let next = if frame.2 == 0 { None } else { Some(NodeId(end)) };
                    let parent_span = parts.spans[frame.0 as usize];
                    if meta.next_sibling != next || span.offset <= parent_span.offset
                        || span.end().ok_or_else(invalid)? >= parent_span.end().ok_or_else(invalid)?
                    { return Err(invalid()); }
                    let object = parts.nodes[frame.0 as usize].kind() == 6;
                    if object != meta.key.is_some() || object != meta.key_span.is_some() {
                        return Err(invalid());
                    }
                    if let Some(key_span) = meta.key_span {
                        if key_span.slice(self.payload.as_slice()).is_none()
                            || key_span.offset <= parent_span.offset
                            || key_span.end().ok_or_else(invalid)? > span.offset
                        { return Err(invalid()); }
                    }
                }
                None if id == 0 && end == count && meta.parent.is_none()
                    && meta.next_sibling.is_none() && meta.key.is_none()
                    && meta.key_span.is_none() => {}
                None => return Err(invalid()),
            }
            if (word.kind() == 4) != meta.string.is_some() { return Err(invalid()); }
            for reference in [meta.key, meta.string].into_iter().flatten() {
                let text = self.resolve_str(reference).ok_or_else(invalid)?;
                if text.chars().count() != reference.scalar_len as usize { return Err(invalid()); }
            }
            if let Some(number) = meta.number_index {
                if word.kind() != 3 || parts.numbers.get(number as usize).is_none() { return Err(invalid()); }
            }
            if matches!(word.kind(), 5 | 6) {
                let record = parts.containers[word.payload_index() as usize];
                let next_depth = u16::try_from(stack.len() + 1).map_err(|_| invalid())?;
                depth = depth.max(next_depth);
                stack.try_reserve(1).map_err(|_| Error::new(ErrorCode::BudgetExhausted,
                    ErrorSource::Dom, span.offset, "document layout allocation failed"))?;
                stack.push((id, end, record.child_count));
            }
        }
        if containers != parts.containers.len() || depth != parts.max_depth
            || stack.iter().any(|frame| frame.1 != count || frame.2 != 0)
        { return Err(invalid()); }
        Ok(())
    }
}
