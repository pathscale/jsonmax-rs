//! Model-driven parameter editing and explicit reactor-owned validator sessions.
#![forbid(unsafe_code)]

use std::{collections::BTreeMap, ffi::OsString, fmt, io::{self, BufRead, Write}, path::PathBuf};
pub use jmax_abi::{
    buffer::Payload,
    endpoint::{EndpointField, EndpointSchemaSpec, EndpointType, ServiceSpec},
    error::{Error as AbiError, ErrorCode, ErrorSource},
    syntax::Limits,
    traits::ResourceBudget,
};

pub mod load;
pub mod params;
pub mod session;
pub mod ui;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const MAX_MODEL_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_PENDING: u32 = 128;
pub const HELP: &str = "jmax-validator --services PATH [--preset NAME] [--service NAME --endpoint NAME] [--param NAME=VALUE] [--view list|native|tui]\n\n--services, -s   Version-1 service model file\n--preset         Named preset from the model\n--service        Select a service by exact name\n--endpoint       Select an endpoint by exact name\n--param, -p      Repeatable grammar-aware field input\n--view           list (default), native, or tui\n--help, -h       Show help\n--version, -V    Show version\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode { List, Native, Tui }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub services: PathBuf,
    pub preset: Option<String>,
    pub service: Option<String>,
    pub endpoint: Option<String>,
    pub parameters: Vec<(String, String)>,
    pub view: ViewMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command { Help, Version, Open(Options) }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointKey { pub service_id: u16, pub code: u32 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: String,
    pub endpoint: EndpointKey,
    pub parameters: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidatorModel { pub services: Vec<ServiceSpec>, pub presets: Vec<Preset> }

impl ValidatorModel {
    pub fn endpoint(&self, key: EndpointKey) -> Option<&EndpointSchemaSpec> {
        let mut services = self.services.iter().filter(|service| service.id == key.service_id);
        let service = services.next()?;
        if services.next().is_some() { return None; }
        let mut endpoints = service.endpoints.iter().filter(|endpoint| endpoint.code == key.code);
        let endpoint = endpoints.next()?;
        if endpoints.next().is_some() { return None; }
        Some(endpoint)
    }
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        let mut presets = self.presets.iter().filter(|preset| preset.name == name);
        let preset = presets.next()?;
        if presets.next().is_some() { return None; }
        Some(preset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterInput { pub name: String, pub text: String }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputGrammar { Text, Json }

#[derive(Clone, Debug)]
pub struct PreparedParams { pub endpoint: EndpointKey, pub payload: Payload }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPhase { Disconnected, Connecting, Ready, Closing, Closed, Failed }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig { pub max_pending: u32, pub budget: ResourceBudget }

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_pending: DEFAULT_MAX_PENDING,
            budget: ResourceBudget { cpu_units: 1_000_000, retained_bytes: 8 * 1024 * 1024, timeout_ms: 30_000 },
        }
    }
}

#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub sequence: u64,
    pub endpoint: EndpointKey,
    pub streaming: bool,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Connected,
    Immediate { sequence: u64, payload: Payload },
    Stream { sequence: u64, payload: Payload },
    RemoteError { sequence: u64, payload: Payload },
    StreamClosed { sequence: u64 },
    Closed,
}

/// The host owns polling, scheduling, connection establishment, and credentials.
/// Each method must return promptly without waiting for network progress.
pub trait SessionReactor {
    fn connect(&mut self) -> ValidatorResult<()>;
    fn send(&mut self, sequence: u64, params: PreparedParams, budget: ResourceBudget) -> ValidatorResult<()>;
    fn poll_event(&mut self) -> ValidatorResult<Option<SessionEvent>>;
    fn close(&mut self) -> ValidatorResult<()>;
}

#[derive(Debug)]
pub struct Session {
    pub(crate) phase: SessionPhase,
    pub(crate) next_sequence: u64,
    pub(crate) pending: BTreeMap<u64, PendingRequest>,
    pub(crate) config: SessionConfig,
}

impl Session {
    pub fn phase(&self) -> SessionPhase { self.phase }
    pub fn pending(&self) -> &BTreeMap<u64, PendingRequest> { &self.pending }
    pub fn config(&self) -> SessionConfig { self.config }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointRow {
    pub key: EndpointKey,
    pub service: String,
    pub endpoint: String,
    pub description: String,
}

#[derive(Debug)]
pub enum ValidatorError {
    Usage(String),
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    Validation(AbiError),
    State { phase: SessionPhase, detail: &'static str },
}

pub type ValidatorResult<T> = std::result::Result<T, ValidatorError>;

impl fmt::Display for ValidatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => f.write_str(message),
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::Validation(error) => write!(f, "{error}"),
            Self::State { phase, detail } => write!(f, "session {phase:?}: {detail}"),
        }
    }
}

impl std::error::Error for ValidatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Validation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<AbiError> for ValidatorError { fn from(error: AbiError) -> Self { Self::Validation(error) } }

/// Arguments exclude argv[0]. Paths retain their native operating-system encoding.
pub fn parse_args<I>(arguments: I) -> ValidatorResult<Command>
where I: IntoIterator<Item = OsString> {
    let mut arguments = arguments.into_iter();
    let mut services = None;
    let mut preset = None;
    let mut service = None;
    let mut endpoint = None;
    let mut parameters = Vec::new();
    let mut view = ViewMode::List;
    let mut view_seen = false;
    while let Some(argument) = arguments.next() {
        let option = argument.to_str().ok_or_else(|| ValidatorError::Usage("option names must be UTF-8".into()))?;
        if matches!(option, "--help" | "-h") { return Ok(Command::Help); }
        if matches!(option, "--version" | "-V") { return Ok(Command::Version); }
        if !matches!(option, "--services" | "-s" | "--preset" | "--service" | "--endpoint" | "--param" | "-p" | "--view") {
            return Err(ValidatorError::Usage(format!("unknown option: {option}")));
        }
        let value = arguments.next().ok_or_else(|| ValidatorError::Usage(format!("missing value for {option}")))?;
        if matches!(option, "--services" | "-s") {
            if services.is_some() { return Err(ValidatorError::Usage("duplicate --services".into())); }
            if value.is_empty() { return Err(ValidatorError::Usage("empty services path".into())); }
            services = Some(PathBuf::from(value));
            continue;
        }
        let value = value.into_string().map_err(|_| ValidatorError::Usage(format!("value for {option} must be UTF-8")))?;
        match option {
            "--preset" => set_once(&mut preset, value, option)?,
            "--service" => set_once(&mut service, value, option)?,
            "--endpoint" => set_once(&mut endpoint, value, option)?,
            "--param" | "-p" => {
                let (name, text) = value.split_once('=').ok_or_else(|| ValidatorError::Usage("parameters require NAME=VALUE".into()))?;
                if name.is_empty() || parameters.iter().any(|(existing, _)| existing == name) {
                    return Err(ValidatorError::Usage("empty or duplicate parameter name".into()));
                }
                parameters.push((name.to_owned(), text.to_owned()));
            }
            "--view" => {
                if view_seen { return Err(ValidatorError::Usage("duplicate --view".into())); }
                view_seen = true;
                view = match value.as_str() {
                    "list" => ViewMode::List,
                    "native" => ViewMode::Native,
                    "tui" => ViewMode::Tui,
                    _ => return Err(ValidatorError::Usage("view must be list, native, or tui".into())),
                };
            }
            _ => return Err(ValidatorError::Usage(format!("unknown option: {option}"))),
        }
    }
    if service.is_some() != endpoint.is_some() {
        return Err(ValidatorError::Usage("--service and --endpoint must be supplied together".into()));
    }
    if preset.is_some() && service.is_some() {
        return Err(ValidatorError::Usage("a preset and explicit endpoint selection are mutually exclusive".into()));
    }
    if !parameters.is_empty() && preset.is_none() && endpoint.is_none() {
        return Err(ValidatorError::Usage("parameters require an endpoint or preset".into()));
    }
    Ok(Command::Open(Options {
        services: services.ok_or_else(|| ValidatorError::Usage("--services is required".into()))?,
        preset, service, endpoint, parameters, view,
    }))
}

fn set_once(target: &mut Option<String>, value: String, option: &str) -> ValidatorResult<()> {
    if value.is_empty() || target.is_some() {
        return Err(ValidatorError::Usage(format!("empty or duplicate {option}")));
    }
    *target = Some(value);
    Ok(())
}

pub fn run<I>(arguments: I, input: &mut dyn BufRead, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8
where I: IntoIterator<Item = OsString> {
    match execute(arguments, input, stdout) {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            let code = if matches!(&error, ValidatorError::Usage(_)) { EXIT_USAGE } else { EXIT_FAILURE };
            let _ = writeln!(stderr, "jmax-validator: {error}");
            code
        }
    }
}

fn execute<I>(arguments: I, input: &mut dyn BufRead, stdout: &mut dyn Write) -> ValidatorResult<()>
where I: IntoIterator<Item = OsString> {
    let print_error = |source| ValidatorError::Io { operation: "write stdout", path: PathBuf::from("<stdout>"), source };
    match parse_args(arguments)? {
        Command::Help => stdout.write_all(HELP.as_bytes()).map_err(print_error),
        Command::Version => writeln!(stdout, "jmax-validator {}", env!("CARGO_PKG_VERSION")).map_err(print_error),
        Command::Open(options) => {
            let model = load::load_model(&options.services, Limits::strict_default())?;
            ui::run(&model, &options, input, stdout, None)
        }
    }
}
