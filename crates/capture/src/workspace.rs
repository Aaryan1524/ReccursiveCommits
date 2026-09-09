use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
    process::Command,
};

use reccursive_core::{FeatureId, Revision};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Explicit uncommitted input copied from the user checkout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Prerequisite {
    pub path: String,
    pub state: PrerequisiteState,
}

/// Filesystem state captured for an explicitly owned prerequisite.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteState {
    Present,
    Deleted,
}

/// Inputs for creating one isolated build workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRequest<'a> {
    pub feature_id: FeatureId,
    pub revision: Revision,
    pub source_checkout: &'a Path,
    pub workspace_root: &'a Path,
    pub base_ref: &'a str,
    pub prerequisite_paths: &'a [String],
}

/// A ready detached workspace and the exact inputs it owns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnedWorkspace {
    pub path: PathBuf,
    pub base_commit: String,
    pub prerequisites: Vec<Prerequisite>,
}

impl OwnedWorkspace {
    /// Creates a clean detached clone and applies only explicitly selected dirty files.
    pub fn create(request: WorkspaceRequest<'_>) -> Result<Self, WorkspaceError> {
        validate_source(request.source_checkout)?;
        let requested = normalize_requested_paths(request.prerequisite_paths)?;
        let before = SourceState::read(request.source_checkout)?;
        let dirty = dirty_entries(request.source_checkout)?;
        if dirty.values().any(|entry| entry.ambiguous) {
            return Err(WorkspaceError::AmbiguousCheckout);
        }
        for path in &requested {
            if !dirty.contains_key(path) {
                return Err(WorkspaceError::PrerequisiteNotDirty { path: path.clone() });
            }
        }

        let base_commit = git_text(
            request.source_checkout,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from(format!("{}^{{commit}}", request.base_ref)),
            ],
        )?;
        let workspace_root = prepare_workspace_root(request.workspace_root)?;
        let feature_root = workspace_root.join(request.feature_id.to_string());
        match fs::symlink_metadata(&feature_root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(WorkspaceError::UnsafeWorkspacePath { path: feature_root }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&feature_root)?;
            }
            Err(error) => return Err(WorkspaceError::Io(error)),
        }
        let path = feature_root.join(format!("revision-{}", request.revision.get()));
        if fs::symlink_metadata(&path).is_ok() {
            return Err(WorkspaceError::AlreadyExists { path });
        }

        let result = (|| {
            git_global([
                OsString::from("clone"),
                OsString::from("--quiet"),
                OsString::from("--no-hardlinks"),
                OsString::from("--no-checkout"),
                OsString::from("--"),
                request.source_checkout.as_os_str().to_owned(),
                path.as_os_str().to_owned(),
            ])?;
            git_status(
                &path,
                [
                    OsString::from("checkout"),
                    OsString::from("--quiet"),
                    OsString::from("--detach"),
                    OsString::from(&base_commit),
                ],
            )?;

            let mut prerequisites = Vec::with_capacity(requested.len());
            for relative in &requested {
                ensure_no_symlink_ancestors(request.source_checkout, Path::new(relative))?;
                ensure_no_symlink_ancestors(&path, Path::new(relative))?;
                let source = request.source_checkout.join(relative);
                let destination = path.join(relative);
                let state = copy_prerequisite(&source, &destination)?;
                prerequisites.push(Prerequisite {
                    path: relative.clone(),
                    state,
                });
            }
            let after = SourceState::read(request.source_checkout)?;
            if before != after {
                return Err(WorkspaceError::SourceChanged);
            }
            Ok(Self {
                path: path.clone(),
                base_commit,
                prerequisites,
            })
        })();
        if result.is_err() && path.starts_with(&workspace_root) {
            let _ = fs::remove_dir_all(&path);
        }
        result
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirtyEntry {
    ambiguous: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceState {
    head: String,
    branch: String,
    status: Vec<u8>,
}

impl SourceState {
    fn read(checkout: &Path) -> Result<Self, WorkspaceError> {
        Ok(Self {
            head: git_text(
                checkout,
                [OsString::from("rev-parse"), OsString::from("HEAD")],
            )?,
            branch: git_text(
                checkout,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--symbolic-full-name"),
                    OsString::from("HEAD"),
                ],
            )?,
            status: git_bytes(
                checkout,
                [
                    OsString::from("status"),
                    OsString::from("--porcelain=v1"),
                    OsString::from("-z"),
                    OsString::from("--untracked-files=all"),
                ],
            )?,
        })
    }
}

fn validate_source(path: &Path) -> Result<(), WorkspaceError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| WorkspaceError::InvalidSource {
        path: path.to_path_buf(),
    })?;
    if !path.is_absolute() || !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorkspaceError::InvalidSource {
            path: path.to_path_buf(),
        });
    }
    git_status(
        path,
        [
            OsString::from("rev-parse"),
            OsString::from("--is-inside-work-tree"),
        ],
    )
}

fn prepare_workspace_root(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if !path.is_absolute() {
        return Err(WorkspaceError::InvalidRoot {
            path: path.to_path_buf(),
        });
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(WorkspaceError::UnsafeWorkspacePath {
                path: path.to_path_buf(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(WorkspaceError::Io(error)),
    }
    fs::canonicalize(path).map_err(WorkspaceError::Io)
}

fn ensure_no_symlink_ancestors(root: &Path, relative: &Path) -> Result<(), WorkspaceError> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let Component::Normal(part) = component else {
            return Err(WorkspaceError::UnsafePrerequisite {
                path: relative.to_string_lossy().into_owned(),
            });
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(WorkspaceError::UnsafePrerequisite {
                    path: relative.to_string_lossy().into_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(WorkspaceError::Io(error)),
        }
    }
    Ok(())
}

fn normalize_requested_paths(paths: &[String]) -> Result<BTreeSet<String>, WorkspaceError> {
    let mut normalized = BTreeSet::new();
    for value in paths {
        let path = Path::new(value);
        if value.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(WorkspaceError::UnsafePrerequisite {
                path: value.clone(),
            });
        }
        let parts: Vec<_> = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => part.to_str(),
                Component::CurDir => None,
                _ => None,
            })
            .collect();
        if parts.is_empty() || parts[0] == ".git" {
            return Err(WorkspaceError::UnsafePrerequisite {
                path: value.clone(),
            });
        }
        let value = parts.join("/");
        if !normalized.insert(value.clone()) {
            return Err(WorkspaceError::DuplicatePrerequisite { path: value });
        }
    }
    Ok(normalized)
}

fn dirty_entries(checkout: &Path) -> Result<BTreeMap<String, DirtyEntry>, WorkspaceError> {
    let output = git_bytes(
        checkout,
        [
            OsString::from("status"),
            OsString::from("--porcelain=v1"),
            OsString::from("-z"),
            OsString::from("--untracked-files=all"),
        ],
    )?;
    let records: Vec<&[u8]> = output
        .split(|byte| *byte == 0)
        .filter(|it| !it.is_empty())
        .collect();
    let mut entries = BTreeMap::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.len() < 4 || record[2] != b' ' {
            return Err(WorkspaceError::MalformedStatus);
        }
        let status = &record[..2];
        let path = std::str::from_utf8(&record[3..])
            .map_err(|_| WorkspaceError::NonUtf8Path)?
            .to_owned();
        let ambiguous = status
            .iter()
            .any(|value| matches!(*value, b'R' | b'C' | b'U'))
            || matches!(status, b"AA" | b"DD");
        entries.insert(path, DirtyEntry { ambiguous });
        if status.iter().any(|value| matches!(*value, b'R' | b'C')) {
            index += 1;
            if index >= records.len() {
                return Err(WorkspaceError::MalformedStatus);
            }
        }
        index += 1;
    }
    Ok(entries)
}

fn copy_prerequisite(
    source: &Path,
    destination: &Path,
) -> Result<PrerequisiteState, WorkspaceError> {
    match fs::symlink_metadata(source) {
        Ok(metadata) => {
            if metadata.is_dir() {
                return Err(WorkspaceError::DirectoryPrerequisite {
                    path: source.to_path_buf(),
                });
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            remove_existing(destination)?;
            if metadata.file_type().is_symlink() {
                symlink(fs::read_link(source)?, destination)?;
            } else if metadata.is_file() {
                fs::copy(source, destination)?;
                fs::set_permissions(
                    destination,
                    fs::Permissions::from_mode(metadata.permissions().mode()),
                )?;
            } else {
                return Err(WorkspaceError::UnsupportedFileType {
                    path: source.to_path_buf(),
                });
            }
            Ok(PrerequisiteState::Present)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_existing(destination)?;
            Ok(PrerequisiteState::Deleted)
        }
        Err(error) => Err(WorkspaceError::Io(error)),
    }
}

fn remove_existing(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?;
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(WorkspaceError::Io(error)),
    }
    Ok(())
}

fn git_text<I>(directory: &Path, arguments: I) -> Result<String, WorkspaceError>
where
    I: IntoIterator<Item = OsString>,
{
    let output = git_bytes(directory, arguments)?;
    String::from_utf8(output)
        .map(|value| value.trim().to_owned())
        .map_err(|_| WorkspaceError::NonUtf8Output)
}

fn git_bytes<I>(directory: &Path, arguments: I) -> Result<Vec<u8>, WorkspaceError>
where
    I: IntoIterator<Item = OsString>,
{
    run_git(Some(directory), arguments)
}

fn git_status<I>(directory: &Path, arguments: I) -> Result<(), WorkspaceError>
where
    I: IntoIterator<Item = OsString>,
{
    run_git(Some(directory), arguments).map(|_| ())
}

fn git_global<I>(arguments: I) -> Result<Vec<u8>, WorkspaceError>
where
    I: IntoIterator<Item = OsString>,
{
    run_git(None, arguments)
}

fn run_git<I>(directory: Option<&Path>, arguments: I) -> Result<Vec<u8>, WorkspaceError>
where
    I: IntoIterator<Item = OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().collect();
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command.args(&arguments).output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(WorkspaceError::Git {
            arguments: arguments
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Owned-workspace creation failure.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("source checkout must be an existing absolute directory: {}", path.display())]
    InvalidSource { path: PathBuf },
    #[error("workspace root has no usable parent: {}", path.display())]
    InvalidRoot { path: PathBuf },
    #[error("workspace path is a symlink or non-directory: {}", path.display())]
    UnsafeWorkspacePath { path: PathBuf },
    #[error("owned workspace already exists: {}", path.display())]
    AlreadyExists { path: PathBuf },
    #[error("prerequisite path is unsafe: {path:?}")]
    UnsafePrerequisite { path: String },
    #[error("prerequisite path appears more than once: {path:?}")]
    DuplicatePrerequisite { path: String },
    #[error("requested prerequisite is not an uncommitted path: {path:?}")]
    PrerequisiteNotDirty { path: String },
    #[error("checkout contains a rename, copy, or unresolved merge; resolve it before capture")]
    AmbiguousCheckout,
    #[error("Git returned malformed porcelain status")]
    MalformedStatus,
    #[error("non-UTF-8 paths are not supported for explicit prerequisite ownership")]
    NonUtf8Path,
    #[error("Git returned non-UTF-8 output")]
    NonUtf8Output,
    #[error("prerequisite must name a file, not a directory: {}", path.display())]
    DirectoryPrerequisite { path: PathBuf },
    #[error("unsupported prerequisite file type: {}", path.display())]
    UnsupportedFileType { path: PathBuf },
    #[error("source checkout changed while the owned workspace was being created")]
    SourceChanged,
    #[error("Git command {arguments:?} failed: {message}")]
    Git {
        arguments: Vec<String>,
        message: String,
    },
    #[error("workspace filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use tempfile::tempdir;

    fn repository() -> (tempfile::TempDir, PathBuf) {
        let root = tempdir().unwrap();
        let checkout = root.path().join("source repo");
        fs::create_dir(&checkout).unwrap();
        git_status(
            &checkout,
            [OsString::from("init"), OsString::from("--quiet")],
        )
        .unwrap();
        git_status(
            &checkout,
            [
                OsString::from("config"),
                OsString::from("user.name"),
                OsString::from("Fixture"),
            ],
        )
        .unwrap();
        git_status(
            &checkout,
            [
                OsString::from("config"),
                OsString::from("user.email"),
                OsString::from("fixture@example.invalid"),
            ],
        )
        .unwrap();
        fs::write(checkout.join("owned.txt"), "base\n").unwrap();
        fs::write(checkout.join("unrelated.txt"), "leave me\n").unwrap();
        git_status(&checkout, [OsString::from("add"), OsString::from(".")]).unwrap();
        git_status(
            &checkout,
            [
                OsString::from("commit"),
                OsString::from("--quiet"),
                OsString::from("-m"),
                OsString::from("base"),
            ],
        )
        .unwrap();
        (root, checkout)
    }

    #[test]
    fn selected_changes_are_copied_without_touching_the_source() {
        let (root, checkout) = repository();
        fs::write(checkout.join("owned.txt"), "changed\n").unwrap();
        fs::write(checkout.join("unmentioned.txt"), "private\n").unwrap();
        let before = SourceState::read(&checkout).unwrap();
        let workspace = OwnedWorkspace::create(WorkspaceRequest {
            feature_id: FeatureId::new(),
            revision: Revision::FIRST,
            source_checkout: &checkout,
            workspace_root: &root.path().join("managed workspaces"),
            base_ref: "HEAD",
            prerequisite_paths: &["owned.txt".into()],
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(workspace.path.join("owned.txt")).unwrap(),
            "changed\n"
        );
        assert!(!workspace.path.join("unmentioned.txt").exists());
        assert_eq!(SourceState::read(&checkout).unwrap(), before);
        assert_eq!(
            fs::read_to_string(checkout.join("unmentioned.txt")).unwrap(),
            "private\n"
        );
    }

    #[test]
    fn deleted_binary_executable_and_symlink_inputs_are_reproduced() {
        let (root, checkout) = repository();
        fs::remove_file(checkout.join("owned.txt")).unwrap();
        let binary = checkout.join("binary.bin");
        let mut file = fs::File::create(&binary).unwrap();
        file.write_all(&[0, 159, 146, 150]).unwrap();
        let executable = checkout.join("run.sh");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("run.sh", checkout.join("run-link")).unwrap();
        let workspace = OwnedWorkspace::create(WorkspaceRequest {
            feature_id: FeatureId::new(),
            revision: Revision::FIRST,
            source_checkout: &checkout,
            workspace_root: &root.path().join("workspaces"),
            base_ref: "HEAD",
            prerequisite_paths: &[
                "owned.txt".into(),
                "binary.bin".into(),
                "run.sh".into(),
                "run-link".into(),
            ],
        })
        .unwrap();

        assert!(!workspace.path.join("owned.txt").exists());
        assert_eq!(
            fs::read(workspace.path.join("binary.bin")).unwrap(),
            [0, 159, 146, 150]
        );
        assert_ne!(
            fs::metadata(workspace.path.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            fs::read_link(workspace.path.join("run-link")).unwrap(),
            Path::new("run.sh")
        );
    }

    #[test]
    fn implicit_clean_and_unsafe_paths_are_refused() {
        let (root, checkout) = repository();
        let base = WorkspaceRequest {
            feature_id: FeatureId::new(),
            revision: Revision::FIRST,
            source_checkout: &checkout,
            workspace_root: &root.path().join("workspaces"),
            base_ref: "HEAD",
            prerequisite_paths: &["owned.txt".into()],
        };
        assert!(matches!(
            OwnedWorkspace::create(base),
            Err(WorkspaceError::PrerequisiteNotDirty { .. })
        ));

        assert!(matches!(
            normalize_requested_paths(&["../secret".into()]),
            Err(WorkspaceError::UnsafePrerequisite { .. })
        ));
        assert!(matches!(
            normalize_requested_paths(&[".git/config".into()]),
            Err(WorkspaceError::UnsafePrerequisite { .. })
        ));
    }
}
