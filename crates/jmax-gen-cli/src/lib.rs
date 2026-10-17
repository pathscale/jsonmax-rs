//! Deterministic generation plans, drift reporting, and the command-line driver.
#![forbid(unsafe_code)]

use std::{ffi::OsString, fmt, io::{self, Write}, path::{Component, Path, PathBuf}};
pub use jmax_abi::{endpoint::ServiceSpec, error::{Error as AbiError, ErrorCode, ErrorSource}};

pub mod args;
pub mod pipeline;
pub mod check;
pub mod manifest;

pub const MANIFEST_VERSION: u16 = 1;
pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_DRIFT: u8 = 3;
pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const HELP: &str = "jsonmax-gen --input PATH --output DIR [--format rust|services|json-schema|openapi|asyncapi|markdown|all] [--native PATH] [--check] [--legacy]\n\n--input, -i       Endpoint RON input\n--output, -o      Artifact directory\n--format, -f      Repeatable output selection (default: all)\n--native         Native decoder specification; adds rust/native.rs\n--check          Compare without creating or changing files\n--legacy         Enable version-1 legacy generation conventions\n--help, -h       Show help\n--version, -V    Show version\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutputFormat { Rust, Services, JsonSchema, OpenApi, AsyncApi, Markdown }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode { Write, Check }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
    pub formats: Vec<OutputFormat>,
    pub mode: RunMode,
    pub legacy: bool,
    /// Native decoder specification (native.ron); needs the Rust format.
    pub native: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command { Help, Version, Generate(Options) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    pub format: OutputFormat,
    pub bytes: Vec<u8>,
}

impl Artifact {
    pub fn new(path: PathBuf, format: OutputFormat, bytes: Vec<u8>) -> CliResult<Self> {
        validate_artifact_path(&path)?;
        Ok(Self { path, format, bytes })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerationPlan { pub artifacts: Vec<Artifact> }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: PathBuf,
    pub format: OutputFormat,
    pub byte_len: u64,
    pub content_hash: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactManifest { pub version: u16, pub entries: Vec<ManifestEntry> }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftKind { Missing, Different }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drift { pub path: PathBuf, pub kind: DriftKind }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport { pub compared: usize, pub drift: Vec<Drift> }

impl CheckReport { pub fn is_clean(&self) -> bool { self.drift.is_empty() } }

#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    Validation(AbiError),
    InvalidArtifact { path: PathBuf, detail: &'static str },
}

pub type CliResult<T> = std::result::Result<T, CliError>;

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => f.write_str(message),
            Self::Io { operation, path, source } => write!(f, "{operation} {}: {source}", path.display()),
            Self::Validation(error) => write!(f, "{error}"),
            Self::InvalidArtifact { path, detail } => write!(f, "invalid artifact {}: {detail}", path.display()),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Validation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<AbiError> for CliError { fn from(error: AbiError) -> Self { Self::Validation(error) } }

pub fn validate_artifact_path(path: &Path) -> CliResult<()> {
    if path.as_os_str().is_empty() || !path.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err(CliError::InvalidArtifact { path: path.to_path_buf(), detail: "expected a nonempty relative path of normal components" });
    }
    if path.to_str().is_none() {
        return Err(CliError::InvalidArtifact { path: path.to_path_buf(), detail: "manifest paths must be UTF-8" });
    }
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(CliError::InvalidArtifact { path: path.to_path_buf(), detail: "expected normal path component" });
        };
        let Some(name) = name.to_str() else {
            return Err(CliError::InvalidArtifact { path: path.to_path_buf(), detail: "manifest paths must be UTF-8" });
        };
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        if device || name.ends_with('.') || name.ends_with(' ')
            || name.chars().any(|ch| ch.is_control() || matches!(ch, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')) {
            return Err(CliError::InvalidArtifact { path: path.to_path_buf(), detail: "nonportable artifact path" });
        }
    }
    Ok(())
}

/// Execute with injected streams so diagnostics do not leak into generated bytes.
pub fn run<I>(arguments: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8
where I: IntoIterator<Item = OsString> {
    match execute(arguments, stdout) {
        Ok(code) => code,
        Err(error) => {
            let code = if matches!(&error, CliError::Usage(_)) { EXIT_USAGE } else { EXIT_FAILURE };
            let _ = writeln!(stderr, "jsonmax-gen: {error}");
            code
        }
    }
}

fn execute<I>(arguments: I, stdout: &mut dyn Write) -> CliResult<u8>
where I: IntoIterator<Item = OsString> {
    let print_error = |source| CliError::Io { operation: "write stdout", path: PathBuf::from("<stdout>"), source };
    match args::parse_args(arguments)? {
        Command::Help => { stdout.write_all(HELP.as_bytes()).map_err(print_error)?; Ok(EXIT_SUCCESS) }
        Command::Version => {
            writeln!(stdout, "jsonmax-gen {}", env!("CARGO_PKG_VERSION")).map_err(print_error)?;
            Ok(EXIT_SUCCESS)
        }
        Command::Generate(options) => {
            let plan = pipeline::generate(&options)?;
            let manifest = manifest::build_manifest(&plan)?;
            match options.mode {
                RunMode::Check => {
                    let report = check::compare(&options.output, &plan)?;
                    for drift in &report.drift {
                        writeln!(stdout, "{:?}\t{}", drift.kind, drift.path.display()).map_err(print_error)?;
                    }
                    Ok(if report.is_clean() { EXIT_SUCCESS } else { EXIT_DRIFT })
                }
                RunMode::Write => {
                    pipeline::write_plan(&options.output, &plan)?;
                    stdout.write_all(&manifest::render_manifest(&manifest)?).map_err(print_error)?;
                    Ok(EXIT_SUCCESS)
                }
            }
        }
    }
}
