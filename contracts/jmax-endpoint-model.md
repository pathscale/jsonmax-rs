# jmax-endpoint-model: frozen Wave 0 contract

Owner: W0-ROOTS-5, row 23. Source of truth:
`crates/jmax-endpoint-model/src/lib.rs`. The root exposes registry, legacy,
wire_names, and utilities. Modules can import super::* and do not redefine
canonical endpoint types. The crate supports alloc without std. Allowed
dependencies: jmax-abi, jmax-schema-ir, jmax-format.

## Root definitions

Public ABI re-exports are EndpointField, EndpointSchemaSpec, EndpointType,
EnumVariantSpec, ServiceSpec; Error, ErrorCode, ErrorSource, Result; FieldId,
SchemaFieldIr, SchemaId, SchemaIr; ScalarKind. The endpoint definitions, including every
EndpointType variant, remain exactly those of jmax_abi::endpoint.

```rust
pub const CONTRACT_VERSION: u16 = 1;
pub const MAX_REFERENCE_DEPTH: usize = 128;
pub struct ModelVersion { pub major: u32, pub minor: u32, pub patch: u32 }
impl ModelVersion { pub const fn new(major: u32, minor: u32, patch: u32) -> Self; }
pub struct TypeDefinition { pub name: String, pub ty: EndpointType }
pub struct RoleSpec { pub name: String, pub includes: Vec<String>, pub description: String }
pub struct ErrorSpec { pub code: u32, pub name: String, pub message: String }
pub struct HeaderSpec { pub name: String, pub ty: EndpointType, pub required: bool, pub description: String }
pub struct EndpointModel {
    pub version: ModelVersion, pub services: Vec<ServiceSpec>,
    pub definitions: Vec<TypeDefinition>, pub roles: Vec<RoleSpec>,
    pub errors: Vec<ErrorSpec>, pub headers: Vec<HeaderSpec>,
}
pub enum MessageShape { Params, Response }
pub struct EndpointLocation { pub service_index: usize, pub endpoint_index: usize }
pub struct Registry<'a> {
    pub(crate) model: &'a EndpointModel,
    pub(crate) services_by_id: BTreeMap<u16, usize>,
    pub(crate) services_by_name: BTreeMap<&'a str, usize>,
    pub(crate) endpoints_by_code: BTreeMap<u32, EndpointLocation>,
    pub(crate) endpoints_by_method: BTreeMap<String, EndpointLocation>,
    pub(crate) definitions_by_name: BTreeMap<&'a str, usize>,
    pub(crate) roles_by_name: BTreeMap<&'a str, usize>,
    pub(crate) errors_by_code: BTreeMap<u32, usize>,
}
impl<'a> Registry<'a> { pub fn model(&self) -> &'a EndpointModel; }
pub enum LegacyVersion { V1, V2 }
pub struct LegacyModelV1 {
    pub version: ModelVersion, pub services: Vec<ServiceSpec>,
    pub definitions: Vec<TypeDefinition>, pub roles: Vec<String>,
}
pub enum LegacyDocument { V1(LegacyModelV1), V2(EndpointModel) }
pub enum NameCase { Preserve, Snake, Camel, Pascal, ScreamingSnake, Kebab }
pub struct WireNamePolicy { pub case: NameCase, pub case_insensitive: bool }
pub struct WireName { pub source: String, pub wire: String }
pub enum PgType {
    Integer, BigInt, DoublePrecision, Boolean, Text, Bytea, Uuid, Inet, Jsonb, Numeric, TimestampMs,
}
impl PgType { pub const fn sql_name(self) -> &'static str; }
pub struct ProtocolHeader { pub name: String, pub value: String }
```

String, Vec, BTreeMap mean alloc types. ModelVersion is Clone + Copy + Debug +
Default + PartialEq + Eq + PartialOrd + Ord + Hash. Its Display is `major.minor.patch`
with no prefixes or padding. EndpointModel is Clone + Debug + Default + PartialEq
+ Eq; its default has version 0.0.0 and empty vectors. All other owning records
are Clone + Debug + PartialEq + Eq. Registry derives only Debug. The small enums,
EndpointLocation, and WireNamePolicy are Clone + Copy + Debug + PartialEq + Eq.
NameCase defaults to Preserve; WireNamePolicy defaults to Preserve/false.

PgType::sql_name returns, in variant order: integer, bigint, double precision,
boolean, text, bytea, uuid, inet, jsonb, numeric, bigint. TimestampMs deliberately
means epoch-milliseconds stored as bigint, with no timezone conversion.

Root internal helpers:

```rust
pub(crate) fn model_error(code: ErrorCode, detail: &'static str) -> Error;
pub(crate) fn endpoint_at(model: &EndpointModel, location: EndpointLocation) -> Option<&EndpointSchemaSpec>;
```

All local model failures use the ABI Error with SchemaCompile source, byte_offset
0, and schema/field ids 0 unless specific context is available. Details are
static strings, never leaked allocations. Endpoint codes and error codes have
separate namespaces. Neither this crate nor its adapters issue validation proofs.

## Wave 1: src/registry.rs

```rust
pub fn build_registry(model: &EndpointModel) -> Result<Registry<'_>>;
pub fn service_by_id<'a>(registry: &Registry<'a>, id: u16) -> Option<&'a ServiceSpec>;
pub fn service_by_name<'a>(registry: &Registry<'a>, name: &str) -> Option<&'a ServiceSpec>;
pub fn endpoint_by_code<'a>(registry: &Registry<'a>, code: u32) -> Option<&'a EndpointSchemaSpec>;
pub fn endpoint_by_method<'a>(registry: &Registry<'a>, method: &str) -> Option<&'a EndpointSchemaSpec>;
pub fn definition_by_name<'a>(registry: &Registry<'a>, name: &str) -> Option<&'a TypeDefinition>;
pub fn role_by_name<'a>(registry: &Registry<'a>, name: &str) -> Option<&'a RoleSpec>;
pub fn error_by_code<'a>(registry: &Registry<'a>, code: u32) -> Option<&'a ErrorSpec>;
pub fn resolve_type<'a>(registry: &Registry<'a>, ty: &'a EndpointType) -> Result<&'a EndpointType>;
pub fn is_authorized(registry: &Registry<'_>, endpoint: &EndpointSchemaSpec, granted_roles: &[&str]) -> Result<bool>;
```

build_registry validates the entire model before returning the immutable borrowed
index. It preserves model order and uses checked indices. Enforce unique service
ids/names, globally unique endpoint codes, unique endpoint names within a service,
unique canonical method strings, unique definition/role/error names, unique error
codes, and ASCII case-insensitive header names. Endpoint.service must equal its
enclosing service name exactly. Names for services, endpoints, definitions,
roles, enum variants, and error names are nonempty ASCII identifiers
`[A-Za-z_][A-Za-z0-9_]*` other than `_`; field names may be any nonempty UTF-8
string but must be unique within their record. Declared Struct/Enum names and
field/variant names must also be validated recursively. Top-level definition
name must match its inline Struct/Enum name when its type is inline nominal.
Inline nominal types share the definition namespace and cannot collide with a
different definition; repeated identical references use StructRef/EnumRef instead.

Validate every StructRef, EnumRef and StructTable reference, including deeply
nested Vec/Optional and stream_response. StructRef and StructTable resolve to a
Struct; EnumRef resolves to an Enum. Alias-only cycles are errors. Recursive
nominal structs are permitted: graph validation tracks visited definitions and
must not infinitely expand them. Traversal and alias dereferencing are bounded
by MAX_REFERENCE_DEPTH; exhaustion is DepthLimitExceeded. Nested inline ownership
traversal also observes this ceiling. NanoId length must be nonzero; enum names
and numeric discriminants must be unique. Field counts passed to schema mapping
are checked separately by utilities, not silently narrowed here.

Role includes must exist, be unique, and form an acyclic directed graph. Endpoint
roles must exist and be unique. Empty endpoint roles means public access.
is_authorized validates each supplied grant exists, computes transitive includes,
and succeeds if any endpoint role is present in that closure, or the endpoint
has no required role. An unknown grant/required role is ConstraintViolation,
never implicit authorization. It is valid to pass a registered endpoint clone;
its required role names are still checked. Query functions return None for a
missing key and never reinterpret a case or numeric id. resolve_type follows
reference aliases only, returning the final borrowed type; it does not expand
fields. Unknown refs, wrong target kind, duplicate model keys and role cycles
use ConstraintViolation. No partial registry is returned.

Method keys are wire_names::method_name(service.name, endpoint.name), with no
implicit casing. The registry must not depend on legacy or utilities. It may
call wire_names::method_name and utilities::normalize_header_name; neither helper
calls registry construction, so there is no initialization cycle.

## Wave 2: src/legacy.rs

```rust
pub fn import_legacy(document: LegacyDocument) -> Result<EndpointModel>;
pub fn export_legacy(model: &EndpointModel, version: LegacyVersion) -> Result<LegacyDocument>;
```

V1 uses canonical service/type shapes plus role-name strings. Import adds empty
RoleSpec.includes/description and empty errors/headers. V2 is the full canonical
EndpointModel shape. Validate through build_registry after import and before
export. Preserve every code, enum numeric value, string, field/service order,
stream type, and model version. Do not recase names or rewrite ids.

V1 export is allowed only if errors and headers are empty and every role has
empty includes and description. Otherwise return UnsupportedSchemaFeature;
never drop metadata to fit V1. Role strings preserve declaration order. V2
clones the model exactly. Unsupported future legacy versions must be handled
by the importing parser as explicit errors, not mapped to V1 or V2.

## Wave 3: src/wire_names.rs

```rust
pub fn convert_case(name: &str, case: NameCase) -> Result<String>;
pub fn resolve_wire_names(names: &[String], policy: WireNamePolicy) -> Result<Vec<WireName>>;
pub fn method_name(service: &str, endpoint: &str) -> Result<String>;
```

Preserve returns the exact nonempty UTF-8 input. Other cases require ASCII.
Split words at underscore, hyphen, or ASCII whitespace, at a lowercase-or-digit
followed by uppercase, and before the final uppercase in an acronym followed by
lowercase. Ignore empty words created by repeated separators; error if no word
remains. Non-alphanumeric ASCII characters outside those separators are rejected
for conversion. Snake lowercases and joins with _, Kebab lowercases and joins
with -, ScreamingSnake uppercases and joins with _, Pascal uppercases each word's
first character and lowercases the rest, Camel lowercases the first word and
uses Pascal for the rest. For example HTTPServerID -> http_server_id,
httpServerId, HttpServerId, HTTP_SERVER_ID, or http-server-id. Digits remain part
of their current word. Preserve never normalizes Unicode or punctuation.

resolve_wire_names returns the same length/order as input, retaining source and
computed wire value. Compare complete output bytes for collisions, or their
ASCII lowercase forms when case_insensitive. Return DuplicateObjectKey for any
collision, including repeated identical source names. Empty/invalid input uses
ConstraintViolation. No suffixes are invented to hide collisions. method_name
requires both inputs be the identifier form accepted by registry and returns
`service.endpoint`. It never applies a case policy, and a dot within either
component is an error. These algorithms also govern derive rename_all behavior.

## Wave 4: src/utilities.rs

```rust
pub fn pg_type(registry: &Registry<'_>, ty: &EndpointType) -> Result<PgType>;
pub fn normalize_header_name(name: &str) -> Result<String>;
pub fn validate_header(header: &ProtocolHeader) -> Result<()>;
pub fn fields_for(endpoint: &EndpointSchemaSpec, shape: MessageShape) -> &[EndpointField];
pub fn schema_for_fields(name: &str, schema_id: SchemaId, fields: &[EndpointField], reject_unknown: bool) -> Result<SchemaIr>;
```

PG mappings: UInt32 -> BigInt, Int32 -> Integer, Int64 -> BigInt, Float64 ->
DoublePrecision, Boolean -> Boolean, String/NanoId/blockchain address/hash ->
Text, Bytea -> Bytea, UUID -> Uuid, IpAddr -> Inet, TimeStampMs -> TimestampMs,
BlockchainDecimal -> Numeric, Enum -> Integer, Struct/Object/StructTable/Vec ->
Jsonb. Optional unwraps recursively; nominal refs resolve through the registry.
Unit has no SQL storage type and returns UnsupportedSchemaFeature. Resolve
cycles/depth errors exactly as registry. No SQL is executed or identifier text
interpolated into SQL. NanoId length is validation metadata, not a SQL type name.

Header names must be nonempty ASCII HTTP token characters, and are returned
ASCII lowercase. validate_header applies that check without rewriting its input
and permits only HTAB and ASCII 0x20..=0x7e in values. Reject CR/LF/NUL and other
bytes with ConstraintViolation. No header is serialized with unchecked text.
fields_for returns params or response unchanged and never the stream type.

schema_for_fields creates flat ABI IR in field declaration order, FieldId(index),
required_count checked to u16, exact supplied SchemaId/name/reject_unknown, and
unique nonempty field names. Int32/Int64/UInt32 map to Number with exact
signed-representable intrinsic bounds (I64 must include its full bounds to select
Integer in the schema-ir adapter); Float64 maps to Number without integer bounds;
Boolean maps to ScalarKind::True, which the authoritative schema-ir adapter
interprets as the Boolean category. String maps to String without constraints.
Optional unwraps the scalar and sets required=false: absence is allowed, but
explicit null is rejected. Nested Optional is rejected as unrepresentable.
UUID uses format uuid, IpAddr uses ip, Bytea uses base64, blockchain address uses
blockchain-address, and blockchain hash uses blockchain-transaction-hash. NanoId
uses format nanoid only for len=21, the compiler's fixed default FormatOptions;
other lengths return UnsupportedSchemaFeature. Check format availability with
jmax-format registry::lookup; preserve registry errors and attach schema context.
TimeStampMs is an i64 Number. Bytea's wire representation is canonical padded
standard base64, matching the format registry. Reject BlockchainDecimal (no
DecimalPolicy in this flat IR), Unit, Enum, and compound/reference types. Never
flatten nested records, claim explicit nullable semantics, or invent undocumented
format names. Unsupported semantics use UnsupportedSchemaFeature.

## Cross-module invariants

EndpointType, EndpointField, EndpointSchemaSpec, EnumVariantSpec, and ServiceSpec
are ABI identities throughout adapters and generated code. Registry owns no
copies of endpoint records, so its lifetime excludes mutating the indexed model.
Legacy conversion is explicit and loss checked. Casing is deterministic and
locale independent. Utilities never call build_registry internally; functions
requiring resolution take an already constructed index. Model errors remain the
canonical ABI Error, with deterministic declaration-order validation failures.
These contracts add metadata wrappers without extending the frozen ABI structs.

## Wave 5: src/native.rs

```rust
pub struct NativeIdent { pub type_name: String, pub wire: String, pub ident: String }
pub struct NativeSpec {
    pub roots: Vec<String>,
    pub tuples: Vec<String>,
    pub decimal_policy: DecimalPolicy,
    pub reject_unknown: bool,
    pub idents: Vec<NativeIdent>,
}
pub fn validate_native_spec(registry: &Registry<'_>, spec: &NativeSpec) -> Result<()>;
pub fn schema_graph_for(registry: &Registry<'_>, spec: &NativeSpec, root: &str, schema_id: SchemaId) -> Result<jmax_schema_ir::Schema>;
```

NativeSpec is a separate artifact from the endpoint model; it never changes a model's
meaning. It derives Clone + Debug + PartialEq + Eq; NativeIdent likewise. Default:
no roots, no tuples, DecimalPolicy::spot_8, reject_unknown true, no idents. roots lists
top-level definition names whose nominal struct becomes a decoder entry; tuples lists
definition names of structs encoded on the wire as a JSON array; decimal_policy applies
to every BlockchainDecimal; reject_unknown applies to every object. validate_native_spec
requires every root and tuple to name a definition that resolves to a Struct (uniqueness
and nonempty names included), every ident to name an existing field of a struct reachable
from a root with a legal snake_case non-keyword identifier, no two idents for one field and
no collision among the resulting identifiers, and DecimalPolicy to satisfy
jmax_decimal::validate_policy shape (precision and scale consistent). Failures are
ConstraintViolation; wire-name collisions are DuplicateObjectKey.

schema_graph_for lowers root, a root name from spec.roots, to a nested jmax_schema_ir::Schema
named after the root with the supplied SchemaId and runs jmax_schema_ir::model::validate. Mapping:
Struct (and StructRef/StructTable resolved through the registry) to Object with one Property per
field in declaration order, FieldId(index + 1) per object, required unless the field type is
Optional, and reject_unknown from the spec; Vec to Array with no bounds; Optional
unwraps its element and clears required (absence allowed, explicit null rejected, nested Optional
and Optional elements in arrays unsupported); a tuple struct to Array with min_items =
max_items = field count whose single items node is shared by all fields, which therefore must
lower to structurally equal nodes; BlockchainDecimal to Decimal with
DecimalConstraints{policy: decimal_policy, no bounds}; UInt32/Int32/Int64/TimeStampMs to
Integer with their exact signed-representable bounds (0..=u32::MAX, i32, i64); Float64 to
Number; Boolean to Boolean; String to String; UUID, IpAddr, BlockchainAddress and
BlockchainTransactionHash to String with the same format names as utilities::schema_for_fields;
NanoId only for len=21 as there; Bytea with format base64. Recursion, Enum, EnumRef, Object,
Unit, heterogeneous tuples and unavailable formats return UnsupportedSchemaFeature. Traversal
is bounded by MAX_REFERENCE_DEPTH (DepthLimitExceeded). The result contains no pattern and no
definitions or references; each nested object is its own node, so the graph is a tree.
