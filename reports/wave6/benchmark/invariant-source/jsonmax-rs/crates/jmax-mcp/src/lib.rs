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
