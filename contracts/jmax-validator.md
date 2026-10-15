# jmax-validator: frozen Wave 0 contract

Status: W0-ROOTS-7. This is a std host tool with a stream-injected library driver. The manifest's std/alloc features forward ABI settings; the validator CLI itself requires std. All shared types live in lib.rs and are shown exactly in the root snapshot. Each of load, params, session, and ui imports `super::*`; none defines a competing endpoint model, JSON owner, error ABI, budget, or session state type. EndpointType and every nested field retain the frozen ABI definition.

## Driver, CLI, and shared state

The root implements parse_args and run completely. Arguments exclude argv[0]. Exact supported options are in HELP: --services/-s PATH, --preset NAME, --service NAME, --endpoint NAME, repeatable --param/-p NAME=VALUE, --view list|native|tui, --help/-h, --version/-V. This parser accepts separate values only, with no equals option syntax or positional arguments. Parameter text is split only on the first '='; empty parameter values are valid text, while names must be nonempty and unique. All singleton options reject duplicates/empty values. Native services paths retain non-UTF-8 OsString encoding; other values and option names require UTF-8. Help/version return immediately when encountered and do not load files. Service and endpoint selectors must be supplied together; they are mutually exclusive with a preset. Parameter overrides require a selection. The default view is List. --services is required for Open.

run routes usage errors to exit 2, operational/model/state/validation errors to exit 1, success to exit 0, with one diagnostic prefix on stderr. Help/version and UI output use stdout. Library callers may bypass run and inspect ValidatorError. Its Io and Validation variants retain original source errors. Root run loads the model with strict limits, then calls ui::run with no reactor. The standalone CLI supports model inspection and parameter validation offline. An embedded host passes a concrete reactor into ui::run for live requests; there is no hidden global connector, implicit event loop, credential prompt, or executor acquisition. This explicit seam is the integration contract, not an automatically created network session.

ValidatorModel::endpoint and preset return None on ambiguous matches rather than choosing a duplicate. Full semantic diagnostics belong to load::validate_model. Session is only state; SessionReactor owns connections, polling, progress, timeouts, and credentials. Session does not borrow the UI/model and is not itself a reactor handle. The host's local reactor binding must remain local, as in jmax-wss::ReactorBinding. Transport payloads are owned Payload values. PreparedParams deliberately has public fields so callers can inspect/build it; session must revalidate it before sending, rather than treating its type as a proof.

## Wave 1: load.rs

```rust
pub const MODEL_VERSION: u16 = 1;
pub fn load_model(path: &std::path::Path, limits: Limits) -> ValidatorResult<ValidatorModel>;
pub fn parse_model(payload: Payload, limits: Limits) -> ValidatorResult<ValidatorModel>;
pub fn validate_model(model: &ValidatorModel, limits: Limits) -> ValidatorResult<()>;
pub fn select_endpoint(model: &ValidatorModel, service: &str, endpoint: &str) -> ValidatorResult<EndpointKey>;
pub fn select_preset<'a>(model: &'a ValidatorModel, name: &str) -> ValidatorResult<&'a Preset>;
```

load_model reads at most min(MAX_MODEL_BYTES, limits.max_payload_bytes) with a bounded read that detects an extra byte, then delegates to parse_model. Never read_to_end without a bound or use a lossy conversion of input bytes. Missing/unreadable paths are ValidatorError::Io with operation/path/source. parse_model uses the allowed DOM/compat dependencies for strict whole-document JSON parsing. No trailing content, comments, duplicate keys, or unvalidated subtree is allowed. Follow all Limits fields including decoded arena, nesting, array counts, and field/string sizes. Input is UTF-8. No proof is issued by this crate.

Freeze the version-1 model envelope:

```text
{
  "version": 1,
  "services": [ServiceSpec, ...],
  "presets": [
    {"name": string,
     "endpoint": {"service_id": u16, "code": u32},
     "parameters": {field_name: grammar_input_string, ...}}
  ]
}
```

version and services are required; presets defaults to empty. All other object keys are rejected. Canonical structs use exactly their ABI field names. ServiceSpec requires id/name/endpoints. EndpointSchemaSpec requires code/name/service/roles/params/response/stream_response/description, with stream_response either null or an EndpointType. EndpointField requires name/ty/description. EnumVariantSpec requires name/value/description. Arrays preserve source order; parameters use a BTreeMap. Version is an integer token, not a quoted string.

EndpointType unit variants use their exact ABI PascalCase JSON strings (`"UInt32"`, `"String"`, `"UUID"`, `"TimeStampMs"`, and the other unit variants). Newtype variants use an exactly-one-key object, e.g. `{"StructRef":"Name"}`, `{"Vec":"String"}`, or `{"Optional":"UInt32"}`. Named-field variants use an exactly-one-key object whose value contains precisely the ABI fields, e.g. `{"NanoId":{"len":21}}`, `{"Struct":{"name":"Account","fields":[]}}`, `{"StructTable":{"struct_ref":"Account"}}`, `{"Enum":{"name":"Mode","variants":[]}}`, `{"EnumRef":{"name":"Mode","prefixed_name":false}}`. Empty objects, unknown tags, numeric enum tags, and mismatched shapes are TypeMismatch/InvalidToken. These encodings are explicit legacy-compatible shapes; no external serialization dependency is required.

validate_model checks unique nonempty service names/IDs, unique endpoint names/codes within a service, matching EndpointSchemaSpec.service, valid field/role/variant uniqueness, nonempty named reference targets, positive NanoId length, consistent named definitions, and resolved StructRef/EnumRef/StructTable targets across the whole model. Recursive named definitions are allowed without expansion; traversal is bounded by limits.max_depth. Scalar and finite container structures are fully validated. Reuse compatible endpoint utilities where possible, but do not let an endpoint-only converter skip full-model resolution.

Preset names must be unique/nonempty and reference a known EndpointKey. Every preset parameter name must refer to a declared input field. Missing required endpoint fields may remain unset in a preset for interactive editing, but every provided value must pass params::validate_input. select_endpoint/select_preset require exact case-sensitive, unambiguous matches: missing names use MissingRequiredField; duplicates use DuplicateObjectKey. validate_model must not call prepare_params for incomplete presets or emit I/O/UI output.

## Wave 2: params.rs

```rust
pub fn grammar_for(ty: &EndpointType) -> InputGrammar;
pub fn validate_input(model: &ValidatorModel, field: &EndpointField, input: &str, limits: Limits) -> ValidatorResult<()>;
pub fn prepare_params(model: &ValidatorModel, endpoint: EndpointKey, inputs: &[ParameterInput], limits: Limits) -> ValidatorResult<PreparedParams>;
pub fn validate_prepared(model: &ValidatorModel, params: &PreparedParams, limits: Limits) -> ValidatorResult<()>;
```

grammar_for returns Text for String, Bytea, UUID, NanoId, IpAddr, BlockchainDecimal, BlockchainAddress, and BlockchainTransactionHash; all other types use Json. Optional always uses Json, including Optional(String), to distinguish null from a literal string. Text grammar is raw user text, without quote stripping, JSON heuristics, or whitespace trimming; encode it once with the native JSON string encoder. Thus text `null` for a String field means the string `"null"`. Empty text is a valid String unless constrained by the type's format. Json grammar requires one complete JSON value and rejects trailing content; numeric/boolean inputs are not autoquoted.

Validate UInt32/Int32/Int64 with exact integer grammar and checked range, Float64 as finite JSON number, Boolean as exactly true/false, Unit as null, Object as an object with arbitrary recursively valid JSON, Vec as homogeneous arrays, Optional as null or its inner type, and Struct/StructRef as exact field objects. Optional fields may be omitted; all other declared params/struct fields are required. Unknown fields and duplicate keys are rejected. StructTable is an array of the referenced struct. Enum/EnumRef uses the exact declared variant name as a JSON string; prefixed enum refs use `<EnumName>::<VariantName>` when prefixed_name is true. TimestampMs uses a nonnegative integer fitting i64. BlockchainDecimal uses an exact non-exponent decimal text form with checked i128 coefficient and scale <= 38; it is never converted to f64. Reference lookup uses validate_model's named-definition rules and recursion limits.

Bytea is canonical padded RFC 4648 base64 with no whitespace; UUID is hexadecimal 8-4-4-4-12 case-insensitive form; NanoId is exactly len ASCII characters from A-Z/a-z/0-9/_/-. IpAddr uses standard IPv4/IPv6 address grammar without ports or zones. BlockchainAddress is 0x plus 40 hexadecimal digits; BlockchainTransactionHash is 0x plus 64. Format validation here is structural and does not authenticate chain ownership/checksum. Every text length, numeric digit count, decoded output size, object field count, array item count, and nesting level is bounded by Limits.

prepare_params resolves a known EndpointKey, rejects duplicate/unknown names, validates every supplied input, requires all nonoptional fields, emits one complete parameter object with native encode support, in declared endpoint-field order, and returns owned payload bytes. Omitted Optional fields remain omitted. No partially built bytes escape on failure. validate_prepared reparses the entire public payload against the selected endpoint and the same strict rules, without interpreting already encoded strings as raw UI text. No schema proof is inferred from PreparedParams. Unknown endpoint/name is MissingRequiredField or UnknownObjectKey; wrong token shape TypeMismatch; invalid grammar InvalidSyntax/InvalidNumber; range errors NumberOverflow; format errors FormatMismatch; limits retain the relevant canonical error code. All new semantic errors use Decode source, with field IDs/byte offsets where available.

## Wave 3: session.rs

```rust
pub fn new_session(config: SessionConfig) -> ValidatorResult<Session>;
pub fn connect(session: &mut Session, reactor: &mut dyn SessionReactor) -> ValidatorResult<()>;
pub fn submit(session: &mut Session, model: &ValidatorModel, params: PreparedParams, reactor: &mut dyn SessionReactor, limits: Limits) -> ValidatorResult<u64>;
pub fn poll(session: &mut Session, reactor: &mut dyn SessionReactor) -> ValidatorResult<Option<SessionEvent>>;
pub fn close(session: &mut Session, reactor: &mut dyn SessionReactor) -> ValidatorResult<()>;
```

new_session rejects max_pending zero, cpu_units zero, retained_bytes zero, or timeout_ms zero with BudgetExhausted. It starts Disconnected, next_sequence 1, and an empty pending map. connect requires Disconnected, calls reactor.connect once, then changes to Connecting; reactor failure changes to Failed and propagates its error. A Connected event is accepted only in Connecting and changes to Ready. No method waits for progress. The host must poll/drive its reactor outside these calls; a poll with no event returns None and changes nothing.

submit requires Ready, validates the selected endpoint and full PreparedParams using params::validate_prepared, checks pending count < max_pending and payload bytes <= budget.retained_bytes, checks next_sequence increment for overflow, and calls reactor.send once. Reactor send is atomic: on error it must not have accepted the request. Only after success insert PendingRequest with matching key/sequence/endpoint and streaming set from endpoint.stream_response.is_some(), then advance next_sequence. Sequence IDs are never reused during the session. Capacity/budget rejection is BudgetExhausted, overflow NumberOverflow, invalid phase ValidatorError::State. Validation, capacity, sequence, and send failures leave pending/sequence unchanged; the caller receives the reactor error. The host accounts for wire encoding/response retention under the same request budget.

poll processes at most one event and returns it only after state validation. Immediate requires a matching pending request in Ready or Closing; remove a nonstreaming request, but retain a streaming request for later stream events. Stream requires a matching streaming request and never completes it. RemoteError removes the matching request. StreamClosed removes a matching streaming request. Payload-bearing events must contain complete strictly validated JSON and match their endpoint's response/stream_response semantics; the session validates against the stored model-independent endpoint key by requiring the reactor to return endpoint-validated payloads, since poll does not borrow a model. SessionReactor implementations must enforce those response schema and transport boundaries via the allowed wss/wire/native validation pipeline before constructing SessionEvent. Unrecognized sequences and illegal event kinds are FramingError and transition the session to Failed, clearing pending; do not display/reroute their payloads.

Closed may arrive in Connecting, Ready, or Closing, sets Closed and clears pending. Duplicate Connected/Closed events and events in Disconnected/Closed/Failed are state errors without silently reopening the session. poll errors from the reactor transition to Failed and clear pending. close in Connecting or Ready calls reactor.close, then changes to Closing; close in Closing or Closed is idempotent and does not call the reactor again. close in Disconnected immediately sets Closed. Failed close calls reactor.close once to permit cleanup and becomes Closed on success. A close failure transitions to Failed and clears pending. Cancelling pending calls on a connection shutdown is represented by clearing the pending state; no synthetic successful result is emitted.

No UI or session method logs secrets or payloads implicitly. The reactor is injected by the application and does not become an owned field in Session. The bridge carries only owned input/output across CPU submission; connection handles and network progress remain on the host reactor. Timeouts/cancellation/authentication arrive as structured reactor errors using the canonical ABI code, never as an implicit background task.

## Wave 4: ui.rs

```rust
pub fn rows(model: &ValidatorModel) -> Vec<EndpointRow>;
pub fn render_list(model: &ValidatorModel, output: &mut dyn Write) -> ValidatorResult<()>;
pub fn run(model: &ValidatorModel, options: &Options, input: &mut dyn BufRead, output: &mut dyn Write, reactor: Option<&mut dyn SessionReactor>) -> ValidatorResult<()>;
```

rows returns owned descriptions/names sorted by `(service name UTF-8 bytes, endpoint name UTF-8 bytes, code)` and exact EndpointKeys. render_list writes header `service<TAB>endpoint<TAB>code<TAB>description<LF>` then one row per endpoint, escaping backslash as `\`, tabs as `\t`, CR as `\r`, and LF as `\n` in all text cells. No ANSI styling or terminal probing occurs in list mode. Description text is data, never a terminal control instruction; escape other controls as `\u{hex}`. Numbers are base-10. Output failures retain io::Error.

run validates the model and resolves any exact service/endpoint or preset selection before displaying it. Preset fields are defaults; explicit --param entries override them by exact name. Duplicate explicit names are already rejected by parse_args and must also be rejected for caller-built Options. List mode lists services/endpoints; if selected, validate any complete provided parameters and display missing required names without sending. It never invokes a reactor or consumes interactive input. No selected endpoint in Native/Tui starts at endpoint selection.

Native mode is a plain line-based adapter with stable commands: `list`, `select SERVICE ENDPOINT`, `set NAME=VALUE`, `validate`, `send`, `poll`, `close`, `quit`. Input is bounded by strict max_string_bytes/max_payload_bytes, split commands without damaging parameter text, and require exact names. `set` preserves text after the first '='. `validate` prepares params and reports validation errors without exiting the editing session. `send` uses session::submit only after preparation, `poll` performs one reactor poll, and `close` closes explicitly. EOF acts as quit. Invalid commands produce a readable bounded error while preserving prior selections/inputs. Error output is data-escaped so user/model strings cannot inject terminal control sequences.

Tui is an alternative terminal presentation of the same state/actions and grammar. With only existing dependencies, implement it through the same line-oriented input abstraction and optional ANSI view rendering, not an unlisted terminal/UI crate. It must support selection, editing, validation, sending, polling, closing, and graceful EOF. It must not leave terminal state modified after errors. Native and Tui work offline for all model/parameter actions. If send/poll requires a reactor and None was passed, report a State error with `no reactor bound` and preserve editing state; never report a fake request success. With Some, connect once on the first network action, make progress only through explicit poll, and allow send only when Ready. Display bounded responses and all session errors without panicking or blocking for network completion. Before quit, close any started live session once and propagate cleanup errors. Never create an executor or choose credentials from ambient state.

Module dependency direction is load -> params::validate_input for preset field checks; params reads immutable validated model data without calling load::validate_model recursively; session -> params for submission validation; ui -> load/params/session. The leaf lookup helpers used by params must not call full-model validation and recurse back through preset checking. No shared types are moved out of the root, and no module performs unsafe operations.

## Frozen root snapshot

```rust
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
```

## Frozen binary entrypoint

```rust
//! Process adapter. Hosts supply live reactor bindings through the library API.
use std::{io, process::ExitCode};

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let code = jmax_validator::run(
        std::env::args_os().skip(1),
        &mut stdin.lock(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    );
    ExitCode::from(code)
}
```
