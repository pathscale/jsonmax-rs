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
