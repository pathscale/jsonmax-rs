# jmax-mcp frozen contract, version 1

The root owns every shared type and error constant. Modules import `super::*` and
use canonical ABI errors/owners. Module APIs below are exact. No additional
public module-local structs/enums or implicit runtime state are permitted.
This is an explicit baseline protocol implementation, not automatic adoption of
future protocol versions. The configured server version list is authoritative.

## Wave 1: envelope.rs

```rust
pub fn decode_message(payload: Payload, limits: EnvelopeLimits) -> McpResult<McpEnvelope>;
pub fn validate_json(payload: &Payload, limits: EnvelopeLimits) -> Result<()>;
pub fn object_field(payload: &Payload, name: &str, limits: EnvelopeLimits) -> Result<Option<Payload>>;
pub fn decode_string(payload: &Payload, limits: EnvelopeLimits) -> Result<String>;
```

All parsers consume one complete JSON value, reject trailing nonwhitespace,
invalid UTF-8/escapes/surrogates/numbers, and duplicate decoded keys at every
object level. EnvelopeLimits are nonzero and bound raw bytes, total decoded
string bytes, nesting, and fields per object. Unknown generic object fields
are permitted by validate_json and object_field; method-specific closed sets
are enforced by state. object_field requires an object and returns an owned copy
of the complete raw JSON value for a decoded matching key, without outer
whitespace; absence is None. decode_string requires a JSON string and returns its
decoded Unicode value. These helpers use canonical Result and precise offsets.

A message must be one JSON-RPC object with jsonrpc exactly "2.0", method a
nonempty string, optional id, and optional params. No batch arrays and no
response/result/error envelopes are accepted. Top-level allowed keys are
jsonrpc, id, method, params. Params, if present, is an object or array, not null.
IDs are a JSON string, null, or an exact signed i64 integer. Fraction/exponent,
quoted numbers interpreted as numbers, negative zero, overflow, and booleans
are rejected as IDs. String IDs preserve decoded contents; numeric IDs retain
numeric identity without floating point. An absent id is a notification, whereas
an explicit null is Some(McpId::Null). Params own their raw bytes independently
of input. No spans or input borrows are retained.

Malformed JSON maps to PARSE_ERROR with null id. A syntactically valid but
invalid envelope maps to INVALID_REQUEST; retain a valid id if it can be extracted
unambiguously, otherwise null. Duplicate top-level keys always make id
untrustworthy. Resource exhaustion maps to INTERNAL_ERROR. The canonical helper
errors remain available to other modules; decode_message converts only at the
MCP boundary. McpFailure has a human-readable bounded message without echoing
secret params. On failure for a valid notification, the host suppresses a reply.

## Wave 2: state.rs

```rust
pub fn route(
    session: &mut Session, message: &McpEnvelope, server: &ServerInfo,
    services: &[ServiceSpec], roles: &[String], limits: EnvelopeLimits,
) -> McpResult<McpAction>;
pub fn validate_call(
    action: &McpAction, services: &[ServiceSpec], roles: &[String],
    validator: &dyn ToolArgumentsValidator,
) -> McpResult<ValidatedSlots>;
```

Validate server configuration: nonempty implementation name/version; nonempty,
unique, nonempty-string protocol_versions in preference order; list_changed
requires tools. Validate global catalog identities and tool-name uniqueness,
matching service names, and nonempty unique role entries before routing. Invalid
configuration is INTERNAL_ERROR and never changes the session. Closed sessions
reject requests with SERVER_NOT_INITIALIZED. Protocol failures leave session
unchanged. The session is mutated only after all validation for that action.

* New + initialize request: params must be an object with protocolVersion:string,
  capabilities:object, clientInfo:{name:nonempty-string,version:nonempty-string},
  optional _meta:object. Other initialize/clientInfo fields are INVALID_PARAMS.
  If the offered version is in server.protocol_versions, select it; otherwise
  select the first configured version. Record selected version/client info and
  move to AwaitingInitialized. Return Initialize with request ID and version.
* AwaitingInitialized + notifications/initialized notification: params absent
  or object with only optional _meta:object; move to Ready, return Initialized.
  It must have no ID. Premature/repeated/wrong-shape initialized is INVALID_REQUEST
  and does not advance state. The host never replies to a notification failure.
* ping request is accepted in every nonclosed phase; params absent or empty
  object (optional _meta:object). Return Ping. It does not initialize the session.
* Ready + tools/list request: require server.capabilities.tools, params absent or
  object with only optional _meta:object. Return ListTools. No pagination is
  advertised; a cursor is INVALID_PARAMS.
* Ready + tools/call request: require tools capability. Params is an object with
  name:string, optional arguments:object, optional _meta:object and no other keys.
  Missing arguments becomes an owned `{}`. Resolve name via root tool_name over
  catalog, enforce exact any-role intersection, return CallTool with code and
  owned argument bytes. Unknown and forbidden names both yield INVALID_PARAMS
  with the same message, preventing role-based catalog disclosure.
* A repeated initialize request is INVALID_REQUEST. tools methods before Ready
  are SERVER_NOT_INITIALIZED. Disabled/unknown request methods are METHOD_NOT_FOUND.
  Notifications other than notifications/initialized are IgnoreNotification and
  never invoke tools, including request-only method names sent without IDs.

The caller serializes the initialization result before delivering
notifications/initialized. A transport failure closes/discards that Session;
route does not perform I/O or rollback a delivered initialization.
validate_call accepts only CallTool, repeats global identity and role checks by
code (roles may have changed), and invokes validator exactly once. Check identical
argument bytes, payload proof length, and decoded proof length. Validation
errors map to INVALID_PARAMS, bad ownership/proof to INTERNAL_ERROR; unauthorized
or missing codes use the same INVALID_PARAMS message. Other action kinds are
INVALID_REQUEST. No guessed schema fingerprint: the validator is responsible for
binding the endpoint's exact params schema and issuing the canonical proof.

## Wave 3: catalog.rs

```rust
pub fn list_tools(
    services: &[ServiceSpec], roles: &[String],
    schemas: &dyn ToolSchemaProvider, max_bytes: u32,
) -> Result<Vec<ToolDefinition>>;
pub fn find_tool<'a>(
    services: &'a [ServiceSpec], roles: &[String], name: &str,
) -> Result<&'a EndpointSchemaSpec>;
```

Validate the full catalog before filtering: unique service IDs/names, unique
global endpoint codes and service/name pairs, matching endpoint.service, valid
ASCII tool-name identifiers, and unique nonempty role entries. Public means
empty roles; otherwise admit exact any-role intersection. Preserve service and
endpoint order. For list_tools, call schemas.input_schema exactly once per
visible endpoint, never for hidden endpoints. Each returned schema must be one
complete JSON object, with no duplicate keys, valid JSON/UTF-8 and all references
resolving within that object; the provider owns schema correctness and named
reference resolution, while catalog validates object syntax and byte bounds.
The aggregate UTF-8 bytes of names, descriptions, and schema payloads must fit
max_bytes using checked arithmetic before cloning. Output codes/descriptions
are exact catalog values. find_tool returns ConstraintViolation for unknown
names and AuthFailed for forbidden names; state maps both to a common message.

Schema providers must use the same endpoint mapping as jmax-codegen-spec:
closed field objects, Optional omitted from required plus nullable value,
exact integer ranges, number/bool/string primitives, base64 Bytea, UUID format,
NanoId ASCII pattern and exact length, ipv4/ipv6 union, nested closed Struct,
locally resolved StructRef/EnumRef, StructTable additionalProperties ref, Vec
items, Unit null, numeric enum values with display-name/description extensions,
i64 millisecond timestamp, string decimal/address/hash projections. Decimal
pattern is `^-?[0-9]+(\.[0-9]+)?$`; no chain-specific assumptions. Hidden types
are not independently published. ToolDefinition contains no output schema for
baseline compatibility; structured results remain an explicit server option.

## Wave 4: results.rs

```rust
pub fn encode_response(response: &McpResponse<'_>, sink: &mut dyn ByteSink) -> Result<()>;
pub fn response_payload(response: &McpResponse<'_>, max_bytes: u32) -> Result<Payload>;
pub fn initialize_result(server: &ServerInfo, protocol_version: &str, max_bytes: u32) -> Result<Payload>;
pub fn tools_list_result(tools: &[ToolDefinition], max_bytes: u32) -> Result<Payload>;
pub fn tool_result_payload(result: &ToolResult<'_>, max_bytes: u32) -> Result<Payload>;
impl JsonEncode for McpResponse<'_> {
    fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()>;
}
```

Success envelope key order: jsonrpc, id, result. Error: jsonrpc, id, error;
error keys: code, message, optional data. Null ID is emitted as null, string IDs
are escaped, i64 IDs/codes use exact decimal text. No implicit newline or batch
wrapper. A response never contains both result and error. JsonEncode nested
values must emit exactly one JSON value; failures propagate. Streaming sink
failures may leave a prefix, which callers discard. Owned helpers return no
partial payload. BufferSink bounds every owned output.

initialize_result checks valid server configuration and that selected version
is configured; emit protocolVersion, capabilities, serverInfo, optional
instructions. capabilities is {} when tools=false, otherwise
{"tools":{"listChanged":bool}}; serverInfo has name, version. tools_list_result
emits {"tools":[{"name":...,"description":...,"inputSchema":...}]} with no
endpoint_code on the wire and no pagination cursor. Validate tool names,
uniqueness, and every raw input_schema object before writing it; these public
structs can be manually constructed. Tool result has content array entries
{"type":"text","text":...}, optional structuredContent object, then isError.
The structured encoder is first run into a bounded temporary sink and its output
validated as exactly one object before inclusion. Empty text and empty content
are allowed. isError is a tool execution outcome inside a successful RPC result;
JSON-RPC errors represent protocol/routing failures. A text-only result omits
structuredContent. No execution error is fabricated as successful data.

## Cross-module invariants and canonical errors

Methods and roles are case sensitive. Session is per connection and no global
state exists. Authorization precedes argument validation and tool execution.
Every owned output/input fragment is independent of a transient parser arena.
Use canonical Error codes for helper APIs: syntax codes and offsets propagate,
TypeMismatch for wrong JSON kind, ConstraintViolation for invalid config or
metadata, DuplicateObjectKey for identity collisions, PayloadTooLarge for byte
limits, DepthLimitExceeded for nesting, BudgetExhausted for allocation failure.
Local helper errors have source Wire; encode failures may retain source Encode.
McpFailure is the protocol-level error, not an ABI Error replacement. All error
responses to malformed/unknown IDs use null. Never reply to a valid notification.
Callers must supply JsonEncode implementations that obey their declared schema.

Acyclic dependencies: state/catalog/results may call envelope public helpers;
state may call catalog::find_tool; catalog never calls state or results; results
never calls state. Public APIs do not depend on another module's private types.
Allowed dependencies: jmax-abi, jmax-wire, jmax-endpoint-model, jmax-schema-vm,
jmax-encode. No jmax-codegen-spec dependency. Root code is no_std + alloc.

## Exact shared root source

All shared struct/enum definitions, root functions, constants, and trait methods
are frozen by this source; module implementations extend only the stated API.

```rust
//! JSON-RPC MCP boundaries with explicit initialization and role filtering.
#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::buffer::Payload;
pub use jmax_abi::endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, ServiceSpec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::ValidatedSlots;
pub use jmax_abi::traits::{ByteSink, JsonEncode};
pub use jmax_wire::{BufferSink, EnvelopeLimits, role_allowed, write_json_string};

pub mod envelope;
pub mod state;
pub mod catalog;
pub mod results;

pub const CONTRACT_VERSION: u16 = 1;
pub const JSONRPC_VERSION: &str = "2.0";
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
pub const SERVER_NOT_INITIALIZED: i32 = -32002;
/// Supported baseline, negotiated explicitly rather than inferred from a date.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum McpId {
    Number(i64),
    String(String),
    Null,
}

/// Absence of `id` is a notification; explicit null remains a request ID.
#[derive(Clone, Debug)]
pub struct McpEnvelope {
    pub id: Option<McpId>,
    pub method: String,
    /// Owned complete object or array JSON, never a borrowed input fragment.
    pub params: Option<Payload>,
}

impl McpEnvelope {
    pub fn is_notification(&self) -> bool { self.id.is_none() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    New,
    AwaitingInitialized,
    Ready,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplementationInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub tools: bool,
    pub tools_list_changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub implementation: ImplementationInfo,
    /// Preference order; nonempty, unique versions must be configured.
    pub protocol_versions: Vec<String>,
    pub capabilities: ServerCapabilities,
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub(crate) phase: SessionPhase,
    pub(crate) protocol_version: Option<String>,
    pub(crate) client: Option<ImplementationInfo>,
}

impl Default for Session {
    fn default() -> Self {
        Self { phase: SessionPhase::New, protocol_version: None, client: None }
    }
}

impl Session {
    pub fn phase(&self) -> SessionPhase { self.phase }
    pub fn protocol_version(&self) -> Option<&str> { self.protocol_version.as_deref() }
    pub fn client(&self) -> Option<&ImplementationInfo> { self.client.as_ref() }
    pub fn close(&mut self) { self.phase = SessionPhase::Closed; }
}

#[derive(Clone, Debug)]
pub enum McpAction {
    Initialize { id: McpId, protocol_version: String },
    Initialized,
    Ping { id: McpId },
    ListTools { id: McpId },
    CallTool { id: McpId, endpoint_code: u32, arguments: Payload },
    IgnoreNotification,
}

#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub endpoint_code: u32,
    pub input_schema: Payload,
}

/// Named types are supplied externally, avoiding a dependency on code generation.
pub trait ToolSchemaProvider {
    /// Return a complete JSON Schema object, resolving every reference locally.
    fn input_schema(&self, endpoint: &EndpointSchemaSpec, max_bytes: u32) -> Result<Payload>;
}

pub trait ToolArgumentsValidator {
    fn validate_arguments(&self, endpoint: &EndpointSchemaSpec, arguments: Payload) -> Result<ValidatedSlots>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpFailure {
    pub id: McpId,
    pub code: i32,
    pub message: String,
}

impl core::fmt::Display for McpFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "MCP error {}: {}", self.code, self.message)
    }
}

impl core::error::Error for McpFailure {}

pub type McpResult<T> = core::result::Result<T, McpFailure>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextContent<'a> {
    pub text: &'a str,
}

pub struct ToolResult<'a> {
    pub content: &'a [TextContent<'a>],
    /// The encoder must emit a JSON object, as required by structuredContent.
    pub structured_content: Option<&'a dyn JsonEncode>,
    pub is_error: bool,
}

pub enum McpResponse<'a> {
    Success { id: &'a McpId, result: &'a dyn JsonEncode },
    Error { id: &'a McpId, code: i32, message: &'a str, data: Option<&'a dyn JsonEncode> },
}

/// The name convention matches jmax-codegen-spec without depending on it.
pub fn tool_name(service: &str, endpoint: &str) -> Result<String> {
    fn valid(name: &str) -> bool {
        let mut bytes = name.bytes();
        matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }
    if !valid(service) || !valid(endpoint) {
        return Err(Error::new(ErrorCode::ConstraintViolation, ErrorSource::Wire, 0, "invalid tool identifier"));
    }
    let mut name = String::new();
    name.try_reserve(service.len().saturating_add(endpoint.len()).saturating_add(1))
        .map_err(|_| Error::new(ErrorCode::BudgetExhausted, ErrorSource::Wire, 0, "tool name allocation failed"))?;
    name.push_str(service);
    name.push('.');
    name.push_str(endpoint);
    Ok(name)
}

pub fn failure(id: Option<&McpId>, code: i32, message: &str) -> McpFailure {
    McpFailure { id: id.cloned().unwrap_or(McpId::Null), code, message: String::from(message) }
}
```
