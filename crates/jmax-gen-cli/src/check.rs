//! Read-only artifact drift comparison.

use super::{
    validate_artifact_path, Artifact, CheckReport, CliError, CliResult, Drift, DriftKind,
    GenerationPlan,
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

/// Compare every planned artifact with the corresponding file under `output`.
///
/// Plan validation happens before any filesystem inspection. The output tree is
/// only inspected; this function never creates or changes a file or directory.
pub fn compare(output: &Path, plan: &GenerationPlan) -> CliResult<CheckReport> {
    let manifest = super::manifest::build_manifest(plan)?;
    let mut canonical_artifacts = BTreeMap::new();
    for artifact in &plan.artifacts {
        validate_artifact_path(&artifact.path)?;
        canonical_artifacts.insert(normalize_artifact_path(&artifact.path)?, artifact);
    }

    let output_exists = validate_output_root(output)?;
    let mut report = CheckReport {
        compared: manifest.entries.len(),
        drift: Vec::new(),
    };

    for entry in &manifest.entries {
        let canonical = entry.path.to_str().ok_or_else(|| CliError::InvalidArtifact {
            path: entry.path.clone(),
            detail: "manifest paths must be UTF-8",
        })?;
        let artifact = canonical_artifacts.get(canonical).copied().ok_or_else(|| CliError::InvalidArtifact {
                path: entry.path.clone(),
                detail: "manifest entry has no planned artifact",
            })?;

        if !output_exists {
            report.drift.push(Drift { path: entry.path.clone(), kind: DriftKind::Missing });
            continue;
        }

        let destination = output.join(&entry.path);
        match inspect_artifact_path(output, &destination, &entry.path)? {
            PathState::Missing => report.drift.push(Drift {
                path: entry.path.clone(),
                kind: DriftKind::Missing,
            }),
            PathState::File if file_equals(&destination, &artifact.bytes)? => {}
            PathState::File => report.drift.push(Drift {
                path: entry.path.clone(),
                kind: DriftKind::Different,
            }),
        }
    }

    Ok(report)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathState {
    Missing,
    File,
}

/// Check every existing component of the host-selected output root without
/// following symlinks. A missing component means all planned files are absent.
fn validate_output_root(output: &Path) -> CliResult<bool> {
    if output.as_os_str().is_empty() {
        return Err(CliError::InvalidArtifact {
            path: output.to_path_buf(),
            detail: "empty output path",
        });
    }

    let components = output.components().collect::<Vec<_>>();
    let mut cursor = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        cursor.push(component.as_os_str());
        let is_root = index + 1 == components.len();
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(CliError::InvalidArtifact {
                        path: cursor,
                        detail: "symlink in output path",
                    });
                }
                if !is_root && !metadata.is_dir() {
                    return Err(CliError::InvalidArtifact {
                        path: cursor,
                        detail: "output path ancestor is not a directory",
                    });
                }
                if is_root && !metadata.is_dir() {
                    return Err(CliError::InvalidArtifact {
                        path: output.to_path_buf(),
                        detail: "output must be a real directory",
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect output path", &cursor, error)),
        }
    }

    match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(CliError::InvalidArtifact {
                path: output.to_path_buf(),
                detail: "output must be a real directory",
            })
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect output", output, error)),
    }
}

/// Check normal ancestors and the destination itself, rejecting symlinks and
/// all non-directory ancestors exactly as the writer does.
fn inspect_artifact_path(root: &Path, destination: &Path, relative: &Path) -> CliResult<PathState> {
    let parent = destination.parent().ok_or_else(|| CliError::InvalidArtifact {
        path: relative.to_path_buf(),
        detail: "destination has no parent",
    })?;
    let relative_parent = parent.strip_prefix(root).map_err(|_| CliError::InvalidArtifact {
        path: relative.to_path_buf(),
        detail: "artifact escapes output directory",
    })?;
    let mut cursor = root.to_path_buf();
    for component in relative_parent.components() {
        let Component::Normal(name) = component else {
            return Err(CliError::InvalidArtifact {
                path: relative.to_path_buf(),
                detail: "invalid destination ancestor",
            });
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CliError::InvalidArtifact {
                    path: relative.to_path_buf(),
                    detail: "unsafe destination ancestor",
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PathState::Missing),
            Err(error) => return Err(io_error("inspect artifact directory", &cursor, error)),
        }
    }

    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CliError::InvalidArtifact {
                path: relative.to_path_buf(),
                detail: "destination is not a regular file",
            })
        }
        Ok(_) => Ok(PathState::File),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PathState::Missing),
        Err(error) => Err(io_error("inspect destination", destination, error)),
    }
}

/// Compare raw bytes in bounded memory, including the EOF boundary after the
/// expected content. Memory use is independent of destination file length.
fn file_equals(path: &Path, expected: &[u8]) -> CliResult<bool> {
    let mut file = File::open(path).map_err(|error| io_error("read destination", path, error))?;
    let mut buffer = [0u8; 8192];
    let mut offset = 0usize;

    while offset < expected.len() {
        let wanted = (expected.len() - offset).min(buffer.len());
        let count = loop {
            match file.read(&mut buffer[..wanted]) {
                Ok(count) => break count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error("read destination", path, error)),
            }
        };
        if count == 0 || buffer[..count] != expected[offset..offset + count] {
            return Ok(false);
        }
        offset += count;
    }

    loop {
        match file.read(&mut buffer[..1]) {
            Ok(0) => return Ok(true),
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error("read destination", path, error)),
        }
    }
}

fn normalize_artifact_path(path: &Path) -> CliResult<String> {
    validate_artifact_path(path)?;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| CliError::InvalidArtifact {
                path: path.to_path_buf(),
                detail: "manifest paths must be UTF-8",
            })?),
            _ => return Err(CliError::InvalidArtifact {
                path: path.to_path_buf(),
                detail: "expected normal path component",
            }),
        }
    }
    Ok(parts.join("/"))
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> CliError {
    CliError::Io { operation, path: path.to_path_buf(), source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "jmax-gen-check-{}-{label}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn artifact(path: &str, bytes: &[u8]) -> Artifact {
        Artifact::new(PathBuf::from(path), super::super::OutputFormat::Services, bytes.to_vec()).unwrap()
    }

    fn plan(artifacts: Vec<Artifact>) -> GenerationPlan {
        GenerationPlan { artifacts }
    }

    #[test]
    fn clean_comparison_uses_raw_bytes_and_manifest_order() {
        let root = temp_dir("clean");
        fs::create_dir_all(root.join("z")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("z/last.bin"), [0, 255, 10]).unwrap();
        fs::write(root.join("a/first.bin"), b"same\n").unwrap();
        let value = plan(vec![artifact("z/last.bin", &[0, 255, 10]), artifact("a/first.bin", b"same\n")]);

        let report = compare(&root, &value).unwrap();
        assert_eq!(report.compared, 2);
        assert!(report.is_clean());
        assert!(report.drift.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_root_and_missing_normal_ancestor_are_missing_drift() {
        let root = temp_dir("missing-root");
        let value = plan(vec![artifact("nested/a.txt", b"a"), artifact("other/b.txt", b"b")]);
        let report = compare(&root, &value).unwrap();
        assert_eq!(report.compared, 2);
        assert_eq!(report.drift.iter().map(|drift| drift.kind).collect::<Vec<_>>(), vec![DriftKind::Missing; 2]);
        assert!(!root.exists());

        fs::create_dir_all(root.join("nested")).unwrap();
        let report = compare(&root, &plan(vec![artifact("nested/a.txt", b"a"), artifact("other/b.txt", b"b")])).unwrap();
        assert_eq!(report.drift.iter().map(|drift| drift.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("nested/a.txt"), PathBuf::from("other/b.txt")]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_bytes_length_and_empty_file_boundaries_are_checked() {
        let root = temp_dir("boundaries");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("equal"), b"abc").unwrap();
        fs::write(root.join("short"), b"ab").unwrap();
        fs::write(root.join("long"), b"abcd").unwrap();
        fs::write(root.join("empty"), b"").unwrap();
        fs::write(root.join("same-length"), b"abd").unwrap();
        let report = compare(&root, &plan(vec![
            artifact("equal", b"abc"), artifact("short", b"abc"),
            artifact("long", b"abc"), artifact("empty", b""), artifact("same-length", b"abc"),
        ])).unwrap();
        assert_eq!(report.compared, 5);
        assert_eq!(report.drift, vec![
            Drift { path: PathBuf::from("long"), kind: DriftKind::Different },
            Drift { path: PathBuf::from("same-length"), kind: DriftKind::Different },
            Drift { path: PathBuf::from("short"), kind: DriftKind::Different },
        ]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_crosses_internal_read_buffer_boundaries() {
        let root = temp_dir("read-buffer");
        fs::create_dir_all(&root).unwrap();
        let cases = [8191usize, 8192, 8193];
        let mut artifacts = Vec::new();
        for (index, length) in cases.iter().copied().enumerate() {
            let name = format!("case-{index}");
            let bytes = (0..length).map(|offset| (offset % 251) as u8).collect::<Vec<_>>();
            fs::write(root.join(&name), &bytes).unwrap();
            artifacts.push(artifact(&name, &bytes));
        }
        let report = compare(&root, &plan(artifacts)).unwrap();
        assert_eq!(report.compared, 3);
        assert!(report.is_clean());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_plan_is_clean_and_does_not_create_output() {
        let root = temp_dir("empty");
        let report = compare(&root, &GenerationPlan::default()).unwrap();
        assert_eq!(report.compared, 0);
        assert!(report.is_clean());
        assert!(!root.exists());
    }

    #[test]
    fn invalid_complete_plan_fails_before_output_inspection() {
        let root = temp_dir("invalid-plan");
        let value = plan(vec![artifact("A.txt", b"one"), artifact("a.txt", b"two")]);
        assert!(matches!(compare(&root, &value), Err(CliError::InvalidArtifact { .. })));
        assert!(!root.exists());
    }

    #[test]
    fn output_file_and_destination_directory_are_operational_errors() {
        let root_file = temp_dir("root-file");
        fs::write(&root_file, b"not a directory").unwrap();
        assert!(matches!(compare(&root_file, &plan(vec![artifact("a.txt", b"a")])),
            Err(CliError::InvalidArtifact { .. })));
        fs::remove_file(root_file).unwrap();

        let root = temp_dir("destination-directory");
        fs::create_dir_all(root.join("artifact")).unwrap();
        assert!(matches!(compare(&root, &plan(vec![artifact("artifact", b"a")])),
            Err(CliError::InvalidArtifact { .. })));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_destination_and_ancestor_are_rejected() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("symlinks");
        fs::create_dir_all(&root).unwrap();
        let outside = temp_dir("outside");
        fs::write(&outside, b"content").unwrap();
        symlink(&outside, root.join("link-file")).unwrap();
        assert!(matches!(compare(&root, &plan(vec![artifact("link-file", b"content")])),
            Err(CliError::InvalidArtifact { .. })));

        fs::create_dir_all(root.join("actual")).unwrap();
        symlink(root.join("actual"), root.join("link-dir")).unwrap();
        assert!(matches!(compare(&root, &plan(vec![artifact("link-dir/file", b"content")])),
            Err(CliError::InvalidArtifact { .. })));
        fs::remove_dir_all(root).unwrap();
        fs::remove_file(outside).unwrap();
    }

    #[test]
    fn comparison_rejects_invalid_artifact_paths_before_destination_reads() {
        let root = temp_dir("invalid-path");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/file"), b"bytes").unwrap();
        let report = compare(&root, &plan(vec![artifact("nested/file", b"bytes")])).unwrap();
        assert!(report.is_clean());
        let invalid = GenerationPlan { artifacts: vec![Artifact {
            path: PathBuf::from("../escape"),
            format: super::super::OutputFormat::Services,
            bytes: b"bytes".to_vec(),
        }] };
        assert!(matches!(compare(&root, &invalid), Err(CliError::InvalidArtifact { .. })));
        fs::remove_dir_all(root).unwrap();
    }
}
