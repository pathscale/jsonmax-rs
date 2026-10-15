//! Immutable flat JSON documents with owner-bound navigation and scalar views.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub mod build;
pub mod navigate;
pub mod scalar;
pub mod rebuild;

pub use jmax_abi::buffer::{Payload, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::node::{Container32, DocumentMetadata, DocumentParts, NodeMeta, NodeWord, ValidatedDocument};
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
pub enum NodeKind { Null, Bool, Number, String, Array, Object }

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

/// The native DOM and public ABI retain exactly the same tables and owners.
///
/// `into_validated` is an ownership-preserving identity operation, and
/// `from_validated` checks the table layout. The standard identity `From<T> for
/// T` implementation also supplies `From<Document> for ValidatedDocument`.
pub type Document = ValidatedDocument;

#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    pub(crate) document: &'a Document,
    pub(crate) id: NodeId,
}

impl<'a> Node<'a> {
    pub const fn id(self) -> NodeId { self.id }
    pub const fn document(self) -> &'a Document { self.document }
}

#[derive(Clone, Copy, Debug)]
pub struct Object<'a> { pub(crate) node: Node<'a> }

impl<'a> Object<'a> {
    pub const fn node(self) -> Node<'a> { self.node }
}

#[derive(Clone, Copy, Debug)]
pub struct Array<'a> { pub(crate) node: Node<'a> }

impl<'a> Array<'a> {
    pub const fn node(self) -> Node<'a> { self.node }
}

#[derive(Clone, Debug)]
pub struct Children<'a> {
    pub(crate) document: &'a Document,
    pub(crate) next: Option<NodeId>,
    pub(crate) remaining: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Member<'a> { pub key: &'a str, pub value: Node<'a> }

#[derive(Clone, Debug)]
pub struct Members<'a> { pub(crate) children: Children<'a> }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scalar<'a> { Null, Bool(bool), Number(&'a [u8]), String(&'a str) }

#[derive(Clone, Debug)]
pub enum EditAction {
    Replace(Payload),
    Remove,
    AppendArray(Payload),
    SetMember { key: String, value: Payload },
}

#[derive(Clone, Debug)]
pub struct Edit { pub target: NodeId, pub action: EditAction }

pub(crate) const fn dom_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Dom, offset, detail)
}
