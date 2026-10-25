//! Immutable packed JSON tape with owned and borrowed input facades.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub mod build;
mod legacy;
pub mod navigate;
pub mod rebuild;
pub mod scalar;

pub use build::DomScratch;
pub use jmax_abi::buffer::{Payload, ValidationCapability, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::node::{
    Container32, DocumentMetadata, DocumentParts, NodeMeta, NodeWord, ValidatedDocument,
};
pub use jmax_abi::node::{
    TAPE_AUX_LEN_MASK, TAPE_FLAG_ASCII, TAPE_FLAG_ESCAPED, TAPE_FLAG_EXTENDED, TAPE_FLAG_HAS_KEY,
    TAPE_FLAG_KEY_EXTENDED, TAPE_TAG_KEY, TapeWord,
};
pub use jmax_abi::number::ConvertedNumber;
pub use jmax_abi::span::{NodeId, Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::Limits;
pub use jmax_abi::traits::ByteSink;

pub const NO_NODE: u32 = u32::MAX;
pub const TAG_NULL: u8 = 0;
pub const TAG_FALSE: u8 = 1;
pub const TAG_TRUE: u8 = 2;
pub const TAG_NUMBER: u8 = 3;
pub const TAG_STRING: u8 = 4;
pub const TAG_ARRAY: u8 = 5;
pub const TAG_OBJECT: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}
impl NodeKind {
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            TAG_NULL => Some(Self::Null),
            TAG_FALSE | TAG_TRUE => Some(Self::Bool),
            TAG_NUMBER => Some(Self::Number),
            TAG_STRING => Some(Self::String),
            TAG_ARRAY => Some(Self::Array),
            TAG_OBJECT => Some(Self::Object),
            _ => None,
        }
    }
}

/// Final storage consists of the tape and actual escaped string output only.
/// Input ownership and validation evidence cannot be mutated through this API.
#[derive(Clone, Debug)]
pub struct TapeDocument {
    payload: Payload,
    tape: Vec<TapeWord>,
    decoded: Vec<u8>,
    node_count: u32,
    container_count: u32,
    max_depth: u16,
    proof: ValidationProof,
    capability: ValidationCapability,
}
pub type Document = TapeDocument;

/// Borrows the original input allocation; unescaped strings are never copied.
#[derive(Clone, Debug)]
pub struct BorrowedDocument<'a> {
    raw: &'a [u8],
    tape: Vec<TapeWord>,
    decoded: Vec<u8>,
    node_count: u32,
    container_count: u32,
    max_depth: u16,
    proof: ValidationProof,
    capability: ValidationCapability,
}

/// Constant-time counters, without a parallel per-node metadata allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeMetadata {
    pub root: NodeId,
    pub node_count: u32,
    pub container_count: u32,
    pub decoded_bytes: u32,
    pub tape_words: u32,
    pub max_depth: u16,
}

/// Shared immutable view used by all navigation and scalar operations.
#[derive(Clone, Copy, Debug)]
pub struct TapeView<'a> {
    owner: TapeOwner<'a>,
}
#[derive(Clone, Copy, Debug)]
enum TapeOwner<'a> {
    Owned(&'a TapeDocument),
    Borrowed(&'a BorrowedDocument<'a>),
}

pub trait TapeSource {
    fn as_tape(&self) -> TapeView<'_>;
}
impl TapeSource for TapeDocument {
    fn as_tape(&self) -> TapeView<'_> {
        TapeView {
            owner: TapeOwner::Owned(self),
        }
    }
}
impl TapeSource for BorrowedDocument<'_> {
    fn as_tape(&self) -> TapeView<'_> {
        TapeView {
            owner: TapeOwner::Borrowed(self),
        }
    }
}
impl TapeSource for TapeView<'_> {
    fn as_tape(&self) -> TapeView<'_> {
        *self
    }
}

macro_rules! document_methods {
    () => {
        pub fn as_tape(&self) -> TapeView<'_> {
            TapeSource::as_tape(self)
        }
        pub const fn root(&self) -> NodeId {
            NodeId(0)
        }
        pub fn raw(&self) -> &[u8] {
            self.as_tape().raw()
        }
        pub fn tape(&self) -> &[TapeWord] {
            &self.tape
        }
        pub fn decoded(&self) -> &[u8] {
            &self.decoded
        }
        pub const fn proof(&self) -> ValidationProof {
            self.proof
        }
        pub const fn capability(&self) -> ValidationCapability {
            self.capability
        }
        pub const fn node_count(&self) -> usize {
            self.node_count as usize
        }
        pub const fn container_count(&self) -> usize {
            self.container_count as usize
        }
        pub const fn max_depth(&self) -> u16 {
            self.max_depth
        }
        pub fn metadata(&self) -> TapeMetadata {
            self.as_tape().metadata()
        }
        pub fn root_node(&self) -> Node<'_> {
            navigate::root(self)
        }
        pub fn nodes(&self) -> Nodes<'_> {
            self.as_tape().nodes()
        }
        pub fn node(&self, id: NodeId) -> Option<Node<'_>> {
            navigate::node(self, id)
        }
        pub fn node_kind(&self, id: NodeId) -> Option<NodeKind> {
            self.node(id).map(navigate::kind)
        }
        pub fn node_raw_span(&self, id: NodeId) -> Option<Span32> {
            self.node(id).map(navigate::raw_span)
        }
        pub fn node_string_ref(&self, id: NodeId) -> Option<StrRef> {
            self.node(id).and_then(navigate::string_ref)
        }
        pub fn node_key_ref(&self, id: NodeId) -> Option<StrRef> {
            self.node(id).and_then(navigate::key_ref)
        }
        pub fn node_key_span(&self, id: NodeId) -> Option<Span32> {
            self.node(id).and_then(navigate::key_span)
        }
        pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
            self.as_tape().resolve_str(reference)
        }
        /// Bounds/UTF-8 checked for arbitrary caller-created references. Scalar
        /// counts are trusted from parsing and are never recomputed here.
        pub fn resolve_trusted_str(&self, reference: StrRef) -> Result<&str> {
            self.as_tape().resolve_trusted_str(reference)
        }
    };
}
impl TapeDocument {
    document_methods!();
    pub fn payload(&self) -> &Payload {
        &self.payload
    }
    pub fn to_legacy(&self) -> Result<ValidatedDocument> {
        Ok(ValidatedDocument {
            payload: self.payload.clone(),
            parts: legacy::parts(self.as_tape(), true)?,
            proof: self.proof,
        })
    }
    pub fn into_legacy(self) -> Result<ValidatedDocument> {
        let mut parts = legacy::parts(self.as_tape(), false)?;
        parts.decoded = self.decoded;
        Ok(ValidatedDocument {
            payload: self.payload,
            parts,
            proof: self.proof,
        })
    }
    pub fn to_validated(&self) -> Result<ValidatedDocument> {
        self.to_legacy()
    }
    pub fn from_validated(document: ValidatedDocument, limits: Limits) -> Result<Self> {
        build::from_validated(document, limits)
    }
    /// Returns the final buffers to scratch for later allocation-free reuse.
    pub fn recycle(self, scratch: &mut DomScratch) {
        scratch.reclaim_owned(self);
    }
}
impl BorrowedDocument<'_> {
    document_methods!();
    #[inline]
    pub fn recycle(self, scratch: &mut DomScratch) {
        scratch.reclaim_borrowed(self);
    }
}

impl<'a> TapeView<'a> {
    #[inline]
    pub fn raw(self) -> &'a [u8] {
        match self.owner {
            TapeOwner::Owned(doc) => doc.payload.as_slice(),
            TapeOwner::Borrowed(doc) => doc.raw,
        }
    }
    #[inline]
    pub fn tape(self) -> &'a [TapeWord] {
        match self.owner {
            TapeOwner::Owned(doc) => &doc.tape,
            TapeOwner::Borrowed(doc) => &doc.tape,
        }
    }
    #[inline]
    pub fn decoded(self) -> &'a [u8] {
        match self.owner {
            TapeOwner::Owned(doc) => &doc.decoded,
            TapeOwner::Borrowed(doc) => &doc.decoded,
        }
    }
    #[inline]
    pub const fn root(self) -> NodeId {
        NodeId(0)
    }
    #[inline]
    pub fn root_node(self) -> Node<'a> {
        Node {
            document: self,
            id: NodeId(0),
        }
    }
    #[inline]
    pub fn nodes(self) -> Nodes<'a> {
        Nodes {
            document: self,
            cursor: 0,
            remaining: self.node_count() as u32,
        }
    }
    #[inline]
    pub fn node(self, id: NodeId) -> Option<Node<'a>> {
        navigate::node_in_view(self, id)
    }
    #[inline]
    pub fn proof(self) -> ValidationProof {
        match self.owner {
            TapeOwner::Owned(doc) => doc.proof,
            TapeOwner::Borrowed(doc) => doc.proof,
        }
    }
    #[inline]
    pub fn capability(self) -> ValidationCapability {
        match self.owner {
            TapeOwner::Owned(doc) => doc.capability,
            TapeOwner::Borrowed(doc) => doc.capability,
        }
    }
    #[inline]
    pub fn node_count(self) -> usize {
        match self.owner {
            TapeOwner::Owned(doc) => doc.node_count as usize,
            TapeOwner::Borrowed(doc) => doc.node_count as usize,
        }
    }
    #[inline]
    pub fn container_count(self) -> usize {
        match self.owner {
            TapeOwner::Owned(doc) => doc.container_count as usize,
            TapeOwner::Borrowed(doc) => doc.container_count as usize,
        }
    }
    #[inline]
    pub fn max_depth(self) -> u16 {
        match self.owner {
            TapeOwner::Owned(doc) => doc.max_depth,
            TapeOwner::Borrowed(doc) => doc.max_depth,
        }
    }
    #[inline]
    pub fn metadata(self) -> TapeMetadata {
        TapeMetadata {
            root: self.root(),
            node_count: self.node_count() as u32,
            container_count: self.container_count() as u32,
            decoded_bytes: self.decoded().len() as u32,
            tape_words: self.tape().len() as u32,
            max_depth: self.max_depth(),
        }
    }
    #[inline]
    pub fn resolve_str(self, reference: StrRef) -> Option<&'a str> {
        let storage = match reference.source {
            SpanSource::RawPayload => self.raw(),
            SpanSource::DecodedArena => self.decoded(),
        };
        core::str::from_utf8(reference.span.slice(storage)?).ok()
    }
    #[inline]
    pub fn resolve_trusted_str(self, reference: StrRef) -> Result<&'a str> {
        self.resolve_str(reference).ok_or_else(|| {
            dom_error(
                ErrorCode::InvalidUtf8,
                reference.span.offset,
                "DOM string reference is out of bounds or not UTF-8",
            )
        })
    }
    /// A handle-derived string reference is validated by construction. Kept
    /// private: arbitrary public StrRefs cannot justify unchecked UTF-8.
    pub(crate) fn resolve_record_str(self, reference: StrRef) -> &'a str {
        let storage = match reference.source {
            SpanSource::RawPayload => self.raw(),
            SpanSource::DecodedArena => self.decoded(),
        };
        let bytes =
            &storage[reference.span.offset as usize..reference.span.end().unwrap_or(0) as usize];
        // SAFETY: Only references decoded from genuine private tape records
        // reach here. Strict construction validated their UTF-8 and bounds.
        unsafe { core::str::from_utf8_unchecked(bytes) }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    pub(crate) document: TapeView<'a>,
    pub(crate) id: NodeId,
}
impl<'a> Node<'a> {
    #[inline]
    pub const fn id(self) -> NodeId {
        self.id
    }
    #[inline]
    pub const fn document(self) -> TapeView<'a> {
        self.document
    }
    #[inline]
    pub fn kind(self) -> NodeKind {
        navigate::kind(self)
    }
    #[inline]
    pub fn header(self) -> TapeWord {
        self.document.tape()[self.id.0 as usize]
    }
    #[inline]
    pub fn subtree_end(self) -> u32 {
        navigate::subtree_end(self)
    }
    #[inline]
    pub fn raw_span(self) -> Span32 {
        navigate::raw_span(self)
    }
    #[inline]
    pub fn parent(self) -> Option<Self> {
        navigate::parent(self)
    }
    #[inline]
    pub fn children(self) -> Result<Children<'a>> {
        navigate::children(self)
    }
    #[inline]
    pub fn object(self) -> Result<Object<'a>> {
        navigate::object(self)
    }
    #[inline]
    pub fn array(self) -> Result<Array<'a>> {
        navigate::array(self)
    }
    #[inline]
    pub fn string_ref(self) -> Option<StrRef> {
        navigate::string_ref(self)
    }
    #[inline]
    pub fn key_ref(self) -> Option<StrRef> {
        navigate::key_ref(self)
    }
    #[inline]
    pub fn key_span(self) -> Option<Span32> {
        navigate::key_span(self)
    }
    /// Original quoted key, guaranteed to need no escaping by construction.
    #[inline]
    pub fn unescaped_key_token(self) -> Option<&'a [u8]> {
        navigate::unescaped_key_token(self)
    }
    /// Original quoted string, guaranteed to need no escaping by construction.
    #[inline]
    pub fn unescaped_string_token(self) -> Option<&'a [u8]> {
        navigate::unescaped_string_token(self)
    }
    /// Resolves this value's validated member key without UTF-8 revalidation.
    #[inline]
    pub fn key(self) -> Option<&'a str> {
        self.key_ref()
            .map(|reference| self.document.resolve_record_str(reference))
    }
    #[inline]
    pub fn string(self) -> Result<&'a str> {
        scalar::string(self)
    }
    #[inline]
    pub fn raw(self) -> Result<&'a [u8]> {
        scalar::raw(self)
    }
    #[inline]
    pub fn value(self) -> Result<Scalar<'a>> {
        scalar::value(self)
    }
    #[inline]
    pub fn boolean(self) -> Result<bool> {
        scalar::boolean(self)
    }
    #[inline]
    pub fn u64(self) -> Result<u64> {
        scalar::u64(self)
    }
    #[inline]
    pub fn i64(self) -> Result<i64> {
        scalar::i64(self)
    }
    #[inline]
    pub fn f64(self) -> Result<f64> {
        scalar::f64(self)
    }
    #[inline]
    pub fn converted(self) -> Result<ConvertedNumber> {
        scalar::converted(self)
    }
}
pub type BorrowedNode<'a> = Node<'a>;
/// Preorder value iteration skips key records and never inspects extension words
/// as headers. Each step is O(1), independent of subtree size.
#[derive(Clone, Debug)]
pub struct Nodes<'a> {
    document: TapeView<'a>,
    cursor: u32,
    remaining: u32,
}
impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let mut index = self.cursor as usize;
        if self.document.tape()[index].tag() == TAPE_TAG_KEY {
            index += self.document.tape()[index].record_words();
        }
        self.cursor = (index + self.document.tape()[index].record_words()) as u32;
        self.remaining -= 1;
        Some(Node {
            document: self.document,
            id: NodeId(index as u32),
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining as usize, Some(self.remaining as usize))
    }
}
impl ExactSizeIterator for Nodes<'_> {}
impl core::iter::FusedIterator for Nodes<'_> {}
#[derive(Clone, Copy, Debug)]
pub struct Object<'a> {
    pub(crate) node: Node<'a>,
}
impl<'a> Object<'a> {
    pub const fn node(self) -> Node<'a> {
        self.node
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Array<'a> {
    pub(crate) node: Node<'a>,
}
impl<'a> Array<'a> {
    pub const fn node(self) -> Node<'a> {
        self.node
    }
}
#[derive(Clone, Debug)]
pub struct Children<'a> {
    pub(crate) document: TapeView<'a>,
    pub(crate) next: Option<NodeId>,
    pub(crate) remaining: u32,
    pub(crate) object: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Member<'a> {
    pub key: &'a str,
    pub value: Node<'a>,
}
#[derive(Clone, Debug)]
pub struct Members<'a> {
    pub(crate) children: Children<'a>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scalar<'a> {
    Null,
    Bool(bool),
    Number(&'a [u8]),
    String(&'a str),
}
#[derive(Clone, Debug)]
pub enum EditAction {
    Replace(Payload),
    Remove,
    AppendArray(Payload),
    SetMember { key: String, value: Payload },
}
#[derive(Clone, Debug)]
pub struct Edit {
    pub target: NodeId,
    pub action: EditAction,
}
pub(crate) const fn dom_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Dom, offset, detail)
}
