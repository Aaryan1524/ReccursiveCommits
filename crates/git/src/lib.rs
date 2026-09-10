//! Single hardened boundary for all Git subprocess execution.

use std::{
    ffi::OsString,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;
use wait_timeout::ChildExt;

use reccursive_capture::{SnapshotError, SnapshotPackage};

pub const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct GitInvocation {
    pub working_directory: PathBuf,
    pub arguments: Vec<OsString>,
    pub timeout: Duration,
}

impl GitInvocation {
    pub fn new(
        working_directory: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
        timeout: Duration,
    ) -> Result<Self, GitError> {
        let value = Self {
            working_directory: working_directory.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
            timeout,
        };
        if !value.working_directory.is_absolute()
            || value.arguments.is_empty()
            || value.timeout.is_zero()
        {
            return Err(GitError::InvalidInvocation);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git invocation requires an absolute directory, arguments, and a nonzero timeout")]
    InvalidInvocation,
    #[error("Git could not start: {0}")]
    Io(#[from] std::io::Error),
    #[error("Git operation timed out after {0:?}")]
    TimedOut(Duration),
    #[error("Git operation was cancelled")]
    Cancelled,
    #[error("Git failed ({status:?}): {message}")]
    Failed {
        status: Option<i32>,
        message: String,
    },
    #[error("managed clone path already exists: {}", .0.display())]
    ManagedPathExists(PathBuf),
    #[error("snapshot package could not be verified: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("candidate snapshot {component} differs from its authenticated manifest")]
    CandidateSnapshotMismatch { component: &'static str },
    #[error("Git returned non-UTF-8 output where a path was required")]
    NonUtf8Output,
}

pub struct GitRunner;
impl GitRunner {
    pub fn run(
        invocation: &GitInvocation,
        cancellation: &CancellationToken,
    ) -> Result<GitOutput, GitError> {
        let mut command = Command::new("git");
        command
            .current_dir(&invocation.working_directory)
            .args(&invocation.arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_reader = thread::spawn(move || read_bounded(stdout));
        let err_reader = thread::spawn(move || read_bounded(stderr));
        let deadline = Instant::now() + invocation.timeout;
        let status = loop {
            if cancellation.cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::TimedOut(invocation.timeout));
            }
            if let Some(status) = child.wait_timeout(remaining.min(Duration::from_millis(50)))? {
                break status;
            }
        };
        let (stdout, out_truncated) = out_reader.join().map_err(|_| GitError::Failed {
            status: None,
            message: "stdout collector panicked".into(),
        })?;
        let (stderr, err_truncated) = err_reader.join().map_err(|_| GitError::Failed {
            status: None,
            message: "stderr collector panicked".into(),
        })?;
        let output = GitOutput {
            stdout,
            stderr,
            truncated: out_truncated || err_truncated,
        };
        if status.success() {
            Ok(output)
        } else {
            Err(GitError::Failed {
                status: status.code(),
                message: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(512)
                    .collect(),
            })
        }
    }
}

/// Daemon-owned mirror used for release work, never a user's checkout.
#[derive(Clone, Debug)]
pub struct ManagedClone {
    pub path: PathBuf,
}

impl ManagedClone {
    pub fn provision(
        remote: &str,
        path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, GitError> {
        let path = path.into();
        if path.exists() {
            return Err(GitError::ManagedPathExists(path));
        }
        let parent = path.parent().ok_or(GitError::InvalidInvocation)?;
        let name = path.file_name().ok_or(GitError::InvalidInvocation)?;
        GitRunner::run(
            &GitInvocation::new(
                parent,
                [
                    OsString::from("clone"),
                    OsString::from("--mirror"),
                    OsString::from(remote),
                    name.to_os_string(),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        Ok(Self { path })
    }

    pub fn fetch_ref(
        &self,
        remote: &str,
        reference: &str,
        timeout: Duration,
    ) -> Result<GitOutput, GitError> {
        GitRunner::run(
            &GitInvocation::new(
                &self.path,
                ["fetch", "--no-tags", remote, reference],
                timeout,
            )?,
            &CancellationToken::default(),
        )
    }

    pub fn create_release_workspace(
        &self,
        destination: impl Into<PathBuf>,
        commit: &str,
        timeout: Duration,
    ) -> Result<PathBuf, GitError> {
        let destination = destination.into();
        if destination.exists() {
            return Err(GitError::ManagedPathExists(destination));
        }
        let parent = destination.parent().ok_or(GitError::InvalidInvocation)?;
        let name = destination.file_name().ok_or(GitError::InvalidInvocation)?;
        GitRunner::run(
            &GitInvocation::new(
                parent,
                [
                    OsString::from("clone"),
                    OsString::from("--no-checkout"),
                    self.path.as_os_str().to_os_string(),
                    name.to_os_string(),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        GitRunner::run(
            &GitInvocation::new(&destination, ["checkout", "--detach", commit], timeout)?,
            &CancellationToken::default(),
        )?;
        Ok(destination)
    }

    /// Creates a detached release workspace at the current target and imports one verified
    /// snapshot under a daemon-owned ref. No change is applied until `apply_snapshot` is called.
    pub fn prepare_candidate_workspace(
        &self,
        destination: impl Into<PathBuf>,
        target_commit: &str,
        snapshot: &SnapshotPackage,
        timeout: Duration,
    ) -> Result<CandidateWorkspace, GitError> {
        let snapshot = SnapshotPackage::open(snapshot.path.clone())?;
        let path = self.create_release_workspace(destination, target_commit, timeout)?;
        let snapshot_ref = format!(
            "refs/reccursive/snapshots/{}/{}",
            snapshot.manifest.package_id,
            snapshot.manifest.revision.get()
        );
        let imported_ref = format!(
            "refs/reccursive/candidates/{}/{}",
            snapshot.manifest.package_id,
            snapshot.manifest.revision.get()
        );
        GitRunner::run(
            &GitInvocation::new(
                &path,
                vec![
                    OsString::from("fetch"),
                    OsString::from("--no-tags"),
                    snapshot.path.join("objects.bundle").into_os_string(),
                    OsString::from(format!("{snapshot_ref}:{imported_ref}")),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        if git_text(&path, ["rev-parse", imported_ref.as_str()], timeout)?
            != snapshot.manifest.snapshot_commit
        {
            return Err(GitError::CandidateSnapshotMismatch {
                component: "commit",
            });
        }
        let snapshot_tree = format!("{imported_ref}^{{tree}}");
        if git_text(&path, ["rev-parse", snapshot_tree.as_str()], timeout)?
            != snapshot.manifest.result_tree
        {
            return Err(GitError::CandidateSnapshotMismatch { component: "tree" });
        }
        let snapshot_parent = format!("{imported_ref}^");
        if git_text(&path, ["rev-parse", snapshot_parent.as_str()], timeout)?
            != snapshot.manifest.base_commit
        {
            return Err(GitError::CandidateSnapshotMismatch {
                component: "base commit",
            });
        }
        let resolved_target = git_text(&path, ["rev-parse", "HEAD"], timeout)?;
        Ok(CandidateWorkspace {
            path,
            target_commit: resolved_target,
            snapshot_commit: snapshot.manifest.snapshot_commit,
            snapshot_ref: imported_ref,
        })
    }
}

/// A temporary, detached release workspace. Applying the snapshot changes its index and worktree
/// but does not create a commit, allowing conflict and dependency checks to run first.
#[derive(Clone, Debug)]
pub struct CandidateWorkspace {
    pub path: PathBuf,
    pub target_commit: String,
    pub snapshot_commit: String,
    snapshot_ref: String,
}

/// Result of attempting to apply a snapshot to a candidate workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateApplyOutcome {
    /// The captured delta is staged and ready for verification; no commit was created.
    Applied { staged_tree: String },
    /// Git found unmerged paths. The workspace remains available for later reconciliation.
    Conflicted { paths: Vec<String> },
}

/// Evidence that a candidate workspace is safe to turn into a release commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateVerification {
    pub target_commit: String,
    pub staged_tree: String,
}

/// A condition preventing a candidate from becoming a release commit.
#[derive(Debug, Error)]
pub enum CandidateVerificationError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("candidate target changed from {expected} to {actual}")]
    TargetChanged { expected: String, actual: String },
    #[error("candidate has unresolved paths: {paths:?}")]
    UnmergedPaths { paths: Vec<String> },
}

/// Explicit identity and message for a local release candidate commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateCommitRequest {
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

/// Immutable identity of a candidate commit that exists only in its managed workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedCandidate {
    pub commit: String,
    pub parent_commit: String,
    pub tree: String,
}

/// A condition preventing a verified candidate from being persisted as a commit.
#[derive(Debug, Error)]
pub enum CandidateCommitError {
    #[error(transparent)]
    Verification(#[from] CandidateVerificationError),
    #[error("candidate commit message must contain non-whitespace text and no NUL bytes")]
    InvalidMessage,
    #[error("candidate commit {field} is invalid")]
    InvalidIdentity { field: &'static str },
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("persisted candidate parent differs from verified target")]
    ParentMismatch,
    #[error("persisted candidate tree differs from verified staged tree")]
    TreeMismatch,
}

impl CandidateWorkspace {
    /// Applies the captured delta to the current target without committing it.
    ///
    /// If Git reports a conflict, the workspace is intentionally retained for reconciliation.
    pub fn apply_snapshot(&self, timeout: Duration) -> Result<CandidateApplyOutcome, GitError> {
        match GitRunner::run(
            &GitInvocation::new(
                &self.path,
                ["cherry-pick", "--no-commit", self.snapshot_ref.as_str()],
                timeout,
            )?,
            &CancellationToken::default(),
        ) {
            Ok(_) => Ok(CandidateApplyOutcome::Applied {
                staged_tree: self.staged_tree(timeout)?,
            }),
            Err(error) => {
                let paths = self.unmerged_paths(timeout)?;
                if paths.is_empty() {
                    Err(error)
                } else {
                    Ok(CandidateApplyOutcome::Conflicted { paths })
                }
            }
        }
    }

    /// Returns the candidate tree currently staged in this workspace.
    pub fn staged_tree(&self, timeout: Duration) -> Result<String, GitError> {
        git_text(&self.path, ["write-tree"], timeout)
    }

    /// Verifies the staged candidate before a release commit may be created.
    ///
    /// This verifies local Git invariants only. Repository-specific trusted checks are run by the
    /// daemon release service, where their configured commands and durable evidence are owned.
    pub fn verify(
        &self,
        timeout: Duration,
    ) -> Result<CandidateVerification, CandidateVerificationError> {
        let actual_target = git_text(&self.path, ["rev-parse", "HEAD"], timeout)?;
        if actual_target != self.target_commit {
            return Err(CandidateVerificationError::TargetChanged {
                expected: self.target_commit.clone(),
                actual: actual_target,
            });
        }
        let paths = self.unmerged_paths(timeout)?;
        if !paths.is_empty() {
            return Err(CandidateVerificationError::UnmergedPaths { paths });
        }
        GitRunner::run(
            &GitInvocation::new(&self.path, ["diff", "--cached", "--check"], timeout)?,
            &CancellationToken::default(),
        )?;
        Ok(CandidateVerification {
            target_commit: self.target_commit.clone(),
            staged_tree: self.staged_tree(timeout)?,
        })
    }

    /// Creates a local commit from a verified candidate. This never updates a remote ref.
    pub fn persist(
        &self,
        request: &CandidateCommitRequest,
        timeout: Duration,
    ) -> Result<PersistedCandidate, CandidateCommitError> {
        validate_commit_request(request)?;
        let verified = self.verify(timeout)?;
        GitRunner::run(
            &GitInvocation::new(
                &self.path,
                vec![
                    OsString::from("-c"),
                    OsString::from(format!("user.name={}", request.author_name)),
                    OsString::from("-c"),
                    OsString::from(format!("user.email={}", request.author_email)),
                    OsString::from("commit"),
                    OsString::from("--no-verify"),
                    OsString::from("--no-gpg-sign"),
                    OsString::from("-m"),
                    OsString::from(&request.message),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        let commit = git_text(&self.path, ["rev-parse", "HEAD"], timeout)?;
        let parent_commit = git_text(&self.path, ["rev-parse", "HEAD^"], timeout)?;
        let tree = git_text(&self.path, ["rev-parse", "HEAD^{tree}"], timeout)?;
        if parent_commit != verified.target_commit {
            return Err(CandidateCommitError::ParentMismatch);
        }
        if tree != verified.staged_tree {
            return Err(CandidateCommitError::TreeMismatch);
        }
        Ok(PersistedCandidate {
            commit,
            parent_commit,
            tree,
        })
    }

    fn unmerged_paths(&self, timeout: Duration) -> Result<Vec<String>, GitError> {
        let output = GitRunner::run(
            &GitInvocation::new(
                &self.path,
                ["diff", "--name-only", "--diff-filter=U", "-z"],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        let mut paths = output
            .stdout
            .split(|byte| *byte == b'\0')
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).map_err(|_| GitError::NonUtf8Output))
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn validate_commit_request(request: &CandidateCommitRequest) -> Result<(), CandidateCommitError> {
    if request.message.trim().is_empty() || request.message.contains('\0') {
        return Err(CandidateCommitError::InvalidMessage);
    }
    for (field, value) in [
        ("author name", &request.author_name),
        ("author email", &request.author_email),
    ] {
        if value.trim().is_empty()
            || value.trim() != value
            || value.contains(['\0', '\n', '\r'])
            || value.len() > 320
        {
            return Err(CandidateCommitError::InvalidIdentity { field });
        }
    }
    if !request.author_email.contains('@') {
        return Err(CandidateCommitError::InvalidIdentity {
            field: "author email",
        });
    }
    Ok(())
}

fn git_text(
    path: &std::path::Path,
    arguments: impl IntoIterator<Item = impl Into<OsString>>,
    timeout: Duration,
) -> Result<String, GitError> {
    let output = GitRunner::run(
        &GitInvocation::new(path, arguments, timeout)?,
        &CancellationToken::default(),
    )?;
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| GitError::NonUtf8Output)
}

fn read_bounded(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    let mut truncated = false;
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        let room = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(bytes.len());
        if room < count {
            bytes.extend_from_slice(&chunk[..room]);
            truncated = true;
        } else {
            bytes.extend_from_slice(&chunk[..count]);
        }
    }
    (bytes, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reccursive_capture::{ContentValidationPolicy, SnapshotRequest};
    use reccursive_core::{FeatureId, PackageId, Revision, TaskId};
    use tempfile::tempdir;

    fn fixture_git(path: &std::path::Path, arguments: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .status()
                .unwrap()
                .success(),
            "fixture Git command failed: {arguments:?}"
        );
    }
    #[test]
    fn argument_is_not_interpreted_by_a_shell() {
        let dir = tempdir().unwrap();
        let invocation =
            GitInvocation::new(dir.path(), ["--version; false"], Duration::from_secs(1)).unwrap();
        assert!(matches!(
            GitRunner::run(&invocation, &CancellationToken::default()),
            Err(GitError::Failed { .. })
        ));
    }
    #[test]
    fn cancellation_stops_a_process() {
        let dir = tempdir().unwrap();
        let token = CancellationToken::default();
        token.cancel();
        let invocation =
            GitInvocation::new(dir.path(), ["status"], Duration::from_secs(1)).unwrap();
        assert!(matches!(
            GitRunner::run(&invocation, &token),
            Err(GitError::Cancelled)
        ));
    }
    #[test]
    fn managed_mirror_fetches_and_release_workspace_is_detached() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote.git");
        std::process::Command::new("git")
            .args(["init", "--quiet", source.to_str().unwrap()])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                source.to_str().unwrap(),
                "config",
                "user.name",
                "Fixture",
            ])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                source.to_str().unwrap(),
                "config",
                "user.email",
                "fixture@example.invalid",
            ])
            .status()
            .unwrap();
        std::fs::write(source.join("file"), "ok").unwrap();
        std::process::Command::new("git")
            .args(["-C", source.to_str().unwrap(), "add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                source.to_str().unwrap(),
                "commit",
                "--quiet",
                "-m",
                "base",
            ])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "clone",
                "--quiet",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        let mirror = ManagedClone::provision(
            remote.to_str().unwrap(),
            root.path().join("mirror.git"),
            Duration::from_secs(5),
        )
        .unwrap();
        mirror
            .fetch_ref("origin", "HEAD", Duration::from_secs(5))
            .unwrap();
        let head = String::from_utf8(
            GitRunner::run(
                &GitInvocation::new(&source, ["rev-parse", "HEAD"], Duration::from_secs(5))
                    .unwrap(),
                &CancellationToken::default(),
            )
            .unwrap()
            .stdout,
        )
        .unwrap();
        let workspace = mirror
            .create_release_workspace(
                root.path().join("release"),
                head.trim(),
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(workspace.join("file").exists());
    }

    #[test]
    fn candidate_applies_a_verified_snapshot_to_the_latest_target_without_committing() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote.git");
        assert!(
            std::process::Command::new("git")
                .args([
                    "init",
                    "--quiet",
                    "--initial-branch=main",
                    source.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
        fixture_git(&source, &["config", "user.name", "Fixture"]);
        fixture_git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(source.join("file.txt"), "base\n").unwrap();
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "--quiet", "-m", "base"]);
        let base = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();

        std::fs::write(source.join("file.txt"), "captured change\n").unwrap();
        let snapshot = SnapshotPackage::capture(SnapshotRequest {
            package_id: PackageId::new(),
            revision: Revision::FIRST,
            feature_id: FeatureId::new(),
            plan_revision: Revision::FIRST,
            task_ids: [TaskId::new()].into(),
            workspace: &source,
            package_root: &root.path().join("packages"),
            expected_base_commit: &base,
            validation_policy: ContentValidationPolicy::default(),
        })
        .unwrap();

        std::fs::write(source.join("file.txt"), "base\n").unwrap();
        std::fs::write(source.join("target-only.txt"), "new target work\n").unwrap();
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "--quiet", "-m", "advance target"]);
        assert!(
            std::process::Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    "--bare",
                    source.to_str().unwrap(),
                    remote.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
        let target = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
        let target_tree = git_text(
            &source,
            ["rev-parse", "HEAD^{tree}"],
            Duration::from_secs(5),
        )
        .unwrap();

        let mirror = ManagedClone::provision(
            remote.to_str().unwrap(),
            root.path().join("mirror.git"),
            Duration::from_secs(5),
        )
        .unwrap();
        let candidate = mirror
            .prepare_candidate_workspace(
                root.path().join("candidate"),
                &target,
                &snapshot,
                Duration::from_secs(5),
            )
            .unwrap();
        let outcome = candidate.apply_snapshot(Duration::from_secs(5)).unwrap();

        assert_eq!(candidate.target_commit, target);
        assert_eq!(candidate.snapshot_commit, snapshot.manifest.snapshot_commit);
        assert_eq!(
            std::fs::read_to_string(candidate.path.join("file.txt")).unwrap(),
            "captured change\n"
        );
        assert_eq!(
            std::fs::read_to_string(candidate.path.join("target-only.txt")).unwrap(),
            "new target work\n"
        );
        assert_ne!(
            candidate.staged_tree(Duration::from_secs(5)).unwrap(),
            target_tree
        );
        let CandidateApplyOutcome::Applied { staged_tree } = outcome else {
            panic!("expected a clean candidate")
        };
        assert_eq!(
            candidate.verify(Duration::from_secs(5)).unwrap(),
            CandidateVerification {
                target_commit: target.clone(),
                staged_tree: staged_tree.clone(),
            }
        );
        assert_eq!(
            git_text(
                &candidate.path,
                ["rev-parse", "HEAD"],
                Duration::from_secs(5)
            )
            .unwrap(),
            target
        );
        let persisted = candidate
            .persist(
                &CandidateCommitRequest {
                    message: "Apply captured change".into(),
                    author_name: "Fixture Author".into(),
                    author_email: "fixture.author@example.invalid".into(),
                },
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(persisted.parent_commit, target);
        assert_eq!(persisted.tree, staged_tree);
        assert_eq!(
            git_text(
                &candidate.path,
                ["log", "-1", "--format=%an <%ae>"],
                Duration::from_secs(5),
            )
            .unwrap(),
            "Fixture Author <fixture.author@example.invalid>"
        );
        assert_eq!(
            git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap(),
            target
        );
        fixture_git(&candidate.path, &["reset", "--hard", base.as_str()]);
        assert!(matches!(
            candidate.verify(Duration::from_secs(5)),
            Err(CandidateVerificationError::TargetChanged { expected, actual })
                if expected == target && actual == base
        ));
    }

    #[test]
    fn candidate_commit_request_rejects_invalid_identity_and_message() {
        let valid = CandidateCommitRequest {
            message: "Apply change".into(),
            author_name: "Fixture Author".into(),
            author_email: "fixture@example.invalid".into(),
        };
        assert!(validate_commit_request(&valid).is_ok());
        assert!(matches!(
            validate_commit_request(&CandidateCommitRequest {
                message: " \n".into(),
                ..valid.clone()
            }),
            Err(CandidateCommitError::InvalidMessage)
        ));
        assert!(matches!(
            validate_commit_request(&CandidateCommitRequest {
                author_name: "Fixture\nAuthor".into(),
                ..valid.clone()
            }),
            Err(CandidateCommitError::InvalidIdentity {
                field: "author name"
            })
        ));
        assert!(matches!(
            validate_commit_request(&CandidateCommitRequest {
                author_email: "fixture.example.invalid".into(),
                ..valid
            }),
            Err(CandidateCommitError::InvalidIdentity {
                field: "author email"
            })
        ));
    }

    #[test]
    fn candidate_reports_unmerged_paths_and_retains_the_workspace() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote.git");
        assert!(
            std::process::Command::new("git")
                .args([
                    "init",
                    "--quiet",
                    "--initial-branch=main",
                    source.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
        fixture_git(&source, &["config", "user.name", "Fixture"]);
        fixture_git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(source.join("file.txt"), "base\n").unwrap();
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "--quiet", "-m", "base"]);
        let base = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();

        std::fs::write(source.join("file.txt"), "captured change\n").unwrap();
        let snapshot = SnapshotPackage::capture(SnapshotRequest {
            package_id: PackageId::new(),
            revision: Revision::FIRST,
            feature_id: FeatureId::new(),
            plan_revision: Revision::FIRST,
            task_ids: [TaskId::new()].into(),
            workspace: &source,
            package_root: &root.path().join("packages"),
            expected_base_commit: &base,
            validation_policy: ContentValidationPolicy::default(),
        })
        .unwrap();

        std::fs::write(source.join("file.txt"), "target change\n").unwrap();
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "--quiet", "-m", "advance target"]);
        assert!(
            std::process::Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    "--bare",
                    source.to_str().unwrap(),
                    remote.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
        let target = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
        let mirror = ManagedClone::provision(
            remote.to_str().unwrap(),
            root.path().join("mirror.git"),
            Duration::from_secs(5),
        )
        .unwrap();
        let candidate = mirror
            .prepare_candidate_workspace(
                root.path().join("candidate"),
                &target,
                &snapshot,
                Duration::from_secs(5),
            )
            .unwrap();

        assert_eq!(
            candidate.apply_snapshot(Duration::from_secs(5)).unwrap(),
            CandidateApplyOutcome::Conflicted {
                paths: vec!["file.txt".into()]
            }
        );
        assert!(candidate.path.join(".git").exists());
        assert!(candidate.path.join("file.txt").exists());
        assert!(matches!(
            candidate.verify(Duration::from_secs(5)),
            Err(CandidateVerificationError::UnmergedPaths { paths }) if paths == ["file.txt"]
        ));
    }
}
