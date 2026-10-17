//! Normalized schema graphs and the contracts shared by all schema passes.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
pub use jmax_abi::schema::{FieldId, SchemaFieldIr, SchemaId, SchemaIr};
pub use jmax_abi::syntax::ScalarKind;

pub mod model;
pub mod resolve;
pub mod policy;
pub mod canonical;

/// Version of the canonical schema byte grammar, independent of image versions.
pub const CANONICAL_VERSION: u16 = 1;
pub const FINGERPRINT_OFFSET: u64 = 0xcbf29ce484222325;
pub const FINGERPRINT_PRIME: u64 = 0x100000001b3;

/// Index in `Schema::nodes`. This is not an ABI document node identifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TypeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueKind { Null, Boolean, Integer, Number, Decimal, String }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalConstraints {
    pub policy: DecimalPolicy,
    pub minimum: Option<Decimal128>,
    pub maximum: Option<Decimal128>,
}

/// Lengths count decoded Unicode scalar values, never encoded JSON bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScalarConstraints {
    pub kind: ValueKind,
    pub min_i64: Option<i64>,
    pub max_i64: Option<i64>,
    pub decimal: Option<DecimalConstraints>,
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    pub pattern: Option<String>,
    pub format: Option<String>,
    /// A JSON `null` is accepted in place of a value of this kind.
    pub nullable: bool,
}

impl ScalarConstraints {
    pub const fn new(kind: ValueKind) -> Self {
        Self { kind, min_i64: None, max_i64: None, decimal: None,
            min_length: None, max_length: None, pattern: None, format: None, nullable: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    pub field_id: FieldId,
    pub name: String,
    pub ty: TypeId,
    pub required: bool,
    /// A scalar or complete raw container may be captured here.
    pub slot: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectShape {
    /// A JSON `null` is accepted in place of an object.
    pub nullable: bool,
    pub properties: Vec<Property>,
    pub reject_unknown: bool,
    pub min_properties: Option<u32>,
    pub max_properties: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayShape {
    /// A JSON `null` is accepted in place of an array.
    pub nullable: bool,
    pub items: TypeId,
    pub min_items: Option<u32>,
    pub max_items: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeNode {
    Any,
    Scalar(ScalarConstraints),
    Object(ObjectShape),
    Array(ArrayShape),
    Ref(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedType { pub name: String, pub ty: TypeId }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub schema_id: SchemaId,
    pub root: TypeId,
    pub nodes: Vec<TypeNode>,
    pub definitions: Vec<NamedType>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolveLimits {
    pub max_nodes: u32,
    pub max_definitions: u32,
    pub max_reference_hops: u32,
    pub max_name_bytes: u32,
}

impl Default for ResolveLimits {
    fn default() -> Self {
        Self { max_nodes: 65_536, max_definitions: 16_384,
            max_reference_hops: 65_536, max_name_bytes: 4_096 }
    }
}

/// Only the resolver can construct this witness. Alias targets are fully collapsed.
#[derive(Clone, Debug)]
pub struct ResolvedSchema {
    pub(crate) schema: Schema,
    pub(crate) targets: Vec<TypeId>,
}

impl ResolvedSchema {
    pub fn schema(&self) -> &Schema { &self.schema }
    pub fn root(&self) -> TypeId { self.targets[self.schema.root.0 as usize] }
    pub fn target(&self, id: TypeId) -> Option<TypeId> {
        self.targets.get(id.0 as usize).copied()
    }
    pub fn node(&self, id: TypeId) -> Option<&TypeNode> {
        self.target(id).and_then(|id| self.schema.nodes.get(id.0 as usize))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect { Endpoint, JsonSchema202012 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownFormatPolicy { Reject, Annotate }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaFeature {
    LocalReferences, RecursiveTypes, Patterns, Formats, Decimal,
    Union, RemoteReferences, Conditional, UnevaluatedProperties,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaPolicy {
    pub dialect: Dialect,
    pub numeric: NumericPolicy,
    pub unknown_formats: UnknownFormatPolicy,
    pub allow_recursive: bool,
    pub max_pattern_bytes: u32,
}

impl Default for SchemaPolicy {
    fn default() -> Self {
        Self { dialect: Dialect::Endpoint, numeric: NumericPolicy::strict_default(),
            unknown_formats: UnknownFormatPolicy::Reject, allow_recursive: true,
            max_pattern_bytes: 4_096 }
    }
}

/// Canonical bytes are the identity input; the hash is a non-cryptographic cache hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalSchema { pub bytes: Vec<u8>, pub fingerprint: u64 }

/// Incremental, platform-independent FNV-1a over precisely the supplied bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint(u64);

impl Default for Fingerprint { fn default() -> Self { Self::new() } }

impl Fingerprint {
    pub const fn new() -> Self { Self(FINGERPRINT_OFFSET) }
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FINGERPRINT_PRIME);
        }
    }
    pub const fn finish(self) -> u64 { self.0 }
}

pub(crate) fn schema_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, 0, detail)
}
