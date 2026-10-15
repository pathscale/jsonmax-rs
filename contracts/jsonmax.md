# jsonmax contract, version 1

Owner: W0-ROOTS-8, row 39. This is the native public facade, implemented by four separate waves. All shared public types live in the root and may be imported with `super::*`. Child implementations use the existing native dependency APIs and canonical jmax_abi types. They must not add compatibility intermediates, another JSON value tree, alternate proofs, or undeclared dependencies. The library supports `std` and `alloc` without `std`. Core facade operations never create an executor or perform network I/O.

The exact root source snapshot below freezes public field definitions, enum variants, derives, constants, type reexports, options and methods. Private fields are opaque to downstream callers. No child module may expose constructors that bypass the invariants below. ABI schema/document structs have public fields, so accepting them from a caller always requires validation rather than trusting that their type alone establishes provenance.

## Wave 1: parse.rs

Exact public functions:

```rust
pub fn parse_slice(input: &[u8]) -> Result<Document>;
pub fn parse_owned(input: Vec<u8>, options: ParseOptions) -> Result<Document>;
pub fn parse_payload(input: Payload, options: ParseOptions) -> Result<Document>;
pub fn validate_syntax(input: &[u8], options: ParseOptions) -> Result<()>;
pub fn from_slice<T: JsonDecode + SchemaProvider + 'static>(input: &[u8]) -> Result<Decoded<T>>;
pub fn decode_payload<T: JsonDecode + SchemaProvider + 'static>(input: Payload, options: ParseOptions) -> Result<Decoded<T>>;
```

`parse_slice` copies into an owner-bearing payload and uses ParseOptions::default. `parse_owned` adopts the Vec through Payload::from_vec; `parse_payload` preserves the payload's ownership and does not copy the input merely to add padding. All perform complete native syntax/UTF-8 validation, enforce provided limits and numeric policy, reject non-whitespace trailing content, and build the native flat document. `validate_syntax` performs the same complete syntax/number/limit policy without requiring a DOM result. It never validates just a selected prefix. Empty or whitespace-only input is UnexpectedEof. Duplicate decoded keys follow options.limits.reject_duplicate_keys. Unknown-key rejection is meaningful only when a schema supplies known names; schema-free syntax parsing has no field-name whitelist and ignores reject_unknown_keys.

Payload and decoded lengths must fit u32 before a proof is published. Depth, field, string, array and digit limits apply to all subtrees. Byte offsets count original UTF-8 bytes from payload start, not decoded offsets. A successful syntax-only document uses schema fingerprint 0. A proof describes the exact owner-bearing raw/decoded buffers; no proof is issued before complete input validation. Native lower layers own safe publication of proofs. This crate forbids unsafe code, so facade modules must delegate proof construction to their validated lower-layer APIs.

`from_slice` copies input, then delegates to decode_payload with default options. `decode_payload` caches the compiled schema (and compilation errors) once per TypeId in std mode, verifies its fingerprint equals T::schema_fingerprint(), executes full strict schema validation, and calls T::decode_slots only on the resulting ValidatedSlots. Fingerprint mismatch is SchemaImageInvalid and decoding is not attempted. Slots and T remain together in `Decoded<T>` so string/span references retain their owners. `into_parts` transfers both together; resolving a naked reference against an unrelated owner is never implied safe. Failure drops all partial storage. There is no unchecked native decode entry point.

Errors use ABI lexical/syntax/number errors with original offsets and sources. Limit failures use their precise ABI codes: PayloadTooLarge, DecodedArenaOverflow, DepthLimitExceeded or ConstraintViolation as appropriate. Allocation/resource exhaustion is BudgetExhausted. A value outside a typed/schema constraint is not reclassified as generic InvalidSyntax. No offset is wrapped or silently truncated to u32. Unsupported schema policy is reported explicitly rather than weakening validation.

## Wave 2: query.rs

Exact public functions:

```rust
pub fn compile_paths(paths: &[&str]) -> Result<CompiledQuery>;
pub fn query_payload(input: Payload, query: &CompiledQuery, options: QueryOptions) -> Result<QueryResult>;
pub fn query_slice(input: &[u8], paths: &[&str], options: QueryOptions) -> Result<QueryResult>;
pub fn strict_select(input: Payload, query: &CompiledQuery, options: ParseOptions) -> Result<QueryResult>;
```

`compile_paths` validates every path with the native path compiler, owns the exact path strings in input order, and returns InvalidSyntax for a malformed path. Empty string selects the root; an empty list of paths is allowed. Duplicate paths are legal and produce separate outer result entries. The native dot/escape/index grammar is the baseline. Unsupported extension syntax returns UnsupportedSchemaFeature; no invented JSONPath fallback is permitted. Shared path prefixes must use the native compiled selector mechanism during execution rather than reparsing the whole payload once per selector. The opaque root representation may retain validated source paths and compile a native shared selector once per execution; adding fields requires updating this contract rather than changing the frozen root privately.

`query_slice` owns a copy of input and delegates through compile_paths/query_payload. `strict_select` is query_payload with mode Strict and default max_matches 1024. `query_payload` requires max_matches > 0, caps the total count across all path entries including duplicates, and returns BudgetExhausted on excess without a truncated success. Each outer vector corresponds to a path. Missing paths produce empty vectors, selected null produces a nonempty span. Each span covers a complete selected raw JSON value in the payload, never decoded storage, is checked in range, and follows source order. Explicit named object selection obeys decoded key semantics. Duplicate-key policy follows the chosen options whenever those keys are inspected; Strict enforces it globally.

Strict mode validates the entire root and all tails and unselected data before returning any success, including an empty path list. `fully_validated == true` only after full strict syntax validation under the supplied ParseOptions. Query mode may skip validation of unselected subtree contents and may finish after resolving requested values, but must validate selected values completely, maintain checked bounded traversal, and enforce payload, depth, match and decoded-arena budgets on all work actually performed. A malformed skipped suffix may therefore be outside its guarantee. Query mode always returns fully_validated false, even if an implementation happened to scan the whole input.

`QueryResult` never carries a ValidationProof and cannot be converted directly into ParsedMessage, ValidatedDocument, ValidatedSlots, or a validated raw fragment. To validate selected bytes, callers reparse them as a new owned input. Error codes preserve malformed path, malformed inspected JSON, missing/mismatched traversal and budget distinctions; a missing member or array index is an empty selection rather than an error. `raw_match` checks both indices and the span and returns None for any invalid access. Results retain their payload after caller input is dropped.

## Wave 3: encode.rs

Exact public functions:

```rust
pub fn to_vec<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<Vec<u8>>;
pub fn to_string<T: JsonEncode + ?Sized>(value: &T, options: EncodeOptions) -> Result<String>;
pub fn to_sink<T: JsonEncode + ?Sized>(value: &T, sink: &mut dyn ByteSink, options: EncodeOptions) -> Result<()>;
pub fn document_to_vec(document: &Document, options: EncodeOptions) -> Result<Vec<u8>>;
pub fn escape_string(value: &str, options: EncodeOptions) -> Result<Vec<u8>>;
```

Typed output calls JsonEncode directly through the native byte sink/writer path. It does not construct a generic intermediate tree. All outputs must be exactly one complete strict JSON value. Public trait implementations can be user-supplied and may write invalid bytes; therefore stage bounded output, validate its complete JSON syntax/UTF-8 without requiring a DOM before publication, and reject malformed output using canonical Encode-source errors with output-relative offsets. This safety validation belongs in the public facade. A user-supplied encoder's returned ABI error passes through unchanged. Retain correct numeric sign, finite-float rules and exact fixed-point decimal output. No lossy float conversion for Decimal128 is permitted.

`to_vec` uses OutputBuffer and returns its bytes only after encode and validation succeed. `to_string` uses checked UTF-8 conversion, not unchecked construction. `to_sink` stages and validates the complete value before its first destination write, then invokes reserve(encoded_length) followed by write_bytes. The cap applies to bytes appended by this operation, not preexisting destination contents. Destination failures propagate; arbitrary ByteSink implementations may partially write on their own failure and the facade cannot promise rollback after the destination is called. Encoder/validation errors leave the destination untouched. The size cap is hard, includes delimiters/escapes, and may be zero, in which case every complete JSON encoding fails PayloadTooLarge.

`document_to_vec` emits the native document's values through the syntax-aware encoder, preserving array/member order, decoded string contents, and numeric types; it does not merely return raw input bytes. Because Document is an alias of a public-field ABI struct, it must validate all nodes/indices/container boundaries/number slots/string references before reading them and reject inconsistent documents with InternalError or TypeMismatch. It must not treat a copied ValidationProof as evidence that a caller-mutated document is safe. If the lower layer lacks a safe document validator, rebuild a fresh document from the owned payload and verify all supplied parts against it before encoding; silently ignoring supplied mutated parts is not allowed. No unsafe access is used.

`escape_string` emits one quoted complete JSON string with minimal required escaping: quote, reverse solidus, standard short control escapes where available, and lowercase \\u00xx for other ASCII controls. Valid non-ASCII UTF-8 is preserved; forward solidus is not escaped. The result must decode exactly to the input and includes surrounding quotes in its byte budget. All escaping and formatting are delegated to native encoding primitives where available. Allocation failures are BudgetExhausted, length overflow and cap violations PayloadTooLarge, nonfinite numeric values NumberOverflow, and malformed user-produced JSON uses its precise syntax/UTF-8 code with ErrorSource::Encode.

OutputBuffer::reserve enforces logical length plus additional against max_bytes and fallibly reserves Vec capacity. It does not change length. write_bytes reserves first, so a failed write leaves logical bytes unchanged. clear resets length but retains capacity. Reserving does not grant permission to exceed the cap later. The cap limits logical output bytes rather than allocator size-class overhead.

## Wave 4: endpoint.rs

Exact public functions:

```rust
pub fn compile_schema(ir: &SchemaIr, limits: CompileLimits) -> Result<CompiledSchema>;
pub fn load_schema(image: SchemaImage) -> Result<CompiledSchema>;
pub fn validate_payload(input: Payload, schema: &CompiledSchema, options: ParseOptions) -> Result<ValidatedSlots>;
pub fn extract_payload(
    input: Payload,
    schema: &CompiledSchema,
    selection: &Selection,
    options: ParseOptions,
) -> Result<ValidatedSlots>;
pub fn catalog(services: Vec<ServiceSpec>) -> Result<EndpointCatalog>;
pub fn endpoint_schema(
    catalog: &EndpointCatalog,
    code: u32,
    direction: EndpointDirection,
    limits: CompileLimits,
) -> Result<CompiledSchema>;
pub fn validate_endpoint(
    input: Payload,
    catalog: &EndpointCatalog,
    code: u32,
    direction: EndpointDirection,
    role: &str,
    options: ParseOptions,
) -> Result<EndpointMessage>;
```

`compile_schema` delegates to native schema compilation and validates the resulting image before wrapping it. `load_schema` validates magic/version, opcode and key ranges, target states, field/slot bounds, required bits, key-byte UTF-8/ranges, instruction structure and the native fingerprint before wrapping a caller image. CompiledSchema has no unchecked public constructor or mutable image accessor. Fingerprints use the native canonical algorithm, never a facade-specific hash. SchemaImageInvalid identifies malformed images, UnsupportedSchemaFeature unsupported IR, and BudgetExhausted compilation limits. Canonical schema/field IDs are retained in errors.

`validate_payload` validates complete syntax and every schema constraint, publishing all declared slots with owner-bearing storage and a matching schema fingerprint. `extract_payload` requires selection.strict_validate_unselected == true because it returns ValidatedSlots with a proof; false returns ConstraintViolation and performs no partial validation. Selection IDs must exist, be unique, and fit the schema slot layout; violations are SchemaImageInvalid. An empty selection performs complete validation and returns an empty slots vector; otherwise output slots follow selection.field_ids order. validate_payload's all-slot ordering is the validated image's declared slot order. Missing optional selected fields produce SlotValue::Empty, not a shortened vector. Complete unselected constraints, required/duplicate/unknown key policies and trailing content are still enforced. Limits can tighten schema constraints but never relax them. Options.reject_unknown_keys true tightens a permissive schema; false never disables a schema's own RejectUnknown instruction. No success is returned while input, decoded storage or slot ranges remain unvalidated.

`catalog` owns and validates services without network activity. Service IDs/names and endpoint codes are unique globally; endpoint names are unique per service; endpoint.service equals its containing service.name; names and field names are nonempty; params/response/struct field names are unique within each field set. Reject duplicate roles, unresolved StructRef/EnumRef/StructTable references, incompatible reused type definitions, invalid NanoId lengths, ambiguous enum wire names or discriminants, and unsupported recursive layouts through native endpoint-model validation. Struct and enum namespaces follow the native endpoint model, never implicit case folding. Unknown endpoint codes are UnknownObjectKey, malformed specs ConstraintViolation, and unsupported projections UnsupportedSchemaFeature.

`endpoint_schema` projects params for Request, response for Response, and stream_response for Stream into a native schema. Missing stream_response is TypeMismatch. Endpoint-specific formats, integer widths, optionality, arrays and structured references remain enforced. Never map a format-constrained field to an unconstrained string silently. Unsupported representability fails explicitly. Request and Response are object roots; Stream uses its declared EndpointType as root. Do not invent schema IDs by truncating endpoint code or direction; use the native projection's stable schema ID and fingerprint.

`validate_endpoint` checks the caller role against endpoint.roles before payload parsing; empty roles allow anonymous/public access for any supplied role, while a nonempty role list requires exact membership. Unauthorized roles return AuthFailed from Auth without reflecting input content. Resolve and compile the directional schema with CompileLimits::default_limits, then perform complete validation and return EndpointMessage with the unchanged endpoint code/direction and owned slots. This function validates a raw directional body, not an RPC envelope. Wire, HTTP, WSS, MCP and Nagoya adapters remain in their own crates; no asynchronous runtime is initialized by this facade. Schema caches are optional implementation details and must key on complete canonical fingerprints and policy rather than mutable caller pointers.

## Ownership and error invariants

Public raw/decoded references always resolve against the storage that issued them. Decoded::resolve_str checks source, bounds, UTF-8 and scalar count. It does not claim an arbitrary caller-supplied StrRef originated in that message. Document/slots/proofs are canonical ABI types, not replacements. Query results deliberately provide a smaller guarantee and cannot be upgraded by casting. Proof schema_fingerprint, payload_len and decoded_len must describe the exact validated result.

All native errors preserve code, source, original byte_offset and available schema_id/field_id. Facade configuration errors use facade_error with the relevant subsystem source; default zero offsets mean no payload byte is implicated, not a fabricated parse location. Diagnostics are static and contain no payload excerpts. No success path relies on panic, unchecked narrowing, forged proofs, arbitrary raw output, or unvalidated caller-controlled schema images.

## Frozen shared root API

The exact root source snapshot follows, including implemented bounded sink and ownership helper semantics.

```rust
//! Native JSON facade. Query results and validation proofs have distinct types.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::{
    buffer::{ParsedMessage, Payload, UnvalidatedSpan, ValidationProof},
    endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, ServiceSpec},
    error::{Error, ErrorCode, ErrorSource, Result},
    node::ValidatedDocument,
    number::{ConvertedNumber, Decimal128, DecimalPolicy, NumericPolicy},
    scan::BackendId,
    schema::{CompileLimits, FieldId, SchemaId, SchemaImage, SchemaIr, Selection, SlotValue, ValidatedSlots},
    span::{NodeId, NumberRef, Span32, SpanSource, StrRef},
    syntax::{Limits, Scratch},
    traits::{ByteSink, JsonDecode, JsonEncode, ResourceBudget, SchemaProvider},
    wire::{EnvelopeKind, WireEnvelopeView},
};

pub mod parse;
pub mod query;
pub mod encode;
pub mod endpoint;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub type Document = ValidatedDocument;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseOptions {
    pub limits: Limits,
    pub numeric: NumericPolicy,
}
impl Default for ParseOptions {
    fn default() -> Self { Self { limits: Limits::strict_default(), numeric: NumericPolicy::strict_default() } }
}
/// Typed values remain paired with the exact slots and storage used to decode them.
#[derive(Debug)]
pub struct Decoded<T> { slots: ValidatedSlots, value: T }
impl<T> Decoded<T> {
    pub fn slots(&self) -> &ValidatedSlots { &self.slots }
    pub fn value(&self) -> &T { &self.value }
    pub fn payload(&self) -> &Payload { &self.slots.payload }
    pub fn proof(&self) -> ValidationProof { self.slots.proof }
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        let bytes = match reference.source {
            SpanSource::RawPayload => reference.span.slice(self.slots.payload.as_slice())?,
            SpanSource::DecodedArena => reference.span.slice(&self.slots.decoded)?,
        };
        let text = core::str::from_utf8(bytes).ok()?;
        if text.chars().count() == reference.scalar_len as usize { Some(text) } else { None }
    }
    pub fn into_parts(self) -> (ValidatedSlots, T) { (self.slots, self.value) }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryMode {
    /// Validate the complete document, including unselected subtrees and its tail.
    Strict,
    /// Only selected values are validated; this never publishes a validation proof.
    Query,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryOptions {
    pub parse: ParseOptions,
    pub mode: QueryMode,
    pub max_matches: u32,
}
impl Default for QueryOptions {
    fn default() -> Self { Self { parse: ParseOptions::default(), mode: QueryMode::Strict, max_matches: 1024 } }
}
#[derive(Clone, Debug)]
pub struct CompiledQuery { paths: Vec<String> }
impl CompiledQuery {
    pub fn paths(&self) -> &[String] { &self.paths }
}
#[derive(Clone, Debug)]
pub struct QueryResult {
    payload: Payload,
    matches: Vec<Vec<Span32>>,
    fully_validated: bool,
}
impl QueryResult {
    pub fn payload(&self) -> &Payload { &self.payload }
    /// The outer index corresponds exactly to CompiledQuery::paths().
    pub fn matches(&self) -> &[Vec<Span32>] { &self.matches }
    pub fn fully_validated(&self) -> bool { self.fully_validated }
    pub fn raw_match(&self, path_index: usize, match_index: usize) -> Option<&[u8]> {
        self.matches.get(path_index)?.get(match_index)?.slice(self.payload.as_slice())
    }
}
#[derive(Clone, Debug)]
pub struct CompiledSchema { image: SchemaImage }
impl CompiledSchema {
    pub fn image(&self) -> &SchemaImage { &self.image }
    pub fn fingerprint(&self) -> u64 { self.image.header.fingerprint }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeOptions { pub max_output_bytes: usize }
impl Default for EncodeOptions {
    fn default() -> Self { Self { max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES } }
}

/// Bounded growable sink. Failed writes leave the logical contents unchanged.
#[derive(Clone, Debug)]
pub struct OutputBuffer { bytes: Vec<u8>, max_bytes: usize }
impl OutputBuffer {
    pub const fn new(max_bytes: usize) -> Self { Self { bytes: Vec::new(), max_bytes } }
    pub fn as_bytes(&self) -> &[u8] { &self.bytes }
    pub fn into_vec(self) -> Vec<u8> { self.bytes }
    pub fn clear(&mut self) { self.bytes.clear(); }
    pub fn max_bytes(&self) -> usize { self.max_bytes }
}
impl ByteSink for OutputBuffer {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        let length = self.bytes.len().checked_add(additional)
            .ok_or_else(|| facade_error(ErrorCode::PayloadTooLarge, ErrorSource::Encode, "output size overflow"))?;
        if length > self.max_bytes {
            return Err(facade_error(ErrorCode::PayloadTooLarge, ErrorSource::Encode, "output byte limit exceeded"));
        }
        self.bytes.try_reserve(additional)
            .map_err(|_| facade_error(ErrorCode::BudgetExhausted, ErrorSource::Encode, "output allocation failed"))
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn len(&self) -> usize { self.bytes.len() }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointDirection { Request, Response, Stream }
#[derive(Clone, Debug)]
pub struct EndpointCatalog { services: Vec<ServiceSpec> }
impl EndpointCatalog {
    pub fn services(&self) -> &[ServiceSpec] { &self.services }
    pub fn endpoint(&self, code: u32) -> Option<&EndpointSchemaSpec> {
        self.services.iter().flat_map(|service| service.endpoints.iter()).find(|endpoint| endpoint.code == code)
    }
}
#[derive(Clone, Debug)]
pub struct EndpointMessage {
    pub endpoint_code: u32,
    pub direction: EndpointDirection,
    pub slots: ValidatedSlots,
}
pub fn facade_error(code: ErrorCode, source: ErrorSource, detail: &'static str) -> Error {
    Error::new(code, source, 0, detail)
}
```

## Follow-up: reusable storage and precompiled typed schemas

`parse::ParseScratch` retains DOM tables, builder frames, cursor frames, decoded
bytes, and duplicate-key tables. `parse_payload_into(&Payload, ParseOptions,
&mut ParseScratch)` returns a document borrowing scratch. Resetting scratch makes
its prior result unavailable, including after an invalid input.

`endpoint::ValidationScratch` retains cursor storage, VM frames, per-depth seen
masks and unknown-key tables, selection words, slots, decoded bytes, and the
selection reorder buffer. `validate_payload_into` and `extract_payload_into`
accept borrowed payload/schema plus options and scratch; successful output borrows
scratch. They share strict event execution and reference checks with the allocating
API, issue a proof only after EOF, and charge all retained VM capacities against
each invocation's scratch limit. New larger shapes can grow; prewarmed capacities
require no heap allocations. `ValidatedSlots.decoded` is now `Vec<u8>` so spare
capacity survives publication. Existing immutable `ParsedMessage` adapters convert
it to their boxed arena when transferring owners.

`parse::from_slice_with_schema<T>(&[u8], &CompiledSchema)` and
`decode_payload_with_schema<T>(&Payload, &CompiledSchema)` bypass compilation.
Their `_into` variants decode into `ValidationScratch` and return `T`; resolve its
owner-relative references against that scratch's current `slots()`. Custom decoders
may allocate their own owned output. `decode_payload_into` uses the std type cache;
`from_slice_into` also copies the borrowed input. Borrowed-slice helpers therefore
still allocate an input owner. The allocation-free input path uses retained Payload.
Without std, explicit precompiled schemas avoid recompilation. The default generic
helpers retain the alloc-only compilation fallback.

`encode::document_to_buffer` retains `OutputBuffer` capacity and `WriterScratch`
container frames across messages, including frame recovery on encode errors.
