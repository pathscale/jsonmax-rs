# jmax-compat: frozen Wave 0 contract

Status: W0-ROOTS-7. Shared definitions use the frozen ABI and are owned only by lib.rs. Every child may import `super::*`. The existing manifest has neither a Serde dependency nor a Serde feature. Wave 0 does not change that manifest or invent a feature. Optional direct Serde integration is supplied through application-owned implementations of DirectDeserialize and DirectSerialize; consumers with Serde wrap their types and call their codec directly. These traits are functional codec seams, not automatic implementations of external Serde traits. The crate supports alloc without std and forbids unsafe code.

## Exact shared surface

The root snapshot below is the complete source of truth for all public structs/enums, generic bounds, methods, constants, visibility, and ABI re-exports. All adapters return canonical ABI errors. CompatOptions defaults to legacy version 1, LossPolicy::Reject, strict syntax limits, and an 8 MiB output bound. Version zero and unknown versions are rejected even under AllowDocumented. BoundedSink performs checked length accounting and fallible allocation before changing its buffer. ByteSink::reserve reserves capacity but does not change len; all subsequent writes independently enforce the limit.

LossPolicy is never permission to weaken JSON syntax, UTF-8, required fields, uniqueness, reference validity, or native type safety. AllowDocumented permits only explicitly documented metadata loss and returns one CompatibilityWarning for every affected path. Reject fails with UnsupportedSchemaFeature before returning any partially converted output. Conversion::exact has no warnings; Conversion::map preserves the warning list. No warning is silently printed.

## Wave 1: serde.rs

```rust
pub fn decode_direct<'de, T: DirectDeserialize<'de>>(input: &'de [u8], validator: &mut dyn CompatibilityValidator, options: CompatOptions) -> Result<T>;
pub fn encode_direct<T: DirectSerialize + ?Sized>(value: &T, validator: &mut dyn CompatibilityValidator, options: CompatOptions) -> Result<Payload>;
pub fn decode_native<T: JsonDecode>(slots: &ValidatedSlots) -> Result<T>;
pub fn encode_native<T: JsonEncode + ?Sized>(value: &T, validator: &mut dyn CompatibilityValidator, options: CompatOptions) -> Result<Payload>;
```

All option-bearing calls validate legacy_version. decode_direct enforces max_payload_bytes and UTF-8, asks CompatibilityValidator to validate a Payload copy of the entire input with options.limits, checks the returned owner's bytes and proof lengths, then invokes T::deserialize_json on the original borrowed input. No decoded temporary may be borrowed by T. DirectDeserialize implementations must consume exactly one JSON value, enforce the supplied limits, and map their codec errors to the canonical ABI. Optional application Serde bridges use their deserializer's end-of-input check and must not introduce a JSON Value intermediate. The native validator remains authoritative for strict syntax and resource limits regardless of a Serde codec's defaults.

encode_direct writes via DirectSerialize into a fresh BoundedSink capped by min(max_output_bytes, limits.max_payload_bytes). A codec must propagate sink errors and must write exactly one complete JSON value. The resulting owned bytes undergo full independent CompatibilityValidator validation before publication. encode_native applies the identical process through JsonEncode. Both check returned proof length/owner consistency and decoded-arena limits; a malicious codec's invalid JSON cannot become successful output. The return payload contains exactly the encoder's bytes, without a DOM reserialization or whitespace rewriting. Allocation uses try_reserve; no global state, hidden cache, or worker is created.

decode_native calls T::decode_slots against the caller's already validated slots. It grants no new proof and must not rebase spans. JsonDecode implementations resolving string slots must perform owner-bound checks. Validation errors propagate unchanged; codec conversion failures use Decode for input, Encode for output, and the most precise ABI code. Invalid/inconsistent validator return data is InternalError. Oversized input/output is PayloadTooLarge; decoded arena overflow is DecodedArenaOverflow; allocation failure is BudgetExhausted.

## Wave 2: endpoint.rs

```rust
pub fn import_endpoint(legacy: LegacyEndpoint, options: CompatOptions) -> Result<Conversion<EndpointSchemaSpec>>;
pub fn export_endpoint(endpoint: &EndpointSchemaSpec, options: CompatOptions) -> Result<Conversion<LegacyEndpoint>>;
pub fn import_services(legacy: &[LegacyService], options: CompatOptions) -> Result<Conversion<Vec<ServiceSpec>>>;
pub fn export_services(services: &[ServiceSpec], options: CompatOptions) -> Result<Conversion<Vec<LegacyService>>>;
```

LegacyEndpoint and LegacyService are explicit in-memory version-1 wrappers around the ABI model. Existing endpoint-libs integrations populate these wrappers through their own dependency, avoiding a new dependency here. Every nested endpoint wrapper version must match the service/options version. All 23 EndpointType variants are preserved exactly, including Optional versus Unit, named struct and enum references, StructTable, NanoId length, signed enum values, prefixed_name, timestamps, and blockchain-specific types. No type is replaced with Object or String as a fallback.

Imports/exports preserve endpoint codes, service IDs, field order, role order, variant order/values, descriptions, stream-response shape, and exact case. Validate nonempty names, unique service IDs/names, unique endpoint codes/names within each service, unique field names within each field set, unique roles, unique enum variant names/values, and resolvable references across the supplied service set. EndpointSchemaSpec.service must match its containing service.name. Struct/enum definitions with the same name must be identical; conflicts are errors. An endpoint-only conversion validates local structure and nonempty reference names but defers cross-service resolution to import_services/export_services. Finite recursive references are allowed; bound traversal using Limits.max_depth and do not expand cycles. NanoId length zero is ConstraintViolation.

The current wrappers can represent every canonical endpoint field, so version-1 endpoint conversion is lossless and produces no warnings under either policy. Unsupported future metadata requires an explicit contract revision. Invalid duplicate names use DuplicateObjectKey, missing required names MissingRequiredField, conflicting names/references ConstraintViolation, unresolved references UnsupportedSchemaFeature, version mismatch UnsupportedSchemaFeature, traversal exhaustion DepthLimitExceeded. These are SchemaCompile-source errors. No result is returned until the entire conversion validates.

## Wave 3: schema.rs

```rust
pub fn import_schema(input: &[u8], codec: &mut dyn LegacySchemaCodec, options: CompatOptions) -> Result<Conversion<SchemaIr>>;
pub fn export_schema(schema: &SchemaIr, description: &str, codec: &mut dyn LegacySchemaCodec, options: CompatOptions) -> Result<Payload>;
pub fn import_documentation(document: LegacyDocumentation, options: CompatOptions) -> Result<Conversion<LegacyDocumentation>>;
pub fn export_documentation(title: &str, body: &str, options: CompatOptions) -> Result<LegacyDocumentation>;
```

import_schema checks input byte budget and version, invokes the application's LegacySchemaCodec, validates its version and IR, then transfers the SchemaIr. The external codec is responsible for its exact legacy source grammar, full-input consumption, UTF-8, depth, and decoded-memory limits; unsupported syntax or schema features must return UnsupportedSchemaFeature instead of dropping constraints. Codec use does not grant a payload proof. Validate a nonempty schema/field name, unique FieldId and field name, checked required_count equal to the actual required field count, min_len <= max_len, min_i64 <= max_i64, and nonempty supplied format names. Do not synthesize missing formats or pretend unsupported patterns exist in SchemaIr.

LegacySchema.description has no destination in SchemaIr. Nonempty description is therefore explicit metadata loss: Reject returns UnsupportedSchemaFeature, AllowDocumented returns a warning at `description` with detail `legacy schema description is not represented by SchemaIr`. Empty description is lossless. Field order and IDs remain exact. No numeric constraint is dropped.

export_schema validates the native IR and version, constructs a LegacySchema with the supplied description, and writes through a BoundedSink capped by max_output_bytes. LegacySchemaCodec must propagate sink errors; its output format is the application's declared legacy format, not necessarily JSON. This API does not assert JSON proof for opaque schema artifacts. If a codec emits more than max_output_bytes, return PayloadTooLarge. Native IR cannot express some old schema features; they are rejected on import by the codec, never silently relaxed by this crate.

Documentation is UTF-8 Rust String data. Import/export preserves title and body byte-for-byte, including line endings and Markdown punctuation. A nonempty title is required. Their combined UTF-8 byte length uses checked arithmetic and is bounded by max_output_bytes, with import additionally bounded by limits.max_payload_bytes. Version mismatch is UnsupportedSchemaFeature; empty titles MissingRequiredField; inverted bounds ConstraintViolation; duplicate IDs/names DuplicateObjectKey. Schema problems have SchemaCompile source; documentation input errors Decode source.

## Wave 4: utilities.rs

```rust
pub use jmax_abi::endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec};
pub use jmax_abi::span::{Span32, StrRef};
pub use jmax_abi::traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider};
pub fn resolve_str<'a>(payload: &'a Payload, decoded: &'a [u8], reference: StrRef) -> Result<&'a str>;
pub fn checked_raw_span(payload: &Payload, span: Span32) -> Result<&[u8]>;
pub fn find_endpoint<'a>(services: &'a [ServiceSpec], service: &str, endpoint: &str) -> Result<&'a EndpointSchemaSpec>;
pub fn encode_header_value(value: &str) -> Result<String>;
```

These are retained native utilities; do not re-export a module named serde from a different dependency or shadow ABI types. resolve_str selects RawPayload/DecodedArena, checks Span32 end/bounds, UTF-8, and scalar_len. Invalid spans/counts use ConstraintViolation, invalid UTF-8 InvalidUtf8; no unchecked slicing. checked_raw_span checks overflow and owner bounds with ConstraintViolation. find_endpoint uses exact case-sensitive names; missing names MissingRequiredField and ambiguous duplicate matches DuplicateObjectKey. It must not silently choose the first of duplicate service/endpoint names.

encode_header_value preserves valid bytes exactly. Reject all control characters except horizontal tab, including CR, LF, NUL, and DEL, with InvalidToken; reject non-ASCII characters with InvalidToken so a caller cannot accidentally send UTF-8 as an HTTP field value. Empty values and leading/trailing spaces are preserved. This is value validation, not authentication, percent encoding, or header-name construction. Utilities use Decode source for span/model errors and Encode source for header errors.

## Cross-module invariants

There is no intermediate general-purpose JSON tree in direct Serde encoding/decoding, no disabled-feature success fallback, and no default schema bypass. CompatibilityValidator can internally use the allowed DOM/VM dependencies for independent native validation; consumers choose the direct codec explicitly. Every untrusted codec return is checked before publishing native ownership or output. Options are immutable per operation. BoundedSink is the only shared output accumulator, and no module may weaken its limit. Endpoint and schema utilities do not call serde; serde uses root traits and may use utilities; utilities never call conversion modules. Interoperability with an external legacy library requires its application's wrapper implementation, with no Cargo.toml edits in this wave.

## Frozen root snapshot

```rust
//! Explicit compatibility adapters. Legacy values never bypass native validation.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::Payload,
    endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec},
    error::{Error, ErrorCode, ErrorSource, Result},
    node::ValidatedDocument,
    schema::{SchemaIr, ValidatedSlots},
    span::{Span32, SpanSource, StrRef},
    syntax::Limits,
    traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider},
};

pub mod serde;
pub mod endpoint;
pub mod schema;
pub mod utilities;

pub const LEGACY_FORMAT_VERSION: u16 = 1;
pub const DEFAULT_MAX_OUTPUT_BYTES: u32 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossPolicy { Reject, AllowDocumented }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompatOptions {
    pub legacy_version: u16,
    pub loss_policy: LossPolicy,
    pub limits: Limits,
    pub max_output_bytes: u32,
}

impl Default for CompatOptions {
    fn default() -> Self {
        Self {
            legacy_version: LEGACY_FORMAT_VERSION,
            loss_policy: LossPolicy::Reject,
            limits: Limits::strict_default(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatibilityWarning { pub path: String, pub detail: String }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversion<T> { pub value: T, pub warnings: Vec<CompatibilityWarning> }

impl<T> Conversion<T> {
    pub fn exact(value: T) -> Self { Self { value, warnings: Vec::new() } }
    pub fn is_lossless(&self) -> bool { self.warnings.is_empty() }
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Conversion<U> {
        Conversion { value: f(self.value), warnings: self.warnings }
    }
}

/// Application-owned wrappers implement this with their optional Serde codec.
/// Returned borrows may refer only to input, never to temporary parser storage.
pub trait DirectDeserialize<'de>: Sized {
    fn deserialize_json(input: &'de [u8], limits: Limits) -> Result<Self>;
}

/// Encode directly into the bounded native sink, without an intermediate DOM.
pub trait DirectSerialize {
    fn serialize_json(&self, sink: &mut dyn ByteSink) -> Result<()>;
}

/// Validates bytes independently of the optional compatibility codec.
pub trait CompatibilityValidator {
    fn validate_json(&mut self, input: Payload, limits: Limits) -> Result<ValidatedDocument>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyEndpoint {
    pub version: u16,
    pub endpoint: EndpointSchemaSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyService {
    pub version: u16,
    pub id: u16,
    pub name: String,
    pub endpoints: Vec<LegacyEndpoint>,
}

#[derive(Clone, Debug)]
pub struct LegacySchema {
    pub version: u16,
    pub schema: SchemaIr,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyDocumentation {
    pub version: u16,
    pub title: String,
    pub body: String,
}

/// The application's old schema parser supplies this seam; no format is guessed.
pub trait LegacySchemaCodec {
    fn decode_schema(&mut self, input: &[u8], limits: Limits) -> Result<LegacySchema>;
    fn encode_schema(&mut self, schema: &LegacySchema, sink: &mut dyn ByteSink) -> Result<()>;
}

/// Allocation failures and byte limits use the same ABI error as native sinks.
#[derive(Clone, Debug)]
pub struct BoundedSink {
    bytes: Vec<u8>,
    maximum: usize,
}

impl BoundedSink {
    pub fn new(maximum: u32) -> Self { Self { bytes: Vec::new(), maximum: maximum as usize } }
    pub fn as_slice(&self) -> &[u8] { &self.bytes }
    pub fn into_vec(self) -> Vec<u8> { self.bytes }
}

impl ByteSink for BoundedSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        let total = self.bytes.len().checked_add(additional)
            .ok_or_else(|| compat_error(ErrorCode::PayloadTooLarge, "compatibility output length overflow"))?;
        if total > self.maximum {
            return Err(compat_error(ErrorCode::PayloadTooLarge, "compatibility output byte limit exceeded"));
        }
        self.bytes.try_reserve(additional)
            .map_err(|_| compat_error(ErrorCode::BudgetExhausted, "compatibility output allocation failed"))
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn len(&self) -> usize { self.bytes.len() }
}

pub(crate) fn compat_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Decode, 0, detail)
}

pub(crate) fn ensure_version(version: u16, options: CompatOptions) -> Result<()> {
    if version != LEGACY_FORMAT_VERSION || options.legacy_version != LEGACY_FORMAT_VERSION {
        return Err(compat_error(ErrorCode::UnsupportedSchemaFeature, "unsupported legacy format version"));
    }
    Ok(())
}
```
