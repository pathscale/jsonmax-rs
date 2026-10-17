# jmax-schema-ir frozen contract, version 1

Scope: Wave 0 owns `src/lib.rs`; Waves 1 through 4 own only their named modules. All module implementations use `super::*`. The shared root below is normative, including visibility, derives, constants, and method bodies. Do not redefine any ABI type. All APIs are synchronous, `no_std` plus `alloc`, and use only the allowed dependency `jmax-abi`.

## Wave 1: model.rs

```rust
pub fn normalize(ir: &SchemaIr) -> Result<Schema>;
pub fn validate(schema: &Schema) -> Result<()>;
pub fn constraints(schema: &ResolvedSchema, ty: TypeId) -> Result<Option<&ScalarConstraints>>;
pub fn property<'a>(schema: &'a ResolvedSchema, object: TypeId, name: &str) -> Result<Option<&'a Property>>;
pub fn required_fields(schema: &ResolvedSchema, object: TypeId) -> Result<Vec<FieldId>>;
```

`normalize` builds node zero as the root object. It preserves schema name/id, field order, names, requiredness, and reject-unknown policy. Field i has slot i and a separate child node. A field without scalar_kind is Any and must have no scalar constraints. String maps to String, Number with either integer bound maps to Integer, other Number maps to Number, True and False both denote the Boolean category, and Null maps to Null. Literal-only booleans are not represented by this adapter. A name equal to the empty string is a valid JSON object key. `required_count` must exactly match the required fields; do not silently repair it. An ABI scalar field cannot represent containers or decimals; the graph API can.

`validate` checks the entire stored graph, including unreachable nodes. All TypeIds, property targets, array targets and named-definition targets must be in bounds. Names of definitions must be nonempty and unique; their strings are exact local names, not URLs or JSON Pointers. Object property names must be unique by UTF-8 bytes. FieldIds are globally unique across stored properties, and slot indices are globally unique where present. Graph size and counts must fit u32. A reference name must be nonempty; existence and cycles are the resolver's responsibility. Integer bounds only apply to Integer; decimal constraints are required exactly for Decimal; length, pattern and format constraints only apply to String. Reject min greater than max. Decimal precision is 1..=38, target_scale <=38, tick_step >=0, bound scales <=38, and bounds must be comparable without lossy rescaling. Decimal comparison must handle i128 extremes without overflow. Incompatible constraint families must fail rather than be ignored.

`constraints` follows a resolved alias and returns None for a non-scalar node. `property` and `required_fields` require an object after alias resolution; a missing property returns None. Required fields are returned in stored property order. No function mutates its input.

Errors: invalid IDs, duplicate names/IDs/slots, incompatible or contradictory bounds, and bad decimal policy use ConstraintViolation. Wrong accessor node kind uses TypeMismatch. Unrepresentable counts use BudgetExhausted. All errors use SchemaCompile, byte_offset 0, and attach schema/field IDs when available.

## Wave 2: resolve.rs

```rust
pub fn resolve(schema: Schema, limits: ResolveLimits) -> Result<ResolvedSchema>;
pub fn resolve_type(schema: &ResolvedSchema, ty: TypeId) -> Result<TypeId>;
pub fn has_recursion(schema: &ResolvedSchema) -> Result<bool>;
```

Run model::validate before constructing the private ResolvedSchema fields. `targets.len() == schema.nodes.len()`. For every non-Ref node i, targets[i] is i. For every Ref node, targets[i] is the final non-Ref node reached through exact named-definition lookup. No entry points at Ref. root() and node() rely on this witness; never construct it before completing all checks. Check references in unreachable nodes too. Resolve iteratively with visit colors or equivalent bounded state; do not recurse on the machine stack. max_reference_hops bounds any alias chain; zero permits no Ref traversal. max_nodes/max_definitions/max_name_bytes are inclusive limits. max_name_bytes applies to schema, definition, reference and property names.

An alias-only cycle has no value constructor and fails with UnsupportedSchemaFeature. A cycle through Object or Array is legal recursive structure. `has_recursion` examines only the root-reachable, alias-collapsed graph and detects such structural cycles iteratively. Unknown local reference names use UnsupportedSchemaFeature; invalid IDs use ConstraintViolation; exceeded limits or allocation failures use BudgetExhausted. resolve_type returns the checked collapsed target or ConstraintViolation for an invalid ID.

## Wave 3: policy.rs

```rust
pub fn supports(policy: SchemaPolicy, feature: SchemaFeature) -> bool;
pub fn validate_policy(policy: &SchemaPolicy) -> Result<()>;
pub fn validate_schema(schema: &ResolvedSchema, policy: &SchemaPolicy) -> Result<()>;
pub fn known_format(name: &str) -> bool;
```

Both dialects support local references, patterns, formats and decimals. RecursiveTypes is supported exactly when allow_recursive is true. Union, RemoteReferences, Conditional and UnevaluatedProperties are unsupported. Dialect is an explicit identity/provenance choice; this crate does not parse a JSON Schema document or silently accept unmodeled keywords. Importers must call supports for every encountered feature before dropping syntax.

validate_policy rejects a negative numeric.max_exponent_abs and a zero max_pattern_bytes with ConstraintViolation. validate_schema validates policy and all reachable constraints, rejects recursion when disabled, and bounds each pattern's UTF-8 byte count; regex syntax is validated by the compiler. Format names are case sensitive: uuid, ipv4, ipv6, ip, base64, base64url, hex, nanoid, blockchain-address, blockchain-transaction-hash. known_format recognizes exactly this list, shared with format registry version 1. Empty/unknown names fail with UnsupportedSchemaFeature under Reject; Annotate retains them in canonical input but lowering emits no validator for them. No networking or remote resolution exists. Unsupported features use UnsupportedSchemaFeature; pattern byte limits use BudgetExhausted.

## Wave 4: canonical.rs

```rust
pub fn canonicalize(schema: &ResolvedSchema, policy: &SchemaPolicy) -> Result<CanonicalSchema>;
pub fn fingerprint(schema: &ResolvedSchema, policy: &SchemaPolicy) -> Result<u64>;
pub fn fingerprint_bytes(bytes: &[u8]) -> u64;
```

canonicalize first runs policy::validate_schema. Deterministically number reachable alias-collapsed nodes by breadth-first discovery from root (canonical index zero). Visit object edges in ascending UTF-8 property-name order and the single array edge directly. A previously discovered target gets its existing index, making recursion finite. Exclude unreachable nodes, alias names and definition names. Preserve schema name/id, FieldIds, explicit slots, bounds, and policy. Property declaration order does not change identity. Different explicit default/bound representations may change identity; this is structural canonicalization, not a theorem prover for equivalent schemas.

Canonical byte grammar v1: all fixed-width integers little endian, booleans exactly u8 0/1, strings as u32 UTF-8 byte length then bytes, vectors as u32 element count then elements, Option as u8 0 or 1 followed by the present value. No native layout, padding, usize, Debug or enum casts. Prefix is the 12 bytes `JMAX-SCHEMA\0`, followed by u16 CANONICAL_VERSION, policy, schema name, u32 schema_id, u32 reachable node count, and node records in canonical order. Policy is dialect u8 (Endpoint=0, JsonSchema202012=1), NumericPolicy fields in ABI declaration order (three bools then i16), unknown_formats u8 (Reject=0, Annotate=1), allow_recursive bool, max_pattern_bytes u32.

Node record tag is Any=0, Scalar=1, Object=2, Array=3. Any has no payload. Scalar encodes kind u8 (Null=0, Boolean=1, Integer=2, Number=3, Decimal=4, String=5), min_i64 Option<i64>, max_i64 Option<i64>, decimal Option<DecimalConstraints>, min_length/max_length Option<u32>, pattern/format Option<string>, in that order. DecimalConstraints is DecimalPolicy in ABI order (u8,u8,bool,bool,i128), then minimum/maximum Option<Decimal128>; Decimal128 is i128 coefficient,u8 scale. Object is reject_unknown bool, min/max_properties Option<u32>, then property vector sorted by name. Each property is u32 field_id,string name,u32 canonical target,bool required,Option<u32> slot. Array is u32 canonical items target, min/max_items Option<u32>. There are no Ref records.

Nullable nodes. A Scalar, Object or Array node with `nullable` set means a JSON null is accepted in place of
its value (in addition to a value of its kind with all its constraints); a nullable node under a
non-nullable one or the reverse are different schemas. The canonical record of a nullable node has tag 4
(Scalar), 5 (Object) or 6 (Array) and the payload of tag 1, 2 or 3, so the bytes, and therefore the
fingerprint, of every schema with no nullable node are exactly what they were before the flag existed.

fingerprint_bytes computes root Fingerprint/FNV-1a exactly, with wrapping u64 multiplication. fingerprint must return canonicalize(...).fingerprint. Overflow or allocation failure uses BudgetExhausted; policy errors propagate. Hash collisions do not establish schema equivalence, authentication, or trust. Compare canonical bytes for identity-sensitive uses.

## Normative shared root

```rust
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
    /// A JSON null is accepted in place of a value of this kind.
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
    /// A JSON null is accepted in place of an object.
    pub nullable: bool,
    pub properties: Vec<Property>,
    pub reject_unknown: bool,
    pub min_properties: Option<u32>,
    pub max_properties: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayShape {
    /// A JSON null is accepted in place of an array.
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
```
