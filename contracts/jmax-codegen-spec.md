# jmax-codegen-spec frozen contract, version 1

Scope: the root owns every shared public type. Each wave file imports `super::*`.
No module may redefine ABI types, add a dependency, change this interface, or require
another wave's private helpers. Public functions below are the complete module API.
Implementations may share public functions in an acyclic direction: markdown and
protocol may call json_schema; services may call only root helpers. There is no I/O.
All fallible functions return canonical `jmax_abi::error::Result`.

## Wave 1: services.rs

```rust
pub fn public_services(services: &[ServiceSpec]) -> Result<Vec<ServiceSpec>>;
pub fn all_services(services: &[ServiceSpec]) -> Result<Vec<ServiceSpec>>;
pub fn select_services(services: &[ServiceSpec], visibility: Visibility<'_>) -> Result<Vec<ServiceSpec>>;
pub fn services_document(
    services: &[ServiceSpec],
    visibility: Visibility<'_>,
    options: &DocumentOptions,
) -> Result<GeneratedDocument>;
```

All four first call root `validate_services`, before applying visibility.
`public_services` is selection with Public; `all_services` is selection with All.
Selection clones whole matching endpoints and drops services with no visible
endpoints. Preserve service, endpoint, role, and field input order. Never mutate
input. `services_document` emits application/json with keys in this order:
`title`, `version`, `description`, `services`. Each service has `id`, `name`,
`endpoints`; each endpoint has `code`, `name`, `service`, `roles`, `params`,
`response`, `stream_response`, `description`. Each field has `name`, `ty`,
`description`. The EndpointType JSON representation is an object with `kind`
equal to the Rust variant name, followed by its named fields in ABI declaration
order. Tuple variants use `value` (StructRef string, Vec/Optional nested type).
Unit variants have only `kind`. Enum variant entries have `name`, `value`,
`description`. A missing stream_response is JSON null. Inline nesting is bounded
by max_type_depth. This catalog serialization does not resolve named references.

## Wave 2: json_schema.rs

```rust
pub fn validate_registry(registry: &TypeRegistry, max_depth: u16) -> Result<()>;
pub fn type_schema(
    ty: &EndpointType, registry: &TypeRegistry, options: &DocumentOptions,
) -> Result<GeneratedDocument>;
pub fn endpoint_schema(
    endpoint: &EndpointSchemaSpec, direction: SchemaDirection,
    registry: &TypeRegistry, options: &DocumentOptions,
) -> Result<GeneratedDocument>;
pub fn mcp_tools(
    services: &[ServiceSpec], registry: &TypeRegistry,
    visibility: Visibility<'_>, options: &DocumentOptions,
) -> Result<Vec<ToolSpec>>;
```

Every schema document is application/schema+json, with `$schema` set to the root
JSON_SCHEMA_DIALECT. Named definitions live in `$defs`; refs use escaped JSON
Pointer components. Only Struct definitions belong in registry.structs, and only
Enum definitions in registry.enums. Names must be valid_identifier, unique across
both lists and inline named definitions, unless an inline definition is exactly
equal to its registered definition. References must resolve to the correct kind.
Detect conflicting definitions and duplicate field/variant names or enum numeric
values. Recursive named graphs are legal and use refs, never recursive expansion.
max_depth bounds structural nesting, not graph traversal through a repeated ref.
Zero max_depth is invalid. NanoId length must be nonzero.

Field objects have properties, a required array in field order, and
additionalProperties:false. Outer Optional fields are not required; Optional
also permits null in any position. Unit is null. UInt32/Int32/Int64 are integer
with exact respective numeric bounds; Float64 is number; Boolean is boolean;
String is string; Bytea is string with contentEncoding:base64; UUID is string
format uuid; NanoId uses exact byte/ASCII length and pattern
`^[A-Za-z0-9_-]+$`; IpAddr is anyOf string format ipv4 and ipv6. Object is an
unconstrained JSON object. Struct is a closed field object. StructRef is a ref.
StructTable is an object with additionalProperties referencing its named struct.
Vec is array with items. Enum and EnumRef are integer enum schemas; descriptions
and names are retained in `x-enumNames` and `x-enumDescriptions`. prefixed_name
changes each display name to `<enum-name>.<variant-name>`, never wire values.
TimeStampMs is integer in i64 bounds with `x-unit:milliseconds`.
BlockchainDecimal is string with pattern `^-?[0-9]+(\.[0-9]+)?$`.
BlockchainAddress and BlockchainTransactionHash are nonempty strings, with
`x-jmax-type` equal to the variant name. Do not invent chain-specific constraints.

endpoint_schema projects params or response as a closed field object. mcp_tools
validates the entire catalog and registry, filters roles with Visibility, and
uses root tool_name, descriptions, codes, params schemas, and response schemas.
Tool names and schema semantics match jmax-mcp catalog. Every individual schema
and the aggregate bytes of all names, descriptions, and schema texts must fit
max_output_bytes. No hidden endpoint description or schema is emitted.

## Wave 3: protocol.rs

```rust
pub fn openapi(
    services: &[ServiceSpec], registry: &TypeRegistry,
    visibility: Visibility<'_>, options: &DocumentOptions,
) -> Result<GeneratedDocument>;
pub fn asyncapi(
    services: &[ServiceSpec], registry: &TypeRegistry,
    visibility: Visibility<'_>, options: &DocumentOptions,
) -> Result<GeneratedDocument>;
```

Both validate catalog/registry before projection and emit application/json.
OpenAPI uses OPENAPI_VERSION, info from options, optional servers:[{url}], and
POST `/rpc/<service>/<endpoint>` for each visible endpoint. operationId is its
tool_name. requestBody is required application/json params schema; response 200
is application/json response schema. Descriptions and service tags are preserved.
Use x-jmax-code and x-jmax-roles extensions; absent streaming is omitted, present
stream schema is x-jmax-stream-schema. No unsupported security scheme is invented.

AsyncAPI uses ASYNCAPI_VERSION and visible streaming endpoints only. Each channel
key is tool_name, address is `/rpc/<service>/<endpoint>`, and messages.result
contains a payload schema for stream_response. Each operation has action:send,
channel `$ref`, and its result message ref. Include info and optional server
entry `default` with host from server_url and protocol `https` or `http`, derived
from an absolute http(s) URL; reject another scheme or malformed URL. In OpenAPI
server_url must also be an absolute http(s) URL. Inline standalone schemas may
retain local $defs, but all nested refs must resolve within the final document:
rewrite refs to the actual schema location or hoist definitions into components
with collision-safe names. Never leave a `$ref` pointing at a nonexistent root.

## Wave 4: markdown.rs

```rust
pub fn service_docs(
    services: &[ServiceSpec], registry: &TypeRegistry,
    visibility: Visibility<'_>, options: &DocumentOptions,
) -> Result<GeneratedDocument>;
pub fn error_docs(errors: &[ErrorDoc], max_output_bytes: u32) -> Result<GeneratedDocument>;
pub fn systemd_unit(options: &SystemdOptions, max_output_bytes: u32) -> Result<GeneratedDocument>;
pub fn escape_markdown(text: &str) -> String;
```

service_docs validates all catalog and registry metadata and emits text/markdown
with title, version, description, service headings, endpoint headings, codes,
roles, parameter/response tables, and streaming type information. Preserve input
order. Escape untrusted table cells, headings, and link/code delimiters so names
or descriptions cannot inject Markdown structure or raw HTML. escape_markdown
escapes backslash, ASCII Markdown punctuation and HTML-sensitive `&<>`, replaces
CR/LF with spaces, and preserves other Unicode. Field tables state optionality.
error_docs emits code/name/description rows; reject duplicate codes or names,
empty names, and names failing valid_identifier. systemd_unit emits
text/plain: [Unit] Description/After=network-online.target, [Service] Type=simple,
ExecStart, optional User/WorkingDirectory, Restart=on-failure, RestartSec, and
[Install] WantedBy=multi-user.target. Executable and working directory must be
absolute paths. Quote executable and each argument separately with systemd
ExecStart quoting; encode literal percent as `%%`, literal dollar as `$$`,
and backslash-escape backslashes and double quotes. Quoting alone does not
suppress systemd specifier or environment expansion.
Reject NUL, CR, LF, other control characters, empty executable, and invalid user
(user must be a nonempty ASCII alphanumeric/underscore/hyphen identifier).
Description, User, WorkingDirectory must not allow directive injection. All
outputs end with one newline. No files or processes are created.

## Errors and cross-module invariants

Root validation errors have source Encode and offset zero. Duplicate identities,
field names, definition names, or error-doc names/codes use DuplicateObjectKey;
invalid metadata, invalid options, or unresolved references use
ConstraintViolation; unsupported projection semantics use UnsupportedSchemaFeature
rather than silently widening a schema. Excessive inline nesting uses
DepthLimitExceeded. Output byte limits use PayloadTooLarge, fallible allocation
failure uses BudgetExhausted. ByteSink errors propagate unchanged. No partial
GeneratedDocument is returned. Limits must be enforced while constructing output,
not only after allocating an unbounded document. Zero max_output_bytes permits
only an empty output (none of these document formats are empty).

Visibility is public when roles is empty, otherwise exact any-role intersection.
All bypasses role filtering, Public admits only empty roles. Authorization is not
inferred from names. tool_name is `<service>.<endpoint>`, using ASCII identifiers
`[A-Za-z_][A-Za-z0-9_]*`; this is injective and shared with MCP. All JSON strings
must use full JSON escaping, and numeric constants must be exact integer text.
Never encode i64 bounds through floating point. Stable input gives stable bytes;
no timestamps, random IDs, hash-map iteration, network access, or global state.
GeneratedDocument::new checks final byte length; it does not validate media type
or schema semantics. TypeRegistry lookups do not validate uniqueness; projections
must call validate_registry before resolving types. A stream schema is separate
from the immediate response object. Allowed dependencies: jmax-abi,
jmax-endpoint-model, jmax-schema-ir, jmax-encode. All roots support no_std + alloc.

## Exact shared root source

The following frozen source defines all shared structs, enums, constants, root
functions, visibility, and implementations. Canonical ABI items are reexports,
not local alternatives. Wave modules own only the function bodies declared above.

```rust
//! Deterministic, runtime-independent projections of endpoint specifications.
#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, EnumVariantSpec, ServiceSpec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::{SchemaFieldIr, SchemaIr};
pub use jmax_abi::traits::{ByteSink, JsonEncode};

pub mod services;
pub mod json_schema;
pub mod protocol;
pub mod markdown;

pub const CONTRACT_VERSION: u16 = 1;
pub const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
pub const OPENAPI_VERSION: &str = "3.1.0";
pub const ASYNCAPI_VERSION: &str = "3.0.0";

/// Empty endpoint role lists denote public access. Role matching is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility<'a> {
    All,
    Public,
    Roles(&'a [String]),
}

impl Visibility<'_> {
    pub fn includes(self, endpoint: &EndpointSchemaSpec) -> bool {
        match self {
            Self::All => true,
            Self::Public => endpoint.roles.is_empty(),
            Self::Roles(roles) => endpoint.roles.is_empty()
                || endpoint.roles.iter().any(|required| roles.iter().any(|role| role == required)),
        }
    }
}

/// Definitions used to resolve named endpoint types. References can be recursive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeRegistry {
    pub structs: Vec<EndpointType>,
    pub enums: Vec<EndpointType>,
}

impl TypeRegistry {
    pub fn struct_type(&self, name: &str) -> Option<&EndpointType> {
        self.structs.iter().find(|ty| matches!(ty, EndpointType::Struct { name: candidate, .. } if candidate == name))
    }

    pub fn enum_type(&self, name: &str) -> Option<&EndpointType> {
        self.enums.iter().find(|ty| matches!(ty, EndpointType::Enum { name: candidate, .. } if candidate == name))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentOptions {
    pub title: String,
    pub version: String,
    pub description: String,
    pub server_url: Option<String>,
    pub max_output_bytes: u32,
    pub max_type_depth: u16,
}

impl Default for DocumentOptions {
    fn default() -> Self {
        Self {
            title: String::from("Endpoint API"),
            version: String::from("1.0.0"),
            description: String::new(),
            server_url: None,
            max_output_bytes: 16 * 1024 * 1024,
            max_type_depth: 128,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedDocument {
    pub media_type: &'static str,
    pub text: String,
}

impl GeneratedDocument {
    /// Checks byte length, rather than Unicode scalar count.
    pub fn new(media_type: &'static str, text: String, max_bytes: u32) -> Result<Self> {
        if text.len() > max_bytes as usize {
            return Err(spec_error(ErrorCode::PayloadTooLarge, "generated document exceeds output limit"));
        }
        Ok(Self { media_type, text })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub endpoint_code: u32,
    /// A complete JSON Schema object, including any referenced definitions.
    pub input_schema: GeneratedDocument,
    pub output_schema: GeneratedDocument,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaDirection {
    Params,
    Response,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorDoc {
    pub code: i32,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdOptions {
    pub description: String,
    pub executable: String,
    pub arguments: Vec<String>,
    pub user: Option<String>,
    pub working_directory: Option<String>,
    pub restart_seconds: u32,
}

/// Stable and injective for names accepted by `valid_identifier`.
pub fn tool_name(service: &str, endpoint: &str) -> Result<String> {
    if !valid_identifier(service) || !valid_identifier(endpoint) {
        return Err(spec_error(ErrorCode::ConstraintViolation, "invalid service or endpoint identifier"));
    }
    let capacity = service.len().checked_add(endpoint.len()).and_then(|len| len.checked_add(1))
        .ok_or_else(|| spec_error(ErrorCode::BudgetExhausted, "tool name length overflow"))?;
    let mut name = String::new();
    name.try_reserve(capacity)
        .map_err(|_| spec_error(ErrorCode::BudgetExhausted, "tool name allocation failed"))?;
    name.push_str(service);
    name.push('.');
    name.push_str(endpoint);
    Ok(name)
}

pub fn valid_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Validates catalog identity before filtering, including hidden endpoints.
/// Type-reference resolution and type constraints are checked by projections.
pub fn validate_services(services: &[ServiceSpec]) -> Result<()> {
    for (index, service) in services.iter().enumerate() {
        if !valid_identifier(&service.name) {
            return Err(spec_error(ErrorCode::ConstraintViolation, "invalid service identifier"));
        }
        if services[..index].iter().any(|other| other.id == service.id || other.name == service.name) {
            return Err(spec_error(ErrorCode::DuplicateObjectKey, "duplicate service identity"));
        }
        for (endpoint_index, endpoint) in service.endpoints.iter().enumerate() {
            if endpoint.service != service.name || !valid_identifier(&endpoint.name) {
                return Err(spec_error(ErrorCode::ConstraintViolation, "endpoint service or name mismatch"));
            }
            if service.endpoints[..endpoint_index].iter().any(|other| other.name == endpoint.name) {
                return Err(spec_error(ErrorCode::DuplicateObjectKey, "duplicate endpoint name"));
            }
            if services[..index].iter().flat_map(|other| &other.endpoints)
                .chain(service.endpoints[..endpoint_index].iter())
                .any(|other| other.code == endpoint.code)
            {
                return Err(spec_error(ErrorCode::DuplicateObjectKey, "duplicate endpoint code"));
            }
            for (role_index, role) in endpoint.roles.iter().enumerate() {
                if role.is_empty() || endpoint.roles[..role_index].contains(role) {
                    return Err(spec_error(ErrorCode::ConstraintViolation, "empty or duplicate endpoint role"));
                }
            }
            for fields in [&endpoint.params, &endpoint.response] {
                for (field_index, field) in fields.iter().enumerate() {
                    if field.name.is_empty() || fields[..field_index].iter().any(|other| other.name == field.name) {
                        return Err(spec_error(ErrorCode::DuplicateObjectKey, "empty or duplicate field name"));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) const fn spec_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}
```
