//! Deterministic validation and rendering of generated artifact manifests.

use super::{
    validate_artifact_path, ArtifactManifest, CliError, CliResult, GenerationPlan,
    ManifestEntry, OutputFormat, MANIFEST_VERSION,
};
use std::{
    collections::BTreeMap,
    fmt::Write as FmtWrite,
    path::{Component, Path, PathBuf},
};

const FNV1A64_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV1A64_PRIME: u64 = 1_099_511_628_211;

/// Build a canonical manifest after validating every artifact path and destination.
///
/// Paths are represented with forward slashes, and entries are sorted by their
/// UTF-8 bytes. Neither the plan nor any artifact content is modified.
pub fn build_manifest(plan: &GenerationPlan) -> CliResult<ArtifactManifest> {
    let mut by_path = BTreeMap::<String, ManifestEntry>::new();
    let mut folded_paths = BTreeMap::<String, String>::new();

    for artifact in &plan.artifacts {
        let normalized = normalize_path(&artifact.path)?;
        let folded = normalized.to_ascii_lowercase();
        if folded_paths.insert(folded, normalized.clone()).is_some() {
            return Err(invalid(
                &artifact.path,
                "duplicate or case-colliding destination",
            ));
        }

        let byte_len = u64::try_from(artifact.bytes.len()).map_err(|_| {
            invalid(&artifact.path, "artifact byte length does not fit in u64")
        })?;
        let entry = ManifestEntry {
            path: PathBuf::from(&normalized),
            format: artifact.format,
            byte_len,
            content_hash: content_hash(&artifact.bytes),
        };
        // folded_paths catches canonical duplicates before this insertion.
        by_path.insert(normalized, entry);
    }

    Ok(ArtifactManifest {
        version: MANIFEST_VERSION,
        entries: by_path.into_iter().map(|(_, entry)| entry).collect(),
    })
}

/// Render a manifest as deterministic UTF-8 TSV.
///
/// A caller can construct or mutate `ArtifactManifest`, so version, paths,
/// lengths, and destination uniqueness are checked again at this boundary.
pub fn render_manifest(manifest: &ArtifactManifest) -> CliResult<Vec<u8>> {
    if manifest.version != MANIFEST_VERSION {
        return Err(invalid(
            Path::new("."),
            "unsupported manifest version",
        ));
    }

    let mut entries = Vec::with_capacity(manifest.entries.len());
    let mut folded_paths = BTreeMap::<String, String>::new();
    for entry in &manifest.entries {
        let normalized = normalize_path(&entry.path)?;
        let folded = normalized.to_ascii_lowercase();
        if folded_paths.insert(folded, normalized.clone()).is_some() {
            return Err(invalid(
                &entry.path,
                "duplicate or case-colliding destination",
            ));
        }
        entries.push((normalized, entry));
    }
    entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

    let mut output = String::from("jmax-artifacts\t1\n");
    for (path, entry) in entries {
        let token = format_token(entry.format);
        let _ = writeln!(
            output,
            "{path}\t{token}\t{}\t{:016x}",
            entry.byte_len, entry.content_hash
        );
    }
    Ok(output.into_bytes())
}

/// Return the FNV-1a-64 identity of the supplied bytes.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A64_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
    hash
}

fn normalize_path(path: &Path) -> CliResult<String> {
    validate_artifact_path(path)?;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return Err(invalid(path, "manifest paths must be UTF-8"));
                };
                components.push(name);
            }
            _ => return Err(invalid(path, "expected normal path component")),
        }
    }
    if components.is_empty() {
        return Err(invalid(path, "expected a nonempty relative path"));
    }
    Ok(components.join("/"))
}

fn format_token(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Rust => "rust",
        OutputFormat::Services => "services",
        OutputFormat::JsonSchema => "json-schema",
        OutputFormat::OpenApi => "openapi",
        OutputFormat::AsyncApi => "asyncapi",
        OutputFormat::Markdown => "markdown",
    }
}

fn invalid(path: &Path, detail: &'static str) -> CliError {
    CliError::InvalidArtifact {
        path: path.to_path_buf(),
        detail,
    }
}
