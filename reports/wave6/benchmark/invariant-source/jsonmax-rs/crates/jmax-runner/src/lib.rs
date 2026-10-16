//! Scenario execution contracts shared by the runner's four implementation waves.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::fmt;
pub use jmax_abi::{
    buffer::Payload,
    endpoint::{EndpointSchemaSpec, ServiceSpec},
    error::{Error, ErrorCode, ErrorSource},
    syntax::Limits,
};

pub mod scenario;
pub mod drive;
pub mod coverage;
pub mod process;

pub const CONTRACT_VERSION: u32 = 1;
pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;
pub const MAX_SCENARIO_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_STEPS: usize = 100_000;
pub const REDACTED: &str = "[REDACTED]";
pub type Result<T> = core::result::Result<T, RunnerError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunnerError {
    InvalidScenario { step: Option<usize>, detail: String },
    UnknownConnection(String),
    UnknownResponse(String),
    Transport { connection: String, detail: String },
    Assertion { step: usize, detail: String },
    Timeout { operation: String },
    Process { detail: String },
    Io { operation: &'static str, detail: String },
    Json(Error),
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScenario { step, detail } => write!(f, "invalid scenario at {step:?}: {detail}"),
            Self::UnknownConnection(name) => write!(f, "unknown connection: {name}"),
            Self::UnknownResponse(name) => write!(f, "unknown response: {name}"),
            Self::Transport { connection, detail } => write!(f, "transport {connection}: {detail}"),
            Self::Assertion { step, detail } => write!(f, "assertion at step {step}: {detail}"),
            Self::Timeout { operation } => write!(f, "timeout: {operation}"),
            Self::Process { detail } => write!(f, "process: {detail}"),
            Self::Io { operation, detail } => write!(f, "I/O {operation}: {detail}"),
            Self::Json(error) => fmt::Display::fmt(error, f),
        }
    }
}
impl core::error::Error for RunnerError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Json(error) => Some(error), _ => None }
    }
}
impl From<Error> for RunnerError {
    fn from(error: Error) -> Self { Self::Json(error) }
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub timeout_ms: u32,
    pub steps: Vec<Step>,
}
#[derive(Clone, Debug)]
pub enum Step {
    Connect { name: String, uri: String, role: String },
    Call { connection: String, endpoint: u32, params: Payload, save_as: String },
    Receive { connection: String, save_as: String },
    Assert { response: String, path: String, expected: Expectation },
    Close { connection: String },
}
#[derive(Clone, Debug)]
pub enum Expectation {
    Exists,
    Missing,
    JsonEquals(Payload),
    ErrorCode(u32),
}
#[derive(Clone, Debug)]
pub struct CallRequest {
    pub endpoint: u32,
    pub sequence: u64,
    pub params: Payload,
}
#[derive(Clone, Debug)]
pub struct Reply {
    pub sequence: Option<u64>,
    pub error_code: Option<u32>,
    pub body: Payload,
}

/// Implementations own their I/O and must enforce the supplied relative deadline.
/// Errors must not contain credentials or unsanitized payload contents.
pub trait Transport {
    fn connect(&mut self, name: &str, uri: &str, role: &str, timeout_ms: u32) -> Result<()>;
    fn call(&mut self, name: &str, request: &CallRequest, timeout_ms: u32) -> Result<Reply>;
    fn receive(&mut self, name: &str, timeout_ms: u32) -> Result<Reply>;
    fn close(&mut self, name: &str) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoverageKey {
    pub role: String,
    pub endpoint: u32,
}
#[derive(Clone, Debug, Default)]
pub struct RunReport {
    pub scenario: String,
    pub steps_completed: usize,
    pub assertions_passed: usize,
    pub calls: BTreeMap<CoverageKey, u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoverageReport {
    pub required: alloc::collections::BTreeSet<CoverageKey>,
    pub covered: alloc::collections::BTreeSet<CoverageKey>,
    pub missing: alloc::collections::BTreeSet<CoverageKey>,
    pub unexpected: alloc::collections::BTreeSet<CoverageKey>,
}
impl CoverageReport {
    pub fn is_complete(&self) -> bool { self.missing.is_empty() }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub clear_env: bool,
    pub startup_timeout_ms: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState { Starting, Ready, Exited { code: Option<i32> } }

/// Redacts exact secret strings, longest first. Empty secrets are discarded.
#[derive(Clone, Debug, Default)]
pub struct RedactionPolicy { secrets: Vec<String> }
impl RedactionPolicy {
    pub fn new(mut secrets: Vec<String>) -> Self {
        secrets.retain(|value| !value.is_empty());
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Self { secrets }
    }
    pub fn redact(&self, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut offset = 0;
        while offset < input.len() {
            let remaining = &input[offset..];
            if let Some(secret) = self.secrets.iter().find(|secret| remaining.starts_with(secret.as_str())) {
                result.push_str(REDACTED);
                offset += secret.len();
            } else if let Some(ch) = remaining.chars().next() {
                result.push(ch);
                offset += ch.len_utf8();
            }
        }
        result
    }
}
