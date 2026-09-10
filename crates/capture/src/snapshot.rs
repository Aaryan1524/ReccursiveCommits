use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use reccursive_core::{FeatureId, PackageId, Revision, TaskId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{ContentValidationPolicy, ContentViolation};

const SNAPSHOT_SCHEMA_VERSION: u16 = 1;
const BUNDLE_FILE: &str = "objects.bundle";
const MANIFEST_FILE: &str = "manifest.json";

/// Inputs that identify one immutable snapshot package.
#[derive(Clone, Debug)]
pub struct SnapshotRequest<'a> {
    pub package_id: PackageId,
    pub revision: Revision,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
    pub workspace: &'a Path,
    pub package_root: &'a Path,
    pub expected_base_commit: &'a str,
    /// The unit this one was built on top of, when the workspace has already produced a package.
    pub parent_package_id: Option<PackageId>,
    pub validation_policy: ContentValidationPolicy,
}

/// Portable metadata authenticated together with the Git object bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub schema_version: u16,
    pub package_id: PackageId,
    pub revision: Revision,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
    /// Immediately preceding package from the same workspace, if any.
    pub parent_package_id: Option<PackageId>,
    pub base_commit: String,
    pub base_tree: String,
    pub result_tree: String,
    pub snapshot_commit: String,
    pub bundle_sha256: String,
    pub content_hash: String,
}

/// Verified immutable package on local durable storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotPackage {
    pub path: PathBuf,
    pub manifest: SnapshotManifest,
}

impl SnapshotPackage {
    /// The workspace HEAD after this capture, and therefore the next unit's base commit.
    #[must_use]
    pub fn advanced_base_commit(&self) -> &str {
        &self.manifest.snapshot_commit
    }

    /// Captures the workspace through a temporary Git index and atomically installs the package.
    pub fn capture(request: SnapshotRequest<'_>) -> Result<Self, SnapshotError> {
        if request.task_ids.is_empty() {
            return Err(SnapshotError::NoTasks);
        }
        validate_object_id(request.expected_base_commit)?;
        validate_git_workspace(request.workspace)?;
        let package_root = prepare_root(request.package_root)?;
        let parent = package_root.join(request.package_id.to_string());
        prepare_package_parent(&parent)?;
        let destination = parent.join(format!("revision-{}", request.revision.get()));
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(SnapshotError::AlreadyExists { path: destination });
        }
        let temporary = parent.join(format!(".revision-{}.partial", request.revision.get()));
        if fs::symlink_metadata(&temporary).is_ok() {
            return Err(SnapshotError::IncompleteExists { path: temporary });
        }
        fs::create_dir(&temporary)?;

        let capture = (|| {
            let actual_head = git_text(request.workspace, ["rev-parse", "HEAD"])?;
            if actual_head != request.expected_base_commit {
                return Err(SnapshotError::BaseMismatch {
                    expected: request.expected_base_commit.to_owned(),
                    actual: actual_head,
                });
            }
            let base_tree = git_text(request.workspace, ["rev-parse", "HEAD^{tree}"])?;
            let index = temporary.join("capture.index");
            git_with_index(request.workspace, &index, ["read-tree", "HEAD"])?;
            git_with_index(request.workspace, &index, ["add", "-A", "--", "."])?;
            let result_tree = git_text_with_index(request.workspace, &index, ["write-tree"])?;
            validate_result_tree(
                request.workspace,
                &base_tree,
                &result_tree,
                &request.validation_policy,
            )?;
            let snapshot_commit = git_commit_tree(request.workspace, &result_tree, &actual_head)?;
            let snapshot_ref = format!(
                "refs/reccursive/snapshots/{}/{}",
                request.package_id,
                request.revision.get()
            );
            git_status(
                request.workspace,
                [
                    "update-ref",
                    snapshot_ref.as_str(),
                    snapshot_commit.as_str(),
                ],
            )?;
            let bundle = temporary.join(BUNDLE_FILE);
            let bundle_result = git_status_os(
                request.workspace,
                [
                    OsString::from("bundle"),
                    OsString::from("create"),
                    bundle.as_os_str().to_owned(),
                    OsString::from(&snapshot_ref),
                ],
            );
            let _ = git_status(
                request.workspace,
                ["update-ref", "-d", snapshot_ref.as_str()],
            );
            bundle_result?;
            git_status_os(
                request.workspace,
                [
                    OsString::from("bundle"),
                    OsString::from("verify"),
                    bundle.as_os_str().to_owned(),
                ],
            )?;

            let bundle_sha256 = hash_file(&bundle)?;
            let mut manifest = SnapshotManifest {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                package_id: request.package_id,
                revision: request.revision,
                feature_id: request.feature_id,
                plan_revision: request.plan_revision,
                task_ids: request.task_ids,
                parent_package_id: request.parent_package_id,
                base_commit: actual_head,
                base_tree,
                result_tree,
                snapshot_commit,
                bundle_sha256,
                content_hash: String::new(),
            };
            manifest.content_hash = package_hash(&manifest)?;
            write_manifest(&temporary.join(MANIFEST_FILE), &manifest)?;
            sync_directory(&temporary)?;
            fs::rename(&temporary, &destination)?;
            sync_directory(&parent)?;

            // The package is durable from here, so the workspace can safely move forward.
            //
            // Capture reads its base from the workspace HEAD. Without advancing it, every package
            // taken from one workspace shares a base and its result tree is the whole workspace,
            // so unit N silently contains units 1..N-1. Advancing HEAD to this unit's snapshot
            // commit makes the next unit's base exactly this unit's result, so each package is the
            // delta for its own tasks and nothing else.
            //
            // Order matters for crash safety: if the process dies before this, the package is
            // still on disk and startup recovery re-registers it, while the unchanged HEAD means
            // no unit is lost. Failing here surfaces as a capture error rather than a silently
            // overlapping next package.
            git_status(
                request.workspace,
                ["update-ref", "HEAD", manifest.snapshot_commit.as_str()],
            )?;
            // Capture wrote through a temporary index, so the workspace's own index still points at
            // the previous HEAD. Refresh it to the new HEAD, which touches no working-tree file
            // (the tree is already what was just captured) and leaves the workspace clean rather
            // than showing every captured file as deleted-and-untracked.
            git_status(request.workspace, ["read-tree", "HEAD"])?;

            Ok(Self {
                path: destination,
                manifest,
            })
        })();
        if capture.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        capture
    }

    /// Opens a package only after authenticating its manifest and object bundle.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SnapshotError> {
        let path = path.into();
        let manifest: SnapshotManifest =
            serde_json::from_slice(&fs::read(path.join(MANIFEST_FILE))?)?;
        if manifest.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(SnapshotError::UnsupportedSchema {
                found: manifest.schema_version,
                supported: SNAPSHOT_SCHEMA_VERSION,
            });
        }
        for object in [
            &manifest.base_commit,
            &manifest.base_tree,
            &manifest.result_tree,
            &manifest.snapshot_commit,
        ] {
            validate_object_id(object)?;
        }
        let actual_bundle_hash = hash_file(&path.join(BUNDLE_FILE))?;
        if actual_bundle_hash != manifest.bundle_sha256 {
            return Err(SnapshotError::HashMismatch {
                component: "bundle",
            });
        }
        if package_hash(&manifest)? != manifest.content_hash {
            return Err(SnapshotError::HashMismatch {
                component: "manifest",
            });
        }
        Ok(Self { path, manifest })
    }

    /// Copies a verified package into a new empty directory and re-verifies the result.
    ///
    /// This deliberately copies only the authenticated manifest and bundle. Mutable build
    /// workspaces, Git indexes, credentials, and daemon configuration never become part of a
    /// portable queue export.
    pub fn copy_verified_to(&self, destination: &Path) -> Result<Self, SnapshotError> {
        let verified = Self::open(self.path.clone())?;
        if fs::symlink_metadata(destination).is_ok() {
            return Err(SnapshotError::AlreadyExists {
                path: destination.to_path_buf(),
            });
        }
        fs::create_dir(destination)?;
        let copy_result = (|| {
            for name in [BUNDLE_FILE, MANIFEST_FILE] {
                let source = verified.path.join(name);
                let target = destination.join(name);
                let mut input = File::open(source)?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(target)?;
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
            }
            sync_directory(destination)?;
            let copied = Self::open(destination.to_path_buf())?;
            if copied.manifest != verified.manifest {
                return Err(SnapshotError::HashMismatch {
                    component: "copied manifest",
                });
            }
            Ok(copied)
        })();
        if copy_result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        copy_result
    }

    /// Reconstructs the exact result tree into a new detached Git worktree.
    pub fn reconstruct(&self, destination: &Path) -> Result<(), SnapshotError> {
        let verified = Self::open(self.path.clone())?;
        if fs::symlink_metadata(destination).is_ok() {
            return Err(SnapshotError::RestoreDestinationExists {
                path: destination.to_path_buf(),
            });
        }
        fs::create_dir_all(destination)?;
        git_status(destination, ["init", "--quiet"])?;
        let snapshot_ref = format!(
            "refs/reccursive/snapshots/{}/{}",
            verified.manifest.package_id,
            verified.manifest.revision.get()
        );
        git_status_os(
            destination,
            [
                OsString::from("fetch"),
                OsString::from("--quiet"),
                verified.path.join(BUNDLE_FILE).as_os_str().to_owned(),
                OsString::from(snapshot_ref),
            ],
        )?;
        git_status(
            destination,
            [
                "checkout",
                "--quiet",
                "--detach",
                &verified.manifest.snapshot_commit,
            ],
        )?;
        let restored_tree = git_text(destination, ["rev-parse", "HEAD^{tree}"])?;
        if restored_tree != verified.manifest.result_tree {
            return Err(SnapshotError::TreeMismatch {
                expected: verified.manifest.result_tree,
                actual: restored_tree,
            });
        }
        Ok(())
    }
}

fn package_hash(manifest: &SnapshotManifest) -> Result<String, SnapshotError> {
    let mut unsigned = manifest.clone();
    unsigned.content_hash.clear();
    let encoded = serde_json::to_vec(&unsigned)?;
    Ok(hex_digest(Sha256::digest(encoded)))
}

fn hash_file(path: &Path) -> Result<String, SnapshotError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_manifest(path: &Path, manifest: &SnapshotManifest) -> Result<(), SnapshotError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), SnapshotError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn prepare_root(path: &Path) -> Result<PathBuf, SnapshotError> {
    if !path.is_absolute() {
        return Err(SnapshotError::UnsafeRoot {
            path: path.to_path_buf(),
        });
    }
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SnapshotError::UnsafeRoot {
            path: path.to_path_buf(),
        });
    }
    Ok(fs::canonicalize(path)?)
}

fn prepare_package_parent(path: &Path) -> Result<(), SnapshotError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(SnapshotError::UnsafeRoot {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(SnapshotError::Io(error)),
    }
}

fn validate_git_workspace(path: &Path) -> Result<(), SnapshotError> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(SnapshotError::InvalidWorkspace {
            path: path.to_path_buf(),
        });
    }
    git_status(path, ["rev-parse", "--is-inside-work-tree"])
}

fn validate_object_id(value: &str) -> Result<(), SnapshotError> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(SnapshotError::InvalidObjectId {
            value: value.to_owned(),
        })
    }
}

fn git_commit_tree(workspace: &Path, tree: &str, parent: &str) -> Result<String, SnapshotError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args([
            "commit-tree",
            tree,
            "-p",
            parent,
            "-m",
            "Reccursive snapshot",
        ])
        .env("GIT_AUTHOR_NAME", "Reccursive Snapshot")
        .env("GIT_AUTHOR_EMAIL", "snapshot@localhost")
        .env("GIT_COMMITTER_NAME", "Reccursive Snapshot")
        .env("GIT_COMMITTER_EMAIL", "snapshot@localhost")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z");
    command_output(command)
}

fn validate_result_tree(
    workspace: &Path,
    base_tree: &str,
    result_tree: &str,
    policy: &ContentValidationPolicy,
) -> Result<(), SnapshotError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args([
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        base_tree,
        result_tree,
    ]);
    let output = command.output()?;
    if !output.status.success() {
        return Err(SnapshotError::Git {
            command: format!("{command:?}"),
            message: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    let mut violations = Vec::new();
    for raw_path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(raw_path).into_owned();
        let object = format!("{result_tree}:{path}");
        let mut size_command = Command::new("git");
        size_command
            .arg("-C")
            .arg(workspace)
            .args(["cat-file", "-s", &object]);
        let size_output = size_command.output()?;
        if !size_output.status.success() {
            continue;
        }
        let size = String::from_utf8_lossy(&size_output.stdout)
            .trim()
            .parse::<u64>()
            .map_err(|_| SnapshotError::InvalidObjectId {
                value: object.clone(),
            })?;
        let bytes = if size <= policy.secret_scan_max_bytes {
            let mut blob_command = Command::new("git");
            blob_command
                .arg("-C")
                .arg(workspace)
                .args(["cat-file", "blob", &object]);
            let blob_output = blob_command.output()?;
            if blob_output.status.success() {
                Some(blob_output.stdout)
            } else {
                None
            }
        } else {
            None
        };
        violations.extend(policy.validate_path(&path, size, bytes.as_deref()));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(SnapshotError::ContentBlocked { violations })
    }
}

fn git_with_index<const N: usize>(
    workspace: &Path,
    index: &Path,
    arguments: [&str; N],
) -> Result<(), SnapshotError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args(arguments)
        .env("GIT_INDEX_FILE", index);
    command_output(command).map(|_| ())
}

fn git_text_with_index<const N: usize>(
    workspace: &Path,
    index: &Path,
    arguments: [&str; N],
) -> Result<String, SnapshotError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args(arguments)
        .env("GIT_INDEX_FILE", index);
    command_output(command)
}

fn git_text<const N: usize>(
    workspace: &Path,
    arguments: [&str; N],
) -> Result<String, SnapshotError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args(arguments);
    command_output(command)
}

fn git_status<const N: usize>(workspace: &Path, arguments: [&str; N]) -> Result<(), SnapshotError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args(arguments);
    command_output(command).map(|_| ())
}

fn git_status_os<const N: usize>(
    workspace: &Path,
    arguments: [OsString; N],
) -> Result<(), SnapshotError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args(arguments);
    command_output(command).map(|_| ())
}

fn command_output(mut command: Command) -> Result<String, SnapshotError> {
    let debug = format!("{command:?}");
    let output = command.output()?;
    if !output.status.success() {
        return Err(SnapshotError::Git {
            command: debug,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| SnapshotError::NonUtf8Output)
}

/// Immutable snapshot creation or verification failure.
#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("capture content validation blocked {} item(s)", violations.len())]
    ContentBlocked { violations: Vec<ContentViolation> },
    #[error("snapshot must contain at least one task")]
    NoTasks,
    #[error("invalid full Git object ID: {value:?}")]
    InvalidObjectId { value: String },
    #[error("workspace is not an absolute Git worktree: {}", path.display())]
    InvalidWorkspace { path: PathBuf },
    #[error("package root is unsafe: {}", path.display())]
    UnsafeRoot { path: PathBuf },
    #[error("snapshot package already exists: {}", path.display())]
    AlreadyExists { path: PathBuf },
    #[error("an incomplete snapshot already exists: {}", path.display())]
    IncompleteExists { path: PathBuf },
    #[error("workspace base moved from {expected} to {actual}")]
    BaseMismatch { expected: String, actual: String },
    #[error("snapshot schema {found} is unsupported; this build supports {supported}")]
    UnsupportedSchema { found: u16, supported: u16 },
    #[error("snapshot {component} hash does not match its manifest")]
    HashMismatch { component: &'static str },
    #[error("restore destination already exists: {}", path.display())]
    RestoreDestinationExists { path: PathBuf },
    #[error("restored tree {actual} does not match expected tree {expected}")]
    TreeMismatch { expected: String, actual: String },
    #[error("Git command failed ({command}): {message}")]
    Git { command: String, message: String },
    #[error("Git returned non-UTF-8 output")]
    NonUtf8Output,
    #[error("snapshot JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("snapshot filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sequential_units_from_one_workspace_do_not_overlap() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        git_status(&workspace, ["init", "--quiet"]).unwrap();
        git_status(&workspace, ["config", "user.name", "Fixture"]).unwrap();
        git_status(
            &workspace,
            ["config", "user.email", "fixture@example.invalid"],
        )
        .unwrap();
        fs::write(workspace.join("README.md"), "base\n").unwrap();
        git_status(&workspace, ["add", "."]).unwrap();
        git_status(&workspace, ["commit", "--quiet", "-m", "base"]).unwrap();

        let feature_id = FeatureId::new();
        let packages_root = root.path().join("packages");
        let mut base = git_text(&workspace, ["rev-parse", "HEAD"]).unwrap();
        let original_base = base.clone();
        let mut parent = None;
        let mut captured = Vec::new();

        // Three units captured in order from one workspace, as an agent would build them.
        for unit in 1..=3 {
            fs::write(
                workspace.join(format!("unit-{unit}.txt")),
                format!("unit {unit}\n"),
            )
            .unwrap();
            let package = SnapshotPackage::capture(SnapshotRequest {
                package_id: PackageId::new(),
                revision: Revision::FIRST,
                feature_id,
                plan_revision: Revision::FIRST,
                task_ids: [TaskId::new()].into(),
                workspace: &workspace,
                package_root: &packages_root,
                expected_base_commit: &base,
                parent_package_id: parent,
                validation_policy: ContentValidationPolicy::default(),
            })
            .unwrap();
            base = package.advanced_base_commit().to_owned();
            parent = Some(package.manifest.package_id);
            captured.push(package);
        }

        // Each unit is chained to the one before it, and only the first starts from the original
        // workspace base.
        assert_eq!(captured[0].manifest.parent_package_id, None);
        assert_eq!(captured[0].manifest.base_commit, original_base);
        for unit in 1..3 {
            assert_eq!(
                captured[unit].manifest.parent_package_id,
                Some(captured[unit - 1].manifest.package_id)
            );
            assert_eq!(
                captured[unit].manifest.base_tree,
                captured[unit - 1].manifest.result_tree,
                "unit {} must start from the previous unit's result",
                unit + 1
            );
        }

        // The defect this guards: each unit's own change is exactly one file, not a cumulative
        // snapshot that silently carries every earlier unit's work.
        for (index, package) in captured.iter().enumerate() {
            let changed = git_text(
                &workspace,
                [
                    "diff",
                    "--name-only",
                    &package.manifest.base_tree,
                    &package.manifest.result_tree,
                ],
            )
            .unwrap();
            assert_eq!(
                changed,
                format!("unit-{}.txt", index + 1),
                "unit {} changed more than its own file",
                index + 1
            );
        }

        // The workspace is left consistent: HEAD is the final unit and nothing is uncommitted.
        assert_eq!(
            git_text(&workspace, ["rev-parse", "HEAD"]).unwrap(),
            captured[2].manifest.snapshot_commit
        );
        assert_eq!(
            git_text(&workspace, ["status", "--porcelain=v1"]).unwrap(),
            ""
        );
    }

    #[test]
    fn package_reconstructs_all_git_file_kinds_and_detects_corruption() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        git_status(&workspace, ["init", "--quiet"]).unwrap();
        git_status(&workspace, ["config", "user.name", "Fixture"]).unwrap();
        git_status(
            &workspace,
            ["config", "user.email", "fixture@example.invalid"],
        )
        .unwrap();
        fs::write(workspace.join("delete.txt"), "remove\n").unwrap();
        fs::write(workspace.join("edit.txt"), "before\n").unwrap();
        git_status(&workspace, ["add", "."]).unwrap();
        git_status(&workspace, ["commit", "--quiet", "-m", "base"]).unwrap();
        let base = git_text(&workspace, ["rev-parse", "HEAD"]).unwrap();

        fs::remove_file(workspace.join("delete.txt")).unwrap();
        fs::write(workspace.join("edit.txt"), "after\n").unwrap();
        fs::write(workspace.join("binary.bin"), [0, 159, 146, 150]).unwrap();
        fs::write(workspace.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(workspace.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("run.sh", workspace.join("run-link")).unwrap();
        let package = SnapshotPackage::capture(SnapshotRequest {
            package_id: PackageId::new(),
            revision: Revision::FIRST,
            feature_id: FeatureId::new(),
            plan_revision: Revision::FIRST,
            task_ids: [TaskId::new()].into(),
            workspace: &workspace,
            package_root: &root.path().join("packages"),
            expected_base_commit: &base,
            parent_package_id: None,
            validation_policy: ContentValidationPolicy::default(),
        })
        .unwrap();
        let reopened = SnapshotPackage::open(package.path.clone()).unwrap();
        assert_eq!(reopened.manifest.result_tree, package.manifest.result_tree);

        let copied = reopened
            .copy_verified_to(&root.path().join("copied-package"))
            .unwrap();
        assert_eq!(copied.manifest, package.manifest);
        copied
            .reconstruct(&root.path().join("copied-restored"))
            .unwrap();

        let restored = root.path().join("restored");
        reopened.reconstruct(&restored).unwrap();
        assert!(!restored.join("delete.txt").exists());
        assert_eq!(
            fs::read_to_string(restored.join("edit.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read(restored.join("binary.bin")).unwrap(),
            [0, 159, 146, 150]
        );
        assert_ne!(
            fs::metadata(restored.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            fs::read_link(restored.join("run-link")).unwrap(),
            Path::new("run.sh")
        );

        let mut bundle = OpenOptions::new()
            .append(true)
            .open(package.path.join(BUNDLE_FILE))
            .unwrap();
        bundle.write_all(b"corrupt").unwrap();
        let next_base = package.advanced_base_commit().to_owned();
        let previous_unit = package.manifest.package_id;
        assert!(matches!(
            SnapshotPackage::open(package.path),
            Err(SnapshotError::HashMismatch {
                component: "bundle"
            })
        ));

        fs::write(
            workspace.join(".env"),
            "API_KEY=sk-this-must-not-appear-in-errors-123456\n",
        )
        .unwrap();
        let error = SnapshotPackage::capture(SnapshotRequest {
            package_id: PackageId::new(),
            revision: Revision::FIRST,
            feature_id: FeatureId::new(),
            plan_revision: Revision::FIRST,
            task_ids: [TaskId::new()].into(),
            workspace: &workspace,
            package_root: &root.path().join("packages"),
            // The earlier capture advanced the workspace, so this unit starts from its result.
            expected_base_commit: &next_base,
            parent_package_id: Some(previous_unit),
            validation_policy: ContentValidationPolicy::default(),
        })
        .unwrap_err();
        assert!(matches!(error, SnapshotError::ContentBlocked { .. }));
        assert!(!error.to_string().contains("sk-this-must-not-appear"));
    }
}
