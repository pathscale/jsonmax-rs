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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn artifact(path: &str, format: OutputFormat, bytes: &[u8]) -> super::super::Artifact {
        super::super::Artifact {
            path: PathBuf::from(path),
            format,
            bytes: bytes.to_vec(),
        }
    }

    fn plan(artifacts: Vec<super::super::Artifact>) -> GenerationPlan {
        GenerationPlan { artifacts }
    }

    fn entry(path: &str, format: OutputFormat, len: u64, hash: u64) -> ManifestEntry {
        ManifestEntry {
            path: PathBuf::from(path),
            format,
            byte_len: len,
            content_hash: hash,
        }
    }

    #[test]
    fn fnv1a_vectors_include_empty_binary_and_known_ascii_values() {
        assert_eq!(content_hash(b""), 14_695_981_039_346_656_037);
        assert_eq!(content_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(content_hash(b"hello"), 0xa430_d846_80aa_bd0b);
        assert_eq!(content_hash(&[0, 1, 255]), 0xd94a_3718_6c0d_38bf);
    }

    #[test]
    fn build_sorts_normalizes_and_hashes_without_changing_plan() {
        let input = plan(vec![
            artifact("z/end", OutputFormat::Rust, b"ignored"),
            artifact("b/second", OutputFormat::OpenApi, b""),
            artifact("a/first", OutputFormat::Services, b"abc"),
        ]);
        let original = input.clone();
        let manifest = build_manifest(&input).unwrap();
        assert_eq!(input, original);
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.entries.iter().map(|value| value.path.clone()).collect::<Vec<_>>(), vec![
            PathBuf::from("a/first"), PathBuf::from("b/second"), PathBuf::from("z/end"),
        ]);
        assert_eq!(manifest.entries[0].byte_len, 3);
        assert_eq!(manifest.entries[0].content_hash, content_hash(b"abc"));
        assert_eq!(manifest.entries[1].byte_len, 0);
        assert_eq!(manifest.entries[1].content_hash, content_hash(b""));
    }

    #[test]
    fn build_rejects_exact_duplicates_and_ascii_case_collisions() {
        for second in ["a/file", "A/FILE", "a/./file"] {
            let input = plan(vec![
                artifact("a/file", OutputFormat::Rust, b"1"),
                artifact(second, OutputFormat::Services, b"2"),
            ]);
            assert!(matches!(build_manifest(&input), Err(CliError::InvalidArtifact { detail: "duplicate or case-colliding destination", .. })));
        }
    }

    #[test]
    fn path_validation_covers_traversal_controls_and_windows_names() {
        for path in [
            "", ".", "../file", "a/../../file", "/absolute", "CON", "nested/NUL.txt",
            "COM1.log", "LPT9", "trailing.", "trailing ", "bad?name", "bad\tname", "bad\nname",
        ] {
            assert!(matches!(build_manifest(&plan(vec![artifact(path, OutputFormat::Rust, b"")])), Err(CliError::InvalidArtifact { .. })), "accepted {path:?}");
        }
        assert!(matches!(build_manifest(&plan(vec![artifact("conifer/file", OutputFormat::Rust, b"")])), Ok(_)));
    }

    #[test]
    fn empty_plan_has_header_only_manifest() {
        let manifest = build_manifest(&GenerationPlan::default()).unwrap();
        assert!(manifest.entries.is_empty());
        assert_eq!(render_manifest(&manifest).unwrap(), b"jmax-artifacts\t1\n");
    }

    #[test]
    fn rendering_is_exact_for_all_format_tokens_and_hash_boundaries() {
        let manifest = ArtifactManifest {
            version: MANIFEST_VERSION,
            entries: vec![
                entry("f.md", OutputFormat::Markdown, 0, 0),
                entry("e.yaml", OutputFormat::AsyncApi, 10, 0xabc),
                entry("d.json", OutputFormat::OpenApi, 9, u64::MAX),
                entry("c.json", OutputFormat::JsonSchema, 8, 1),
                entry("b.json", OutputFormat::Services, 7, 2),
                entry("a.rs", OutputFormat::Rust, 6, 3),
            ],
        };
        assert_eq!(render_manifest(&manifest).unwrap(), concat!(
            "jmax-artifacts\t1\n",
            "a.rs\trust\t6\t0000000000000003\n",
            "b.json\tservices\t7\t0000000000000002\n",
            "c.json\tjson-schema\t8\t0000000000000001\n",
            "d.json\topenapi\t9\tffffffffffffffff\n",
            "e.yaml\tasyncapi\t10\t0000000000000abc\n",
            "f.md\tmarkdown\t0\t0000000000000000\n",
        ).as_bytes());
        assert_eq!(manifest.entries[0].path, PathBuf::from("f.md"));
    }

    #[test]
    fn rendering_revalidates_version_paths_and_collisions() {
        let wrong_version = ArtifactManifest { version: MANIFEST_VERSION + 1, entries: Vec::new() };
        assert!(matches!(render_manifest(&wrong_version), Err(CliError::InvalidArtifact { detail: "unsupported manifest version", .. })));

        for path in ["../escape", "bad\tpath", "A/file."] {
            let invalid_manifest = ArtifactManifest {
                version: MANIFEST_VERSION,
                entries: vec![entry(path, OutputFormat::Rust, 0, 0)],
            };
            assert!(matches!(render_manifest(&invalid_manifest), Err(CliError::InvalidArtifact { .. })));
        }

        let collision = ArtifactManifest {
            version: MANIFEST_VERSION,
            entries: vec![entry("a/file", OutputFormat::Rust, 0, 0), entry("A/FILE", OutputFormat::Rust, 0, 0)],
        };
        assert!(matches!(render_manifest(&collision), Err(CliError::InvalidArtifact { detail: "duplicate or case-colliding destination", .. })));
    }

    #[test]
    fn rendering_sorts_a_copy_and_normalizes_native_components() {
        let manifest = ArtifactManifest {
            version: MANIFEST_VERSION,
            entries: vec![
                entry("z/last", OutputFormat::Rust, 1, 0),
                entry("a/./first", OutputFormat::Services, 2, 0),
            ],
        };
        let original = manifest.clone();
        assert_eq!(render_manifest(&manifest).unwrap(), b"jmax-artifacts\t1\na/first\tservices\t2\t0000000000000000\nz/last\trust\t1\t0000000000000000\n");
        assert_eq!(manifest, original);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_native_components_are_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![b'x', 0xff]));
        let result = build_manifest(&plan(vec![super::super::Artifact {
            path,
            format: OutputFormat::Rust,
            bytes: Vec::new(),
        }]));
        assert!(matches!(result, Err(CliError::InvalidArtifact { detail: "manifest paths must be UTF-8", .. })));
    }
}
