# jmax-gen-cli: frozen Wave 0 contract

Status: W0-ROOTS-7. This is a std host tool. The existing std/alloc manifest features are ABI feature forwarding, not a promise that the CLI itself supports no_std. All children import root types with `super::*`. The binary contains only argument/stream acquisition and ExitCode conversion; library run owns orchestration. Canonical ServiceSpec and AbiError are re-exported without replacement. No asynchronous runtime, process-spawned generator, implicit network operation, or global mutable configuration is introduced.

## Shared surface and driver

The exact root and binary snapshots below contain all definitions, derives, variants, error fields, generic bounds, methods, constants, help text, and orchestration. The root's run accepts arguments excluding argv[0] and returns exit codes 0 success, 1 operational/validation failure, 2 usage error, and 3 generated drift. Errors go to stderr, help/version to stdout, write-mode manifest to stdout, and check-mode drift rows to stdout. An output-stream failure is an operational failure even if artifact writes already succeeded. Library callers can inspect CliError directly by using the wave APIs.

CliError::Io retains the operation, full native PathBuf, and original io::Error. AbiError retains its original source/code/offset/schema/field information. InvalidArtifact is a structural or path conflict error, never a reason to write a partial plan. Public Artifact fields are intentionally inspectable; every consumer must revalidate a plan because callers can construct or mutate it without Artifact::new.

The module dependency graph is args -> root types, manifest -> root types, check -> manifest/root types, pipeline -> manifest/root types plus allowed generator crates. No child calls root run. Root run generates one immutable plan, builds/validates the manifest, then either compares or writes. There is no write call on the check branch.

## Wave 1: args.rs

```rust
pub fn parse_args<I>(arguments: I) -> CliResult<Command>
where I: IntoIterator<Item = OsString>;
pub fn parse_format(value: &str) -> CliResult<Vec<OutputFormat>>;
```

Native options exactly match HELP: --input/-i PATH, --output/-o DIR, repeatable --format/-f NAME, --native PATH, --check, --legacy, --help/-h, --version/-V. Also accept --input=PATH, --output=DIR, and --format=NAME. Paths retain OsString encoding; option names and format names require UTF-8. With no format option, select all six formats. Values are rust, services, json-schema, openapi, asyncapi, markdown, and all; all expands to all six. Repeated format selections are deduplicated and sorted by OutputFormat's declaration order, independent of command-line order. Formats must not be empty. parse_format returns the same canonical order, with a single format or all, and rejects comma lists or unknown names with Usage.

Freeze version-1 legacy aliases: --input-dir is --input, --output-dir is --output, --rust is --format rust, --docs is --format markdown, --spec is --format services. Any legacy alias sets Options.legacy=true and may be combined with native aliases subject to the same singleton duplicate rules. A legacy directory input resolves its entrypoint as `services.ron`; an explicit file remains a file. No other guessed alias is accepted. --legacy enables existing legacy generator conventions and artifact content, not a schema validation bypass.

An input and output path are required for Generate, including --check. Reject empty paths, duplicate singleton input/output/mode flags, missing values, unknown flags, unexpected positional arguments, and conflicting scalar settings with CliError::Usage. Repeated --format is the only repeatable native setting. --help/--version take no value, require no input/output, and return their command immediately when encountered. Non-UTF-8 file paths are valid. A flag-like path can be supplied via equals form; separate option values that are recognized flags are missing-value errors. Do not open files or create directories while parsing. Validate empty/deduplicated format vectors again in pipeline for library-constructed Options.

## Wave 2: pipeline.rs

```rust
pub fn generate(options: &Options) -> CliResult<GenerationPlan>;
pub fn write_plan(output: &Path, plan: &GenerationPlan) -> CliResult<()>;
```

generate resolves the input file or directory, reads at most MAX_INPUT_BYTES with a bounded read that detects growth, loads and validates the endpoint RON model through jmax-endpoint-ron, and invokes the allowed Rust/spec/AOT generators. Version files and legacy conventions follow the endpoint loader's version contract; missing required input/version data is an error. Do not infer a valid empty model after a loader failure. Input errors preserve path and operation; model/generator errors preserve ABI information. No filesystem mutation occurs in generate, irrespective of RunMode.

Freeze these output relative paths by selection:

| Format | Required artifact paths |
| --- | --- |
| Rust | `rust/models.rs`, `rust/endpoints.rs`, `rust/registries.rs`, `rust/schema_tables.rs` |
| Services | `services.json`, `services-all.json` |
| JsonSchema | `json-schema.json`, `mcp-tools.json` |
| OpenApi | `openapi.json` |
| AsyncApi | `asyncapi.json` |
| Markdown | `docs/services.md`, `docs/errors.md`, `systemd/services.conf` |

The public/all service distinction must use model role visibility, not arbitrary filtering. AOT tables correspond to the same validated model/fingerprints as Rust registries. Every emitted format comes from the same normalized input version. All requested artifacts, including syntactically valid empty catalogs, must be generated in memory before returning; a format is never represented by an empty placeholder or omitted because a generator is inconvenient. Legacy changes content through supported generator options, with these stable destination paths. If a source feature cannot be represented in a selected projection, return UnsupportedSchemaFeature rather than silently dropping it.

No wall clock, random value, absolute source path, filesystem enumeration order, or nondeterministic map iteration may enter artifact bytes. Preserve semantic model order where required by the endpoint contract; otherwise sort output keys/names deterministically. Generated text uses UTF-8, LF line endings, and exactly one trailing LF. Binary future artifacts require a new format/contract revision. plan.artifacts is sorted by normalized forward-slash path; content is complete and owned.

write_plan calls manifest::build_manifest before touching the destination, revalidates every relative path, and detects duplicate/case-colliding normalized destinations. Artifact paths consist of nonempty normal components, valid UTF-8, without control bytes, Windows punctuation/reserved device names, or trailing dots/spaces. Reject absolute paths, prefixes, parent traversal, and a current-directory-only path. Artifact::new performs these lexical checks; the writer additionally checks actual filesystem components with symlink_metadata and rejects symlink ancestors, symlink destinations, and nonregular destinations. output is created only in write mode. An existing output directory must be a directory, not a symlink. Output paths are host-selected; artifact paths cannot escape them.

Create missing directories only after the entire plan is structurally valid. For each changed artifact, stage complete bytes in a collision-safe create_new sibling temporary file, flush it, then atomically rename on platforms supporting replacement; retain the old file on pre-rename failure. Unchanged files are not rewritten. Clean up owned temporary files on errors, and never truncate a live destination before bytes are staged. The overall multi-file operation is not transactional: earlier successful replacements remain if a later filesystem operation fails, and CliError identifies that failure. Do not delete files absent from the plan or create a hidden cache/manifest. The deterministic manifest is printed by the root, not stored as an extra artifact.

## Wave 3: check.rs

```rust
pub fn compare(output: &Path, plan: &GenerationPlan) -> CliResult<CheckReport>;
```

compare first validates the complete plan with manifest::build_manifest and applies the same path and existing-filesystem component checks as write_plan. It is strictly read-only: no directory creation, temporary file, lockfile, cache, metadata update request, or call to write_plan. For each artifact, compare full raw bytes, not hashes alone. Missing output directory/file or a missing normal ancestor yields DriftKind::Missing for that artifact. A regular file with different length or bytes yields Different. Permission errors, symlinks, nonregular paths, and other I/O failures are operational errors, not drift. Read comparison must not allocate according to untrusted destination-file length: use a bounded streaming comparison or a bounded read of expected length + 1.

compared equals the number of planned artifacts on success; drift rows contain relative canonical paths sorted exactly as the manifest. Unrelated extra files are ignored because this tool does not claim ownership of them. An empty externally supplied plan is structurally valid and compares cleanly with compared 0; generate itself never returns an empty plan for nonempty formats. compare must not mutate the plan. Root output uses `Missing<TAB>path` or `Different<TAB>path`, one LF-terminated row each; clean comparison is silent and exits 0, drift exits 3.

## Wave 4: manifest.rs

```rust
pub fn build_manifest(plan: &GenerationPlan) -> CliResult<ArtifactManifest>;
pub fn render_manifest(manifest: &ArtifactManifest) -> CliResult<Vec<u8>>;
pub fn content_hash(bytes: &[u8]) -> u64;
```

build_manifest validates paths regardless of how artifacts were constructed, normalizes native normal path components to '/' separators, rejects duplicate normalized paths and ASCII-case-insensitive collisions across formats, converts byte lengths with checked u64 conversion, and sorts entries by normalized UTF-8 path bytes. The PathBuf stored in each manifest entry represents that normalized relative path. version is MANIFEST_VERSION. It never mutates artifact bytes or includes the output directory. It does not read the filesystem.

content_hash is FNV-1a-64, offset 14695981039346656037, prime 1099511628211, XOR then wrapping multiply for each byte. It is a deterministic content identity, not a security check or a replacement for full drift comparison. An empty byte slice yields the offset value.

render_manifest revalidates version, paths, ordering/collisions, and does not trust a caller-built ArtifactManifest. It returns exact UTF-8 TSV: first line `jmax-artifacts<TAB>1<LF>`, then one row per entry `<path><TAB><format><TAB><byte_len decimal><TAB><content_hash 16 lowercase hex digits><LF>`. Format tokens are rust, services, json-schema, openapi, asyncapi, markdown. Entry paths cannot contain tab/CR/LF because root validation rejects controls. Rendering sorts a copy by canonical path and rejects duplicate/colliding destinations; it does not mutate caller entries. An empty manifest has only the header line.

## Frozen root snapshot

```rust
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
```

## Frozen binary entrypoint

```rust
//! Process adapter for the library's stream-injected command driver.
use std::{io, process::ExitCode};

fn main() -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let code = jmax_gen_cli::run(
        std::env::args_os().skip(1),
        &mut stdout.lock(),
        &mut stderr.lock(),
    );
    ExitCode::from(code)
}
```

## Native decoders (--native)

The option `--native PATH` (also `--native=PATH`) names a native.ron file.
It is valid only when the Rust format is selected, with no legacy alias, and may be given at
most once; otherwise parse_args returns the usual Cli argument error. generate loads it with
jmax-endpoint-ron load_native_spec under the same bounded read and size limit as the input
model, validates it against the model's registry, and calls codegen-rust generate_with_native,
adding `rust/native.rs` to the Rust artifact paths. The manifest lists it with the Rust format,
and check mode reports drift for it like any artifact. Without `--native` nothing changes:
the four Rust artifacts and every other format are byte-identical to before.

With `--native` the Rust artifacts are generated with `emit_schema_tables` off, because the flat schema tables cannot describe the nested native models; `rust/schema_tables.rs` is still emitted (without tables). Options.native is None for every other caller and changes nothing for them.
