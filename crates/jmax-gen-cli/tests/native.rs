//! `--native`: write mode, check mode, manifest and drift.

use jmax_gen_cli::{args::parse_args, check, manifest, pipeline, Command, DriftKind, OutputFormat, RunMode};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

const BINANCE: &str = include_str!("../../../fixtures/native/binance.ron");
const BINANCE_SPEC: &str = include_str!("../../../fixtures/native/binance.native.ron");
const RPC: &str = include_str!("../../../fixtures/native/rpc.ron");
const RPC_SPEC: &str = include_str!("../../../fixtures/native/rpc.native.ron");
const VERSION: &str = "format_version = 1\nmodel_version = \"1.0.0\"\nminimum_runtime = \"0.1.0\"\n";

/// A flat model: the Rust format's schema tables apply to it.
const FLAT: &str = r#"Config(
    version: "1.0.0",
    definitions: [
        Definition(name: "Quote", ty: Struct(
            name: "Quote",
            fields: [
                Field(name: "symbol", ty: String),
                Field(name: "bid", ty: Int64),
                Field(name: "ask", ty: Int64),
            ],
        )),
    ],
)
"#;
const FLAT_SPEC: &str = "NativeSpec(roots: [\"Quote\"], reject_unknown: true)\n";

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str, model: &str, spec: &str) -> Self {
        let root = std::env::temp_dir().canonicalize().expect("canonical temporary directory").join(format!("jmax-gen-native-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("in")).unwrap();
        fs::write(root.join("in/services.ron"), model).unwrap();
        fs::write(root.join("in/version.toml"), VERSION).unwrap();
        fs::write(root.join("native.ron"), spec).unwrap();
        Self(root)
    }
    fn input(&self) -> PathBuf {
        self.0.join("in/services.ron")
    }
    fn native(&self) -> PathBuf {
        self.0.join("native.ron")
    }
    fn output(&self) -> PathBuf {
        self.0.join("out")
    }
    fn run(&self, extra: &[&str]) -> (u8, String, String) {
        let mut arguments: Vec<OsString> = vec![
            "--input".into(),
            self.input().into(),
            "--output".into(),
            self.output().into(),
        ];
        arguments.extend(extra.iter().map(OsString::from));
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = jmax_gen_cli::run(arguments, &mut stdout, &mut stderr);
        (code, String::from_utf8(stdout).unwrap(), String::from_utf8(stderr).unwrap())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn native_args(workspace: &Workspace) -> Vec<String> {
    vec!["--native".into(), workspace.native().to_string_lossy().into_owned()]
}

fn run_native(workspace: &Workspace, more: &[&str]) -> (u8, String, String) {
    let native = native_args(workspace);
    let mut extra: Vec<&str> = native.iter().map(String::as_str).collect();
    extra.extend_from_slice(more);
    workspace.run(&extra)
}

#[test]
fn write_mode_adds_the_native_artifact_and_lists_it() {
    let workspace = Workspace::new("write", BINANCE, BINANCE_SPEC);
    let (code, stdout, stderr) = run_native(&workspace, &["--format", "rust"]);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let files: Vec<_> = ["models", "endpoints", "registries", "schema_tables", "native"]
        .iter()
        .map(|name| workspace.output().join(format!("rust/{name}.rs")))
        .collect();
    assert!(files.iter().all(|path| path.is_file()), "{files:?}");
    let rows: Vec<&str> = stdout.lines().collect();
    assert_eq!(rows[0], "jmax-artifacts\t1");
    let paths: Vec<&str> = rows[1..].iter().map(|row| row.split('\t').next().unwrap()).collect();
    assert_eq!(paths, ["rust/endpoints.rs", "rust/models.rs", "rust/native.rs", "rust/registries.rs", "rust/schema_tables.rs"]);
    let native_row = rows.iter().find(|row| row.starts_with("rust/native.rs\t")).unwrap();
    let fields: Vec<&str> = native_row.split('\t').collect();
    let bytes = fs::read(&files[4]).unwrap();
    assert_eq!(fields[1], "rust");
    assert_eq!(fields[2], bytes.len().to_string());
    assert_eq!(fields[3], format!("{:016x}", manifest::content_hash(&bytes)));

    // The bytes are exactly the library's generate_with_native output, with the
    // flat schema tables off.
    let model = jmax_endpoint_ron::load_model(BINANCE, Some(VERSION), &Default::default()).unwrap();
    let spec = jmax_endpoint_ron::load_native_spec(BINANCE_SPEC, &Default::default()).unwrap();
    let options = jmax_codegen_rust::CodegenOptions {
        module_name: "rust".into(),
        emit_schema_tables: false,
        ..Default::default()
    };
    let generation =
        jmax_codegen_rust::generate_with_native(&model, &spec, &options, &Default::default()).unwrap();
    for artifact in generation.artifacts() {
        if artifact.path.ends_with("/mod.rs") || artifact.path.ends_with("/compat.rs") {
            continue;
        }
        let written = fs::read_to_string(workspace.output().join(&artifact.path)).unwrap();
        assert_eq!(written, artifact.contents, "{}", artifact.path);
    }
}

#[test]
fn check_mode_reports_missing_and_different_native_output() {
    let workspace = Workspace::new("check", BINANCE, BINANCE_SPEC);
    // Nothing written yet: every Rust artifact is missing.
    let (code, stdout, _) = run_native(&workspace, &["--format", "rust", "--check"]);
    assert_eq!(code, 3);
    assert!(stdout.lines().any(|row| row == "Missing\trust/native.rs"), "{stdout}");
    assert!(!workspace.output().exists(), "check mode creates nothing");

    assert_eq!(run_native(&workspace, &["--format", "rust"]).0, 0);
    let (code, stdout, stderr) = run_native(&workspace, &["--format", "rust", "--check"]);
    assert_eq!((code, stdout.as_str(), stderr.as_str()), (0, "", ""));

    let native = workspace.output().join("rust/native.rs");
    let mut bytes = fs::read(&native).unwrap();
    bytes.extend_from_slice(b"// drift\n");
    fs::write(&native, &bytes).unwrap();
    let (code, stdout, _) = run_native(&workspace, &["--format", "rust", "--check"]);
    assert_eq!((code, stdout.as_str()), (3, "Different\trust/native.rs\n"));

    fs::remove_file(&native).unwrap();
    let (code, stdout, _) = run_native(&workspace, &["--format", "rust", "--check"]);
    assert_eq!((code, stdout.as_str()), (3, "Missing\trust/native.rs\n"));

    // Rewriting repairs it, and a second write changes nothing on disk.
    assert_eq!(run_native(&workspace, &["--format", "rust"]).0, 0);
    let before = fs::metadata(workspace.output().join("rust/models.rs")).unwrap().modified().unwrap();
    assert_eq!(run_native(&workspace, &["--format", "rust"]).0, 0);
    assert_eq!(fs::metadata(workspace.output().join("rust/models.rs")).unwrap().modified().unwrap(), before);
    assert_eq!(run_native(&workspace, &["--format", "rust", "--check"]).0, 0);
}

#[test]
fn the_library_plan_is_sorted_and_deterministic() {
    let workspace = Workspace::new("plan", BINANCE, BINANCE_SPEC);
    let options = match parse_args(
        ["--input", workspace.input().to_str().unwrap(), "--output", workspace.output().to_str().unwrap(),
            "--native", workspace.native().to_str().unwrap()]
            .map(OsString::from),
    )
    .unwrap()
    {
        Command::Generate(options) => options,
        other => panic!("{other:?}"),
    };
    assert_eq!(options.native.as_deref(), Some(workspace.native().as_path()));
    assert_eq!(options.mode, RunMode::Write);
    let first = pipeline::generate(&options).unwrap();
    let second = pipeline::generate(&options).unwrap();
    assert_eq!(first, second);
    let paths: Vec<_> = first.artifacts.iter().map(|a| a.path.to_str().unwrap()).collect();
    assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
    let native = first.artifacts.iter().find(|a| a.path == Path::new("rust/native.rs")).unwrap();
    assert_eq!(native.format, OutputFormat::Rust);
    // Every other format is still present and the plan is a pure value: nothing was written.
    assert!(first.artifacts.iter().any(|a| a.path == Path::new("openapi.json")));
    assert!(!workspace.output().exists());
    // Compare against an empty directory through the library API.
    let report = check::compare(&workspace.output(), &first).unwrap();
    assert_eq!(report.compared, first.artifacts.len());
    assert!(report.drift.iter().all(|drift| drift.kind == DriftKind::Missing));
}

#[test]
fn without_native_nothing_changes() {
    let workspace = Workspace::new("plain", FLAT, FLAT_SPEC);
    // Plain Rust generation of a struct model (with its flat schema tables) works.
    let (code, plain_manifest, stderr) = workspace.run(&[]);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let plain_rust: Vec<&str> = plain_manifest.lines().filter(|row| row.starts_with("rust/")).collect();
    assert_eq!(plain_rust.len(), 4, "{plain_manifest}");
    assert!(!workspace.output().join("rust/native.rs").exists());
    let tables = std::fs::read_to_string(workspace.output().join("rust/schema_tables.rs")).unwrap();
    assert!(tables.contains("SCHEMA_COUNT: usize = 1"), "{tables}");

    // Adding a native specification adds rust/native.rs and leaves every non-Rust artifact identical.
    let (code, native_manifest, stderr) = run_native(&workspace, &[]);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let non_rust = |text: &str| -> Vec<String> {
        text.lines().filter(|row| !row.starts_with("rust/")).map(str::to_owned).collect()
    };
    assert_eq!(non_rust(&plain_manifest), non_rust(&native_manifest));
    assert!(native_manifest.lines().any(|row| row.starts_with("rust/native.rs\trust\t")));
    assert_eq!(native_manifest.lines().filter(|row| row.starts_with("rust/")).count(), 5);
}

#[test]
fn argument_rules() {
    let parse = |extra: &[&str]| {
        let mut arguments: Vec<OsString> = vec!["-i".into(), "in".into(), "-o".into(), "out".into()];
        arguments.extend(extra.iter().map(OsString::from));
        parse_args(arguments)
    };
    let usage = |result: jmax_gen_cli::CliResult<Command>| matches!(result, Err(jmax_gen_cli::CliError::Usage(_)));
    assert!(matches!(parse(&["--native", "n.ron"]), Ok(Command::Generate(o)) if o.native.as_deref() == Some(Path::new("n.ron"))));
    assert!(matches!(parse(&["--native=n.ron", "--check"]), Ok(Command::Generate(o)) if o.mode == RunMode::Check));
    assert!(matches!(parse(&["-f", "rust", "--native", "n.ron"]), Ok(Command::Generate(_))));
    assert!(matches!(parse(&[]), Ok(Command::Generate(o)) if o.native.is_none()));
    assert!(usage(parse(&["--native", "a", "--native", "b"])));
    assert!(usage(parse(&["--native"])));
    assert!(usage(parse(&["--native="])));
    assert!(usage(parse(&["--native", "--check"])));
    assert!(usage(parse(&["--native", "n.ron", "--legacy"])));
    assert!(usage(parse(&["--native", "n.ron", "--rust"])));
    assert!(usage(parse(&["--native", "n.ron", "-f", "markdown"])));
    assert!(usage(parse(&["--native", "n.ron", "-f", "services", "-f", "openapi"])));
}

#[test]
fn unsupported_and_missing_specifications_fail_without_writing() {
    // Untyped objects still have no native form.
    let object_model = FLAT.replace("Field(name: \"ask\", ty: Int64),", "Field(name: \"ask\", ty: Int64), Field(name: \"extra\", ty: Object),");
    let workspace = Workspace::new("unsupported", &object_model, FLAT_SPEC);
    let (code, stdout, stderr) = run_native(&workspace, &["--format", "rust"]);
    assert_eq!((code, stdout.as_str()), (1, ""));
    assert!(stderr.contains("Object has no native form"), "{stderr}");
    assert!(!workspace.output().exists());

    // A format field generates (UUID is validated by jmax-native).
    let workspace = Workspace::new("formats", RPC, RPC_SPEC);
    let (code, _, stderr) = run_native(&workspace, &["--format", "rust"]);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let native = fs::read_to_string(workspace.output().join("rust/native.rs")).unwrap();
    assert!(native.contains("FormatId::Uuid"));

    let workspace = Workspace::new("missing", BINANCE, BINANCE_SPEC);
    fs::remove_file(workspace.native()).unwrap();
    let (code, _, stderr) = run_native(&workspace, &[]);
    assert_eq!(code, 1);
    assert!(stderr.contains("read native specification"), "{stderr}");
    assert!(!workspace.output().exists());

    let workspace = Workspace::new("garbage", BINANCE, "NativeSpec(roots: [\"Nope\"])");
    let (code, _, stderr) = run_native(&workspace, &[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(!workspace.output().exists());
}
