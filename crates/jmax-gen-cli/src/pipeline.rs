//! Bounded input loading and deterministic in-memory artifact generation.

use super::{
    validate_artifact_path, Artifact, CliError, CliResult, GenerationPlan, Options,
    OutputFormat, MAX_INPUT_BYTES,
};
use jmax_endpoint_ron::{ConfigLimits, LoadOptions};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_VERSION_BYTES: u64 = 64 * 1024;
const MAX_DOCUMENT_BYTES: u32 = 64 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Read, validate, and project one endpoint configuration into a complete plan.
/// This function performs no filesystem mutations.
pub fn generate(options: &Options) -> CliResult<GenerationPlan> {
    validate_formats(&options.formats)?;
    let input = resolve_input(&options.input)?;
    let version_path = input.parent().unwrap_or_else(|| Path::new(".")).join("version.toml");
    let config_bytes = read_bounded(&input, MAX_INPUT_BYTES, "read input")?;
    let version_bytes = read_bounded(&version_path, MAX_VERSION_BYTES, "read version")?;
    let config_text = std::str::from_utf8(&config_bytes).map_err(|_| io_error(
        "decode input", &input, io::Error::new(io::ErrorKind::InvalidData, "configuration is not UTF-8"),
    ))?;
    let version_text = std::str::from_utf8(&version_bytes).map_err(|_| io_error(
        "decode version", &version_path, io::Error::new(io::ErrorKind::InvalidData, "version file is not UTF-8"),
    ))?;
    let limits = ConfigLimits {
        max_input_bytes: u32::try_from(MAX_INPUT_BYTES).unwrap_or(u32::MAX),
        ..ConfigLimits::default()
    };
    let load_options = LoadOptions { limits, require_version: true };
    let model = jmax_endpoint_ron::load_model(config_text, Some(version_text), &load_options)?;
    let registry = model_registry(&model);
    let mut plan = GenerationPlan::default();

    for format in &options.formats {
        match format {
            OutputFormat::Rust => append_rust(&mut plan, &model, options.legacy)?,
            OutputFormat::Services => append_services(&mut plan, &model)?,
            OutputFormat::JsonSchema => append_json_schema(&mut plan, &model, &registry)?,
            OutputFormat::OpenApi => append_openapi(&mut plan, &model, &registry)?,
            OutputFormat::AsyncApi => append_asyncapi(&mut plan, &model, &registry)?,
            OutputFormat::Markdown => append_markdown(&mut plan, &model, &registry)?,
        }
    }
    plan.artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(plan)
}

/// Validate the whole plan before creating directories, then stage and replace
/// each changed artifact independently.
pub fn write_plan(output: &Path, plan: &GenerationPlan) -> CliResult<()> {
    super::manifest::build_manifest(plan)?;
    validate_output_root(output)?;
    for artifact in &plan.artifacts {
        validate_artifact_path(&artifact.path)?;
    }
    let mut paths = plan.artifacts.iter().map(|artifact| normalized_path(&artifact.path))
        .collect::<CliResult<Vec<_>>>()?;
    paths.sort();
    for pair in paths.windows(2) {
        if pair[0].eq_ignore_ascii_case(&pair[1]) {
            return Err(CliError::InvalidArtifact {
                path: PathBuf::from(&pair[1]), detail: "duplicate or case-colliding destination",
            });
        }
    }

    if output.exists() {
        let metadata = fs::symlink_metadata(output).map_err(|error| io_error("inspect output", output, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CliError::InvalidArtifact { path: output.to_path_buf(), detail: "output must be a real directory" });
        }
    } else {
        fs::create_dir_all(output).map_err(|error| io_error("create output directory", output, error))?;
    }

    for artifact in &plan.artifacts {
        let destination = output.join(&artifact.path);
        ensure_safe_parent(output, &destination)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(CliError::InvalidArtifact { path: artifact.path.clone(), detail: "destination is not a regular file" });
                }
                if file_equals(&destination, &artifact.bytes)? { continue; }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect destination", &destination, error)),
        }
        let parent = destination.parent().ok_or_else(|| CliError::InvalidArtifact {
            path: artifact.path.clone(), detail: "destination has no parent",
        })?;
        create_missing_directories(output, parent)?;
        let (temporary, mut file) = create_temporary(parent, &destination)?;
        let staged = (|| -> io::Result<()> {
            file.write_all(&artifact.bytes)?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = staged {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(io_error("stage artifact", &temporary, error));
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return Err(io_error("replace artifact", &destination, error));
        }
    }
    Ok(())
}

fn append_rust(plan: &mut GenerationPlan, model: &jmax_endpoint_ron::EndpointModel, legacy: bool) -> CliResult<()> {
    let mut options = jmax_codegen_rust::CodegenOptions::default();
    options.module_name = String::from("rust");
    if legacy {
        options.compatibility.mode = jmax_codegen_rust::CompatibilityMode::LegacyV1;
        options.compatibility.emit_definition_variants = true;
    }
    let generation = jmax_codegen_rust::generate(model, &options)?;
    let mut found = [false; 4];
    for artifact in generation.into_artifacts() {
        let relative = artifact.path.strip_prefix("rust/").unwrap_or(&artifact.path);
        let format = match relative {
            "models.rs" => { found[0] = true; Some(("rust/models.rs", OutputFormat::Rust)) }
            "endpoints.rs" => { found[1] = true; Some(("rust/endpoints.rs", OutputFormat::Rust)) }
            "registries.rs" => { found[2] = true; Some(("rust/registries.rs", OutputFormat::Rust)) }
            "schema_tables.rs" => { found[3] = true; Some(("rust/schema_tables.rs", OutputFormat::Rust)) }
            _ => None,
        };
        if let Some((path, format)) = format {
            push_text(plan, path, format, artifact.contents)?;
        }
    }
    if found.iter().any(|present| !present) {
        return Err(generator_contract_error("Rust generator omitted a required artifact"));
    }
    Ok(())
}

fn append_services(plan: &mut GenerationPlan, model: &jmax_endpoint_ron::EndpointModel) -> CliResult<()> {
    let options = document_options(model);
    for (path, visibility) in [
        ("services.json", jmax_codegen_spec::Visibility::Public),
        ("services-all.json", jmax_codegen_spec::Visibility::All),
    ] {
        let document = jmax_codegen_spec::services::services_document(&model.services, visibility, &options)?;
        push_text(plan, path, OutputFormat::Services, document.text)?;
    }
    Ok(())
}

fn append_json_schema(
    plan: &mut GenerationPlan,
    model: &jmax_endpoint_ron::EndpointModel,
    registry: &jmax_codegen_spec::TypeRegistry,
) -> CliResult<()> {
    let options = document_options(model);
    let tools = jmax_codegen_spec::json_schema::mcp_tools(
        &model.services, registry, jmax_codegen_spec::Visibility::Public, &options,
    )?;
    let mut catalog = String::from("{\"tools\":[");
    for (index, tool) in tools.iter().enumerate() {
        if index != 0 { catalog.push(','); }
        catalog.push_str("{\"name\":"); json_string(&mut catalog, &tool.name);
        catalog.push_str(",\"description\":"); json_string(&mut catalog, &tool.description);
        catalog.push_str(&format!(",\"endpoint_code\":{},\"input_schema\":{},\"output_schema\":{} }}",
            tool.endpoint_code, tool.input_schema.text, tool.output_schema.text));
    }
    catalog.push_str("]}\n");
    push_text(plan, "mcp-tools.json", OutputFormat::JsonSchema, catalog)?;

    let mut definitions = String::from("{\"$schema\":\"");
    definitions.push_str(jmax_codegen_spec::JSON_SCHEMA_DIALECT);
    definitions.push_str("\",\"$defs\":{");
    for (index, definition) in model.definitions.iter().enumerate() {
        if index != 0 { definitions.push(','); }
        json_string(&mut definitions, &definition.name);
        definitions.push(':');
        let schema = jmax_codegen_spec::json_schema::type_schema(&definition.ty, registry, &options)?;
        definitions.push_str(&schema.text);
    }
    definitions.push_str("}}\n");
    push_text(plan, "json-schema.json", OutputFormat::JsonSchema, definitions)
}

fn append_openapi(plan: &mut GenerationPlan, model: &jmax_endpoint_ron::EndpointModel, registry: &jmax_codegen_spec::TypeRegistry) -> CliResult<()> {
    let document = jmax_codegen_spec::protocol::openapi(
        &model.services, registry, jmax_codegen_spec::Visibility::Public, &document_options(model),
    )?;
    push_text(plan, "openapi.json", OutputFormat::OpenApi, document.text)
}

fn append_asyncapi(plan: &mut GenerationPlan, model: &jmax_endpoint_ron::EndpointModel, registry: &jmax_codegen_spec::TypeRegistry) -> CliResult<()> {
    let document = jmax_codegen_spec::protocol::asyncapi(
        &model.services, registry, jmax_codegen_spec::Visibility::Public, &document_options(model),
    )?;
    push_text(plan, "asyncapi.json", OutputFormat::AsyncApi, document.text)
}

fn append_markdown(plan: &mut GenerationPlan, model: &jmax_endpoint_ron::EndpointModel, registry: &jmax_codegen_spec::TypeRegistry) -> CliResult<()> {
    let options = document_options(model);
    let service_docs = jmax_codegen_spec::markdown::service_docs(
        &model.services, registry, jmax_codegen_spec::Visibility::Public, &options,
    )?;
    push_text(plan, "docs/services.md", OutputFormat::Markdown, service_docs.text)?;
    let error_docs = model.errors.iter().map(|error| {
        let code = i32::try_from(error.code).map_err(|_| CliError::Validation(
            jmax_abi::error::Error::new(
                jmax_abi::error::ErrorCode::UnsupportedSchemaFeature,
                jmax_abi::error::ErrorSource::Encode, 0,
                "error code cannot be represented by the Markdown error projection",
            ),
        ))?;
        Ok(jmax_codegen_spec::ErrorDoc {
            code, name: error.name.clone(), description: error.message.clone(),
        })
    }).collect::<CliResult<Vec<_>>>()?;
    let errors = jmax_codegen_spec::markdown::error_docs(&error_docs, MAX_DOCUMENT_BYTES)?;
    push_text(plan, "docs/errors.md", OutputFormat::Markdown, errors.text)?;
    let systemd = jmax_codegen_spec::markdown::systemd_unit(&jmax_codegen_spec::SystemdOptions {
        description: if model.services.is_empty() { String::from("jsonmax endpoint service") }
            else { format!("jsonmax {} endpoint service", model.services[0].name) },
        executable: String::from("/usr/bin/jsonmax-gen"), arguments: Vec::new(), user: None,
        working_directory: None, restart_seconds: 5,
    }, MAX_DOCUMENT_BYTES)?;
    push_text(plan, "systemd/services.conf", OutputFormat::Markdown, systemd.text)
}

fn model_registry(model: &jmax_endpoint_ron::EndpointModel) -> jmax_codegen_spec::TypeRegistry {
    let mut registry = jmax_codegen_spec::TypeRegistry::default();
    for definition in &model.definitions {
        match &definition.ty {
            jmax_abi::endpoint::EndpointType::Struct { .. } => registry.structs.push(definition.ty.clone()),
            jmax_abi::endpoint::EndpointType::Enum { .. } => registry.enums.push(definition.ty.clone()),
            _ => {}
        }
    }
    registry
}

fn document_options(model: &jmax_endpoint_ron::EndpointModel) -> jmax_codegen_spec::DocumentOptions {
    jmax_codegen_spec::DocumentOptions {
        title: String::from("Endpoint API"), version: model.version.to_string(),
        description: String::new(), server_url: None, max_output_bytes: MAX_DOCUMENT_BYTES,
        max_type_depth: 128,
    }
}

fn validate_formats(formats: &[OutputFormat]) -> CliResult<()> {
    if formats.is_empty() { return Err(CliError::Usage(String::from("at least one output format is required"))); }
    for (index, format) in formats.iter().enumerate() {
        if formats[..index].contains(format) {
            return Err(CliError::Usage(String::from("output formats must be deduplicated")));
        }
    }
    Ok(())
}

fn resolve_input(input: &Path) -> CliResult<PathBuf> {
    let metadata = fs::metadata(input).map_err(|error| io_error("inspect input", input, error))?;
    if metadata.is_dir() { Ok(input.join("services.ron")) }
    else if metadata.is_file() { Ok(input.to_path_buf()) }
    else { Err(CliError::InvalidArtifact { path: input.to_path_buf(), detail: "input must be a regular file or directory" }) }
}

fn read_bounded(path: &Path, limit: u64, operation: &'static str) -> CliResult<Vec<u8>> {
    let file = File::open(path).map_err(|error| io_error(operation, path, error))?;
    let read_limit = limit.checked_add(1).ok_or_else(|| CliError::Usage(String::from("input size limit overflow")))?;
    let mut file = file.take(read_limit);
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(operation, path, error)),
        };
        total = total.checked_add(count as u64).ok_or_else(|| io_error(
            operation, path, io::Error::new(io::ErrorKind::InvalidData, "input size overflow"),
        ))?;
        if total > limit || total > read_limit {
            return Err(io_error(operation, path, io::Error::new(io::ErrorKind::InvalidData, "input exceeds byte limit")));
        }
        bytes.try_reserve(count).map_err(|_| io_error(
            "allocate input", path, io::Error::new(io::ErrorKind::Other, "input allocation failed"),
        ))?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}

fn push_text(plan: &mut GenerationPlan, path: &str, format: OutputFormat, mut text: String) -> CliResult<()> {
    if !text.ends_with('\n') { text.push('\n'); }
    while text.ends_with("\n\n") { text.pop(); }
    if text.len() > MAX_DOCUMENT_BYTES as usize {
        return Err(CliError::Validation(jmax_abi::error::Error::new(
            jmax_abi::error::ErrorCode::PayloadTooLarge, jmax_abi::error::ErrorSource::Encode,
            0, "generated artifact exceeds output byte limit",
        )));
    }
    plan.artifacts.push(Artifact::new(PathBuf::from(path), format, text.into_bytes())?);
    Ok(())
}

fn json_string(output: &mut String, value: &str) {
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""), '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"), '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"), '\r' => output.push_str("\\r"), '\t' => output.push_str("\\t"),
            ch if ch <= '\u{1f}' => output.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
}

fn generator_contract_error(detail: &'static str) -> CliError {
    CliError::Validation(jmax_abi::error::Error::new(
        jmax_abi::error::ErrorCode::ConstraintViolation,
        jmax_abi::error::ErrorSource::SchemaCompile, 0, detail,
    ))
}

fn normalized_path(path: &Path) -> CliResult<String> {
    validate_artifact_path(path)?;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| CliError::InvalidArtifact {
                path: path.to_path_buf(), detail: "artifact path is not UTF-8",
            })?),
            _ => return Err(CliError::InvalidArtifact { path: path.to_path_buf(), detail: "invalid artifact component" }),
        }
    }
    Ok(parts.join("/"))
}

fn validate_output_root(output: &Path) -> CliResult<()> {
    if output.as_os_str().is_empty() { return Err(CliError::InvalidArtifact { path: output.to_path_buf(), detail: "empty output path" }); }
    let mut cursor = PathBuf::new();
    for component in output.components() {
        cursor.push(component.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(CliError::InvalidArtifact { path: cursor, detail: "symlink in output path" }),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect output path", &cursor, error)),
        }
    }
    Ok(())
}

fn ensure_safe_parent(root: &Path, destination: &Path) -> CliResult<()> {
    let relative = destination.strip_prefix(root).map_err(|_| CliError::InvalidArtifact {
        path: destination.to_path_buf(), detail: "artifact escapes output directory",
    })?;
    let mut cursor = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let Component::Normal(name) = component else {
            return Err(CliError::InvalidArtifact { path: relative.to_path_buf(), detail: "invalid destination component" });
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => return Err(CliError::InvalidArtifact { path: relative.to_path_buf(), detail: "unsafe destination ancestor" }),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect artifact directory", &cursor, error)),
        }
    }
    Ok(())
}

fn create_missing_directories(root: &Path, parent: &Path) -> CliResult<()> {
    let relative = parent.strip_prefix(root).map_err(|_| CliError::InvalidArtifact { path: parent.to_path_buf(), detail: "artifact escapes output directory" })?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else { return Err(CliError::InvalidArtifact { path: relative.to_path_buf(), detail: "invalid directory component" }); };
        cursor.push(name);
        match fs::create_dir(&cursor) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&cursor).map_err(|error| io_error("inspect artifact directory", &cursor, error))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() { return Err(CliError::InvalidArtifact { path: cursor, detail: "unsafe artifact directory" }); }
            }
            Err(error) => return Err(io_error("create artifact directory", &cursor, error)),
        }
    }
    Ok(())
}

fn file_equals(path: &Path, expected: &[u8]) -> CliResult<bool> {
    let mut file = File::open(path).map_err(|error| io_error("read destination", path, error))?;
    let mut buffer = [0u8; 8192];
    let mut offset = 0usize;
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => return Ok(offset == expected.len()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error("read destination", path, error)),
        };
        if count > expected.len().saturating_sub(offset) { return Ok(false); }
        if buffer[..count] != expected[offset..offset + count] { return Ok(false); }
        offset += count;
    }
}

fn create_temporary(parent: &Path, destination: &Path) -> CliResult<(PathBuf, File)> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".jsonmax-{}-{sequence}.tmp", std::process::id());
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create temporary artifact", destination, error)),
        }
    }
    Err(io_error("create temporary artifact", destination, io::Error::new(io::ErrorKind::AlreadyExists, "temporary name space exhausted")))
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> CliError {
    CliError::Io { operation, path: path.to_path_buf(), source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, time::{SystemTime, UNIX_EPOCH}};

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        env::temp_dir().join(format!("jmax-gen-pipeline-{name}-{}-{nonce}", std::process::id()))
    }

    fn options(input: PathBuf, formats: Vec<OutputFormat>) -> Options {
        Options { input, output: PathBuf::from("unused"), formats, mode: super::super::RunMode::Write, legacy: false }
    }

    #[test]
    fn format_validation_rejects_empty_and_duplicates() {
        assert!(matches!(validate_formats(&[]), Err(CliError::Usage(_))));
        assert!(matches!(validate_formats(&[OutputFormat::Rust, OutputFormat::Rust]), Err(CliError::Usage(_))));
        assert!(validate_formats(&[OutputFormat::Rust, OutputFormat::Services]).is_ok());
    }

    #[test]
    fn bounded_read_accepts_limit_and_rejects_one_byte_over() {
        let dir = temp_dir("bounds");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("input");
        fs::write(&path, b"abcd").unwrap();
        assert_eq!(read_bounded(&path, 4, "read input").unwrap(), b"abcd");
        fs::write(&path, b"abcde").unwrap();
        assert!(matches!(read_bounded(&path, 4, "read input"), Err(CliError::Validation(_))));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_input_resolves_services_entrypoint() {
        let dir = temp_dir("resolve");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(resolve_input(&dir).unwrap(), dir.join("services.ron"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn generation_preserves_missing_input_path_and_operation() {
        let path = temp_dir("missing").join("services.ron");
        let options = options(path.clone(), vec![OutputFormat::Services]);
        match generate(&options).unwrap_err() {
            CliError::Io { operation, path: actual, .. } => {
                assert_eq!(operation, "inspect input");
                assert_eq!(actual, path);
            }
            error => panic!("unexpected error: {error}"),
        }
    }

    #[test]
    fn generation_requires_the_adjacent_version_file() {
        let dir = temp_dir("version");
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("services.ron");
        fs::write(&input, "()").unwrap();
        let options = options(input, vec![OutputFormat::Services]);
        match generate(&options).unwrap_err() {
            CliError::Io { operation, path, source } => {
                assert_eq!(operation, "read version");
                assert_eq!(path, dir.join("version.toml"));
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            error => panic!("unexpected error: {error}"),
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writer_rejects_invalid_and_case_colliding_plans_before_writing() {
        let root = temp_dir("invalid-plan");
        let plan = GenerationPlan { artifacts: vec![
            Artifact::new(PathBuf::from("A.txt"), OutputFormat::Rust, b"a\n".to_vec()).unwrap(),
            Artifact::new(PathBuf::from("a.txt"), OutputFormat::Rust, b"b\n".to_vec()).unwrap(),
        ] };
        assert!(matches!(write_plan(&root, &plan), Err(CliError::InvalidArtifact { .. })));
        assert!(!root.exists());
    }

    #[test]
    fn writer_stages_bytes_and_does_not_rewrite_unchanged_content() {
        let root = temp_dir("write");
        let plan = GenerationPlan { artifacts: vec![Artifact::new(
            PathBuf::from("nested/output.txt"), OutputFormat::Services, b"stable\n".to_vec(),
        ).unwrap()] };
        write_plan(&root, &plan).unwrap();
        let destination = root.join("nested/output.txt");
        let before = fs::metadata(&destination).unwrap().modified().unwrap();
        write_plan(&root, &plan).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"stable\n");
        assert_eq!(fs::metadata(&destination).unwrap().modified().unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}
