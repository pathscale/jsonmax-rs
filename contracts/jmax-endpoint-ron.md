# jmax-endpoint-ron: frozen Wave 0 contract

Owner: W0-ROOTS-5, row 24. Source of truth:
`crates/jmax-endpoint-ron/src/lib.rs`. Public modules: load, normalize, validate,
version. Allowed dependencies: jmax-abi, jmax-endpoint-model,
jmax-schema-compile. No external RON/TOML/serialization parser dependency is
available; implement the bounded grammar below directly. Parsing and normalization
work with alloc without std. Filesystem APIs exist only with feature std.

## Root definitions and exports

Error, ErrorCode, ErrorSource, Result are canonical ABI re-exports. EndpointField,
EndpointModel, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ErrorSpec,
HeaderSpec, ModelVersion, RoleSpec, ServiceSpec, TypeDefinition are re-exported
from endpoint-model, with no replacement endpoint types.

```rust
pub const CONTRACT_VERSION: u16 = 1;
pub const CONFIG_FORMAT_VERSION: u32 = 1;
pub const RUNTIME_VERSION: ModelVersion = ModelVersion::new(0, 1, 0);
pub struct ConfigLimits {
    pub max_input_bytes: u32, pub max_depth: u16, pub max_nodes: u32,
    pub max_string_bytes: u32, pub max_collection_items: u32,
}
pub struct LoadOptions { pub limits: ConfigLimits, pub require_version: bool }
pub struct ConfigNode { pub offset: u32, pub len: u32, pub value: ConfigValue }
pub enum ConfigValue {
    Unit, Bool(bool), U64(u64), I64(i64), String(String), Identifier(String),
    Sequence(Vec<ConfigNode>), Tuple(Vec<ConfigNode>), Record(Vec<(String, ConfigNode)>),
    Variant { name: String, value: Box<ConfigNode> },
}
pub struct LoadedConfig { pub source_name: String, pub root: ConfigNode }
pub struct ConfigDefaults { pub service: Option<String>, pub roles: Vec<String>, pub description: String }
pub struct VersionRequirements {
    pub format_version: u32, pub model_version: ModelVersion, pub minimum_runtime: ModelVersion,
}
pub fn load_model(source: &str, version_toml: Option<&str>, options: &LoadOptions) -> Result<EndpointModel>;
pub fn validate_input_size(input: &str, limits: &ConfigLimits) -> Result<()>;
pub(crate) fn config_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error;
```

String/Vec/Box are alloc types. ConfigLimits, LoadOptions, VersionRequirements
are Clone + Copy + Debug + PartialEq + Eq. All syntax/default records and
ConfigValue are Clone + Debug + PartialEq + Eq. ConfigDefaults additionally
implements Default with None/empty vectors/empty description. ConfigLimits
Default: 16 MiB input, depth 128, 1,000,000 nodes, 1 MiB decoded string bytes,
100,000 entries per collection. LoadOptions Default uses those limits and
requires a version document. All fields are public.

Offsets and lengths are UTF-8 input byte ranges, not character counts. A variant
node covers its identifier and payload; its boxed child covers the parenthesized
payload. LoadedConfig keeps decoded syntax and a source label, not an input
borrow or unbounded copy of the original document. Source labels are diagnostic
data and are not automatically opened as paths.

load_model is implemented in the root: parse an optional version document first,
reject its absence when require_version, parse config, normalize, validate model,
then validate version requirements. Return no partial model. Supplying a version
document always validates it even when require_version is false. The explicit
version fields in the config are never silently overwritten by version.toml.
validate_input_size rejects lengths above u32 or the configured byte ceiling
with PayloadTooLarge, and zero max_depth/max_nodes with ConstraintViolation.

All local errors are canonical ABI Error, SchemaCompile source. Parse errors
have their offending input byte offset or input length for EOF. Semantic errors
use the relevant ConfigNode offset where available; post-normalization errors
use offset 0. Static detail strings describe the failure without leaking an
allocation. Schema/field context is added when known.

## Wave 1: src/load.rs

```rust
pub fn parse_str(input: &str, source_name: &str, limits: &ConfigLimits) -> Result<LoadedConfig>;
pub fn parse_bytes(input: &[u8], source_name: &str, limits: &ConfigLimits) -> Result<LoadedConfig>;
#[cfg(feature = "std")]
pub fn load_file(path: &std::path::Path, limits: &ConfigLimits) -> Result<LoadedConfig>;
#[cfg(feature = "std")]
pub fn load_model_files(config_path: &std::path::Path, version_path: Option<&std::path::Path>, options: &LoadOptions) -> Result<EndpointModel>;
```

The supported RON configuration grammar is deliberately explicit:

- Unicode whitespace; `//` line comments; nested `/* ... */` block comments.
  Comment nesting also obeys max_depth. Unclosed comments fail at EOF.
- ASCII identifiers `[A-Za-z_][A-Za-z0-9_]*`, true/false, and decimal integers.
  Optional leading + or - is accepted; positive values use U64, negative values
  I64. Minus zero is I64(0). No arithmetic expressions or implicit conversions.
- Quoted UTF-8 strings with escapes `\"`, `\\`, `\n`, `\r`, `\t`, `\0`, and
  Rust-style `\u{1..6 hex digits}`. Reject surrogate/non-scalar values and
  unescaped control bytes. Enforce max_string_bytes on decoded bytes before
  growth. Raw strings, byte strings, char literals, floats, radix-prefixed or
  underscored integers, and extension directives are explicitly unsupported.
- `[value, ...]` creates Sequence. `(value, ...)` creates Tuple, including a
  one-element tuple. `()` creates Unit. `(key: value, ...)` and `{key: value, ...}`
  create Record, with identifier or quoted-string keys. A single unnamed
  `(value)` is treated as a one-element Tuple for variant argument handling.
- `Identifier(...)` creates Variant with a Unit, Tuple, or Record payload node.
  Bare identifiers create Identifier nodes. `None`, scalar type tags, and
  enum tags remain identifiers for normalization; parsing is schema independent.
- Collections allow trailing commas. No duplicate record keys, mixed named and
  unnamed payloads, implicit concatenation, or content after the root value.

Count every ConfigNode, including variant payload nodes, against max_nodes.
Count each sequence/tuple element and record pair against the per-collection
limit. Apply depth before descending or pushing a parser frame; never recurse
on unbounded input. Offsets and lengths use checked arithmetic. A source label
must not exceed max_string_bytes. Parsing must consume all input apart from
whitespace/comments. No regex, recursive exponential backtracking, includes,
environment expansion, or executable config expressions.

Errors: InvalidSyntax for malformed punctuation/numbers, UnexpectedEof for an
unfinished collection/comment, UnclosedString for unfinished strings,
InvalidEscape/LoneSurrogate/ControlCharInString for string failures, InvalidUtf8
for parse_bytes with invalid UTF-8, NumberOverflow for integers beyond the
selected 64-bit domain, DuplicateObjectKey for repeated record keys,
DepthLimitExceeded for nesting, BudgetExhausted for node/item ceilings,
DecodedArenaOverflow for decoded string ceilings, TrailingContent after a root,
and UnsupportedSchemaFeature for recognized unsupported syntax. Limit checks
precede allocations and narrowing conversions. parse_bytes enforces input byte
limits before UTF-8 decoding. It reports the first invalid byte.

load_file opens only the supplied path and reads at most max_input_bytes + 1
bytes with checked arithmetic. It rejects oversized input before an unbounded
allocation. load_model_files reads config with the configured limit and version
metadata with a 64 KiB ceiling, then uses the root load_model pipeline. Missing
required version_path is MissingRequiredField. File I/O failures use
ConstraintViolation with a static read/open detail. Neither API walks directories,
infers includes, follows configuration-supplied paths, or writes files.

## Wave 2: src/normalize.rs

```rust
pub fn normalize(config: &LoadedConfig) -> Result<EndpointModel>;
pub fn normalize_type(node: &ConfigNode) -> Result<EndpointType>;
pub fn normalize_defaults(node: &ConfigNode) -> Result<ConfigDefaults>;
```

Normalization is pure and deterministic. It validates shapes, numeric widths,
required keys and duplicate/unknown keys even for manually constructed syntax
trees. Every record listed below accepts a bare Record or the exact named
wrapper given in parentheses. No arbitrary wrapper name is discarded.

Top-level (Config or EndpointModel): `version` required, `services`, `definitions`,
`roles`, `errors`, `headers` default to empty sequences; `defaults` defaults to
ConfigDefaults::default(). Version is a quoted strict three-component version
or a record/wrapper Version with required u32 major/minor/patch. Top-level defaults
apply only to omitted values; an explicit empty string/vector overrides them.

- Defaults: optional `service` (string or None/Some(string)), `roles` string
  sequence, `description` string. No other keys.
- Service: required u16 `id`; `name` required unless defaults.service is Some;
  `endpoints` defaults empty. No automatically allocated service ids.
- Endpoint: required u32 `code`; exactly one of `name` or `method` supplies the
  endpoint name. `service` defaults to its enclosing service name and must match
  it if supplied. `roles` defaults from defaults.roles. `params`/`response`
  default empty Field sequences; `stream_response` defaults None and accepts
  None, Some(type), or a direct type node; `description` defaults from defaults.
  A fully qualified method is not accepted as a name: methods are constructed
  from enclosing service plus name by endpoint-model wire_names.
- Definition: required string `name` and required `ty` type node.
- Field: required string `name` and required `ty`; `description` defaults empty.
- Role: required string `name`; `includes` defaults empty string sequence;
  `description` defaults empty. A bare string role is also accepted and expands
  to those empty metadata values, preserving legacy role lists.
- Error: required u32 `code`, string `name`, string `message`.
- Header: required string `name`, type `ty`; `required` defaults true;
  `description` defaults empty.
- EnumVariant: required string `name`, i32 `value`; `description` defaults empty.

No endpoint-specific error reference field exists in the ABI. Reject such input
as UnknownObjectKey rather than discarding it. Errors are model-global ErrorSpec
records. Defaults cannot invent codes, types, references, or role declarations.

Scalar EndpointType nodes are exactly the identifiers UInt32, Int32, Int64,
Float64, Boolean, String, Bytea, UUID, IpAddr, Object, Unit, TimeStampMs,
BlockchainDecimal, BlockchainAddress, BlockchainTransactionHash. Payload variants:

```text
NanoId(len: <u8>)
Struct(name: <string>, fields: [<Field>, ...])
StructRef(<string>)
StructTable(struct_ref: <string>)
Vec(<type>)
Optional(<type>)
Enum(name: <string>, variants: [<EnumVariant>, ...])
EnumRef(name: <string>, prefixed_name: <bool>)
```

Payload record keys are required except Struct.fields/Enum.variants default empty
and EnumRef.prefixed_name defaults false. StructRef/Vec/Optional require exactly
one tuple argument. No strings are interpreted as type expressions, no
misspelled tag is treated as a ref, and no unknown keys are ignored. Preserve
all field/variant/service ordering and descriptions verbatim. StructRef and
EnumRef names are not resolved during normalization. No normalization step
changes spelling/casing, assigns duplicate codes new values, or guesses widths.

Errors: MissingRequiredField, UnknownObjectKey, DuplicateObjectKey, TypeMismatch,
NumberOverflow, UnsupportedSchemaFeature for unknown type tags, and
ConstraintViolation for conflicting aliases or malformed option wrappers. Type
nesting is bounded by endpoint-model MAX_REFERENCE_DEPTH even when callers
construct ConfigNode directly. Empty required names are rejected. Return a
complete model or an error; no partial/default-filled substitute is returned.

## Wave 3: src/validate.rs

```rust
pub fn validate_model(model: &EndpointModel) -> Result<()>;
```

Delegate canonical name/reference/role/header/enum/code invariants to
endpoint-model::registry::build_registry. Validate every endpoint, parameter,
response, stream type, definition, role, header and error, including unused
records. Duplicate endpoint codes are global across services. service.name and
endpoint.service agree; method keys are canonical `service.endpoint` and unique.
Do not reject legal zero ids/codes merely because they are zero. All error names
and codes are unique in their own namespaces. There are no endpoint-local error
refs to validate because the ABI cannot represent them; normalization rejects
those unsupported keys. Roles, includes, reference kind checks, legal nominal
recursion, alias-only cycle rejection, and maximum reference depth use exactly
the endpoint-model contract. Validation has no filesystem or runtime side effect
and makes no JSON payload validation proof.

## Wave 4: src/version.rs

```rust
pub fn parse_semver(value: &str) -> Result<ModelVersion>;
pub fn parse_version(input: &str) -> Result<VersionRequirements>;
pub fn validate_version(model: &EndpointModel, requirements: &VersionRequirements) -> Result<()>;
```

parse_semver accepts exactly three nonempty ASCII decimal u32 components separated
by dots, with no leading zeros except 0 itself, signs, whitespace, prerelease,
build metadata, or v prefix. All three components are mandatory. Bad spelling
uses InvalidSyntax; component overflow uses NumberOverflow.

The supported version.toml document has exactly three required top-level keys,
with optional horizontal whitespace, blank lines, LF or CRLF, and # comments
outside quoted strings:

```toml
format_version = 1
model_version = "1.2.3"
minimum_runtime = "0.1.0"
```

format_version is an unsigned decimal u32. The two version strings are plain
double-quoted strings containing strict parse_semver input, with no escapes.
Reject duplicate keys (DuplicateObjectKey), unknown keys (UnknownObjectKey),
missing keys (MissingRequiredField), tables/dotted keys/arrays/multiline values
(UnsupportedSchemaFeature), malformed syntax (InvalidSyntax), and more than
64 KiB or u32-addressable length (PayloadTooLarge). No TOML extension is silently
accepted or mapped to defaults. Track input byte offsets in version parse errors.

validate_version requires format_version == CONFIG_FORMAT_VERSION, exact
model.version == requirements.model_version, and minimum_runtime <=
RUNTIME_VERSION under lexicographic (major, minor, patch) ordering. Unsupported
format/future runtime requirements use UnsupportedSchemaFeature; a model version
mismatch uses ConstraintViolation. Model and runtime versions are separate
namespaces. No filesystem lookup or implicit fallback to a neighboring file.

## Cross-module invariants

The dependency path is load_model -> load/normalize/validate/version;
normalize may call version::parse_semver; validate calls endpoint-model registry;
version does not call normalize/load. load's file wrapper may call root load_model
but parse_str must never do so, avoiding recursion. Use canonical ABI endpoint
types only after normalization. Every size conversion is checked, resource
ceilings apply before growth, missing fields differ from explicitly empty fields,
and unsupported input fails closed. No wave edits root definitions, adds parser
dependencies, or issues a payload validation proof.

## Native specification loading

```rust
pub fn load_native_spec(source: &str, limits: &ConfigLimits) -> Result<NativeSpec>;
```

Parses a separate `native.ron` document with the Wave 1 grammar and normalizes it. The root is
a record (optionally wrapped as `NativeSpec(...)` or `Native(...)`) with keys `roots`,
`tuples` (sequences of quoted names), `decimal` (optional record `(max_precision, target_scale,
allow_exponent, reject_inexact_rescale, tick_step)`, default DecimalPolicy::spot_8),
`reject_unknown` (boolean, default true), `idents` (sequence of `Ident(type, wire, ident)`
tuples or `(type: .., wire: .., ident: ..)` records), `nanoid_alphabet` (the identifier `Url` or `Base62`, default Url)
`nullable_optionals` (boolean, default false), and `nanoid_fields` (sequence of `NanoIdField(type, wire, alphabet)` tuples or `(type: .., wire: .., alphabet: ..)` records). Unknown keys are UnknownObjectKey,
duplicate keys DuplicateObjectKey, wrong value kinds TypeMismatch, integer overflow
NumberOverflow, an unfinished document UnexpectedEof, all with the offending node offset and
SchemaCompile source. No name is resolved here: model-dependent validation is
endpoint-model validate_native_spec, so loading never needs a model. The loader leaves the
model grammar and load_model untouched. Limits are enforced as for load_model.
