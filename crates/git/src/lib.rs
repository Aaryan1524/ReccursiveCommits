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

    /// Adopts a mirror that was provisioned earlier, without contacting the remote.
    ///
    /// Provisioning clones; a daemon that restarts must reuse the mirror it already owns rather
    /// than re-cloning a repository on every release.
    #[must_use]
    pub fn adopt(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves a reference in the mirror to the commit it points at.
    pub fn resolve(&self, reference: &str, timeout: Duration) -> Result<String, GitError> {
        git_text(&self.path, ["rev-parse", reference], timeout)
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

/// Explicit remote destination for publishing one persisted candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidatePublishRequest {
    pub remote: String,
    pub target_ref: String,
}

/// Evidence that a remote branch now points to the persisted candidate commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedCandidate {
    pub commit: String,
    pub target_ref: String,
}

/// A condition preventing a candidate from being safely published.
#[derive(Debug, Error)]
pub enum CandidatePublishError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("candidate publish remote is invalid")]
    InvalidRemote,
    #[error("candidate publish target must be a valid refs/heads/ branch")]
    InvalidTargetRef,
    #[error("persisted candidate does not belong to this candidate workspace")]
    LocalCandidateMismatch,
    #[error("remote target {target_ref} does not exist")]
    TargetMissing { target_ref: String },
    #[error("remote target {target_ref} advanced from expected {expected} to {actual}")]
    TargetAdvanced {
        target_ref: String,
        expected: String,
        actual: String,
    },
    #[error("remote target {target_ref} did not confirm candidate {expected}; found {actual:?}")]
    ConfirmationFailed {
        target_ref: String,
        expected: String,
        actual: Option<String>,
    },
}

/// Read-only classification used after a publish attempt has an uncertain result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationRecovery {
    /// The remote already contains the candidate; no retry is needed.
    Published(PublishedCandidate),
    /// The remote remains at the verified target, so a leased retry is safe.
    PendingRetry {
        target_ref: String,
        target_commit: String,
    },
    /// Another commit is present. Automatic retry is unsafe and must stop.
    Ambiguous {
        target_ref: String,
        expected_target: String,
        actual_commit: String,
    },
}

/// Bounded retry configuration for publication transport failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationRetryPolicy {
    pub max_attempts: u8,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for PublicationRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(5 * 60),
        }
    }
}

impl PublicationRetryPolicy {
    pub fn new(
        max_attempts: u8,
        initial_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self, RetryPolicyError> {
        if max_attempts == 0 || initial_delay.is_zero() || max_delay < initial_delay {
            return Err(RetryPolicyError::Invalid);
        }
        Ok(Self {
            max_attempts,
            initial_delay,
            max_delay,
        })
    }

    /// Selects the only safe next action after one unsuccessful publication attempt.
    /// `failed_attempts` includes the attempt that just failed.
    pub fn decide(
        &self,
        failed_attempts: u8,
        error: &CandidatePublishError,
    ) -> PublicationResolution {
        match classify_publication_error(error) {
            ErrorDisposition::Retryable => {
                if failed_attempts >= self.max_attempts {
                    PublicationResolution::Manual {
                        block: PublicationBlock::RetryExhausted,
                    }
                } else {
                    PublicationResolution::Retry {
                        after: self.delay_for(failed_attempts),
                    }
                }
            }
            ErrorDisposition::RecoverRemote => PublicationResolution::RecoverRemote,
            ErrorDisposition::Rebuild => PublicationResolution::RebuildCandidate,
            ErrorDisposition::Manual(block) => PublicationResolution::Manual { block },
        }
    }

    fn delay_for(&self, failed_attempts: u8) -> Duration {
        let multiplier = 1_u32
            .checked_shl(u32::from(failed_attempts.saturating_sub(1)))
            .unwrap_or(u32::MAX);
        self.initial_delay
            .checked_mul(multiplier)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }
}

/// Invalid bounded-retry configuration.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RetryPolicyError {
    #[error("retry policy needs attempts and a nonzero delay no larger than its maximum")]
    Invalid,
}

/// A next step which never silently chooses a merge, signing identity, or access workaround.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationResolution {
    Retry { after: Duration },
    RecoverRemote,
    RebuildCandidate,
    Manual { block: PublicationBlock },
}

/// User-visible categories for non-retryable publication blocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationBlock {
    Authentication,
    Signing,
    BranchRule,
    Conflict,
    Configuration,
    Cancelled,
    GitFailure,
    RetryExhausted,
}

enum ErrorDisposition {
    Retryable,
    RecoverRemote,
    Rebuild,
    Manual(PublicationBlock),
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

    /// Publishes a persisted candidate only when the remote target is still the verified parent.
    pub fn publish(
        &self,
        candidate: &PersistedCandidate,
        request: &CandidatePublishRequest,
        timeout: Duration,
    ) -> Result<PublishedCandidate, CandidatePublishError> {
        validate_publish_request(request)?;
        self.validate_persisted_candidate(candidate, timeout)?;
        match remote_ref(&self.path, &request.remote, &request.target_ref, timeout)? {
            Some(actual) if actual == self.target_commit => {}
            Some(actual) => {
                return Err(CandidatePublishError::TargetAdvanced {
                    target_ref: request.target_ref.clone(),
                    expected: self.target_commit.clone(),
                    actual,
                });
            }
            None => {
                return Err(CandidatePublishError::TargetMissing {
                    target_ref: request.target_ref.clone(),
                });
            }
        }
        GitRunner::run(
            &GitInvocation::new(
                &self.path,
                vec![
                    OsString::from("push"),
                    OsString::from("--porcelain"),
                    OsString::from(format!(
                        "--force-with-lease={}:{}",
                        request.target_ref, self.target_commit
                    )),
                    OsString::from(&request.remote),
                    OsString::from(format!("{}:{}", candidate.commit, request.target_ref)),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        let actual = remote_ref(&self.path, &request.remote, &request.target_ref, timeout)?;
        if actual.as_deref() != Some(candidate.commit.as_str()) {
            return Err(CandidatePublishError::ConfirmationFailed {
                target_ref: request.target_ref.clone(),
                expected: candidate.commit.clone(),
                actual,
            });
        }
        Ok(PublishedCandidate {
            commit: candidate.commit.clone(),
            target_ref: request.target_ref.clone(),
        })
    }

    /// Resolves an uncertain publish result without modifying the remote.
    pub fn recover_publication(
        &self,
        candidate: &PersistedCandidate,
        request: &CandidatePublishRequest,
        timeout: Duration,
    ) -> Result<PublicationRecovery, CandidatePublishError> {
        validate_publish_request(request)?;
        self.validate_persisted_candidate(candidate, timeout)?;
        match remote_ref(&self.path, &request.remote, &request.target_ref, timeout)? {
            Some(actual) if actual == candidate.commit => {
                Ok(PublicationRecovery::Published(PublishedCandidate {
                    commit: candidate.commit.clone(),
                    target_ref: request.target_ref.clone(),
                }))
            }
            Some(actual) if actual == self.target_commit => Ok(PublicationRecovery::PendingRetry {
                target_ref: request.target_ref.clone(),
                target_commit: self.target_commit.clone(),
            }),
            Some(actual) => Ok(PublicationRecovery::Ambiguous {
                target_ref: request.target_ref.clone(),
                expected_target: self.target_commit.clone(),
                actual_commit: actual,
            }),
            None => Err(CandidatePublishError::TargetMissing {
                target_ref: request.target_ref.clone(),
            }),
        }
    }

    fn validate_persisted_candidate(
        &self,
        candidate: &PersistedCandidate,
        timeout: Duration,
    ) -> Result<(), CandidatePublishError> {
        let local_commit = git_text(&self.path, ["rev-parse", "HEAD"], timeout)?;
        let local_parent = git_text(&self.path, ["rev-parse", "HEAD^"], timeout)?;
        let local_tree = git_text(&self.path, ["rev-parse", "HEAD^{tree}"], timeout)?;
        if local_commit != candidate.commit
            || local_parent != candidate.parent_commit
            || local_parent != self.target_commit
            || local_tree != candidate.tree
        {
            return Err(CandidatePublishError::LocalCandidateMismatch);
        }
        Ok(())
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

fn validate_publish_request(
    request: &CandidatePublishRequest,
) -> Result<(), CandidatePublishError> {
    if request.remote.trim().is_empty() || request.remote.contains(['\0', '\n', '\r']) {
        return Err(CandidatePublishError::InvalidRemote);
    }
    let Some(branch) = request.target_ref.strip_prefix("refs/heads/") else {
        return Err(CandidatePublishError::InvalidTargetRef);
    };
    if branch.is_empty()
        || branch.ends_with(['/', '.'])
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch.split('/').any(|part| part.ends_with(".lock"))
        || branch
            .chars()
            .any(|character| character.is_whitespace() || "~^:?*[\\".contains(character))
    {
        return Err(CandidatePublishError::InvalidTargetRef);
    }
    Ok(())
}

fn remote_ref(
    workspace: &std::path::Path,
    remote: &str,
    target_ref: &str,
    timeout: Duration,
) -> Result<Option<String>, GitError> {
    let output = GitRunner::run(
        &GitInvocation::new(
            workspace,
            ["ls-remote", "--refs", remote, target_ref],
            timeout,
        )?,
        &CancellationToken::default(),
    )?;
    let output = String::from_utf8(output.stdout).map_err(|_| GitError::NonUtf8Output)?;
    let Some(line) = output.lines().next() else {
        return Ok(None);
    };
    let mut fields = line.split_whitespace();
    let commit = fields.next().filter(|value| is_object_id(value));
    let reference = fields.next();
    if fields.next().is_some() || reference != Some(target_ref) {
        return Err(GitError::Failed {
            status: None,
            message: "remote returned an invalid ref response".into(),
        });
    }
    commit.map(str::to_owned).map(Some).ok_or(GitError::Failed {
        status: None,
        message: "remote returned an invalid object ID".into(),
    })
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn classify_publication_error(error: &CandidatePublishError) -> ErrorDisposition {
    match error {
        CandidatePublishError::TargetAdvanced { .. } => ErrorDisposition::Rebuild,
        CandidatePublishError::ConfirmationFailed { .. } => ErrorDisposition::RecoverRemote,
        CandidatePublishError::InvalidRemote
        | CandidatePublishError::InvalidTargetRef
        | CandidatePublishError::LocalCandidateMismatch
        | CandidatePublishError::TargetMissing { .. } => {
            ErrorDisposition::Manual(PublicationBlock::Configuration)
        }
        CandidatePublishError::Git(error) => classify_git_failure(error),
    }
}

fn classify_git_failure(error: &GitError) -> ErrorDisposition {
    match error {
        GitError::TimedOut(_) => ErrorDisposition::Retryable,
        GitError::Cancelled => ErrorDisposition::Manual(PublicationBlock::Cancelled),
        GitError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::NetworkDown
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ) =>
        {
            ErrorDisposition::Retryable
        }
        GitError::Failed { message, .. } => {
            let message = message.to_ascii_lowercase();
            if [
                "authentication failed",
                "could not read username",
                "permission denied (publickey)",
                "terminal prompts disabled",
                "access denied",
            ]
            .iter()
            .any(|marker| message.contains(marker))
            {
                ErrorDisposition::Manual(PublicationBlock::Authentication)
            } else if ["gpg failed", "signing failed", "sign_and_send_pubkey"]
                .iter()
                .any(|marker| message.contains(marker))
            {
                ErrorDisposition::Manual(PublicationBlock::Signing)
            } else if [
                "protected branch",
                "required status checks",
                "hook declined",
                "gh006",
            ]
            .iter()
            .any(|marker| message.contains(marker))
            {
                ErrorDisposition::Manual(PublicationBlock::BranchRule)
            } else if message.contains("conflict") {
                ErrorDisposition::Manual(PublicationBlock::Conflict)
            } else if [
                "could not resolve host",
                "connection timed out",
                "connection reset",
                "network is unreachable",
                "temporary failure",
                "remote end hung up",
                "http 5",
            ]
            .iter()
            .any(|marker| message.contains(marker))
            {
                ErrorDisposition::Retryable
            } else {
                ErrorDisposition::Manual(PublicationBlock::GitFailure)
            }
        }
        GitError::InvalidInvocation
        | GitError::ManagedPathExists(_)
        | GitError::Snapshot(_)
        | GitError::CandidateSnapshotMismatch { .. }
        | GitError::NonUtf8Output
        | GitError::Io(_) => ErrorDisposition::Manual(PublicationBlock::GitFailure),
    }
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
            parent_package_id: None,
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
        let publish_request = CandidatePublishRequest {
            remote: remote.to_string_lossy().into_owned(),
            target_ref: "refs/heads/main".into(),
        };
        assert_eq!(
            candidate
                .recover_publication(&persisted, &publish_request, Duration::from_secs(5))
                .unwrap(),
            PublicationRecovery::PendingRetry {
                target_ref: "refs/heads/main".into(),
                target_commit: target.clone(),
            }
        );
        assert_eq!(
            candidate
                .publish(&persisted, &publish_request, Duration::from_secs(5))
                .unwrap(),
            PublishedCandidate {
                commit: persisted.commit.clone(),
                target_ref: "refs/heads/main".into(),
            }
        );
        assert_eq!(
            remote_ref(
                &candidate.path,
                &publish_request.remote,
                &publish_request.target_ref,
                Duration::from_secs(5),
            )
            .unwrap(),
            Some(persisted.commit.clone())
        );
        assert_eq!(
            candidate
                .recover_publication(&persisted, &publish_request, Duration::from_secs(5))
                .unwrap(),
            PublicationRecovery::Published(PublishedCandidate {
                commit: persisted.commit.clone(),
                target_ref: "refs/heads/main".into(),
            })
        );
        assert!(matches!(
            candidate.publish(&persisted, &publish_request, Duration::from_secs(5)),
            Err(CandidatePublishError::TargetAdvanced { expected, actual, .. })
                if expected == target && actual == persisted.commit
        ));
        std::fs::write(source.join("concurrent.txt"), "another publisher\n").unwrap();
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "--quiet", "-m", "concurrent publish"]);
        let concurrent = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
        fixture_git(
            &source,
            &["push", remote.to_str().unwrap(), "HEAD:refs/heads/race"],
        );
        fixture_git(
            &remote,
            &["update-ref", "refs/heads/main", concurrent.as_str()],
        );
        assert_eq!(
            candidate
                .recover_publication(&persisted, &publish_request, Duration::from_secs(5))
                .unwrap(),
            PublicationRecovery::Ambiguous {
                target_ref: "refs/heads/main".into(),
                expected_target: target.clone(),
                actual_commit: concurrent,
            }
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
    fn candidate_publish_request_rejects_unsafe_destinations() {
        assert!(matches!(
            validate_publish_request(&CandidatePublishRequest {
                remote: "\n".into(),
                target_ref: "refs/heads/main".into(),
            }),
            Err(CandidatePublishError::InvalidRemote)
        ));
        assert!(matches!(
            validate_publish_request(&CandidatePublishRequest {
                remote: "origin".into(),
                target_ref: "main".into(),
            }),
            Err(CandidatePublishError::InvalidTargetRef)
        ));
        assert!(matches!(
            validate_publish_request(&CandidatePublishRequest {
                remote: "origin".into(),
                target_ref: "refs/heads/../main".into(),
            }),
            Err(CandidatePublishError::InvalidTargetRef)
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
            parent_package_id: None,
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

    #[test]
    fn publication_retry_policy_only_retries_classified_transport_failures() {
        let policy =
            PublicationRetryPolicy::new(3, Duration::from_secs(5), Duration::from_secs(20))
                .unwrap();
        let timeout = CandidatePublishError::Git(GitError::TimedOut(Duration::from_secs(1)));
        assert_eq!(
            policy.decide(1, &timeout),
            PublicationResolution::Retry {
                after: Duration::from_secs(5)
            }
        );
        assert_eq!(
            policy.decide(2, &timeout),
            PublicationResolution::Retry {
                after: Duration::from_secs(10)
            }
        );
        assert_eq!(
            policy.decide(3, &timeout),
            PublicationResolution::Manual {
                block: PublicationBlock::RetryExhausted
            }
        );
        let authentication = CandidatePublishError::Git(GitError::Failed {
            status: Some(128),
            message: "fatal: Authentication failed".into(),
        });
        assert_eq!(
            policy.decide(1, &authentication),
            PublicationResolution::Manual {
                block: PublicationBlock::Authentication
            }
        );
        assert_eq!(
            policy.decide(
                1,
                &CandidatePublishError::TargetAdvanced {
                    target_ref: "refs/heads/main".into(),
                    expected: "a".repeat(40),
                    actual: "b".repeat(40),
                }
            ),
            PublicationResolution::RebuildCandidate
        );
        assert_eq!(
            policy.decide(
                1,
                &CandidatePublishError::ConfirmationFailed {
                    target_ref: "refs/heads/main".into(),
                    expected: "a".repeat(40),
                    actual: None,
                }
            ),
            PublicationResolution::RecoverRemote
        );
        assert!(matches!(
            PublicationRetryPolicy::new(0, Duration::from_secs(1), Duration::from_secs(1)),
            Err(RetryPolicyError::Invalid)
        ));
    }

    #[test]
    fn publication_fault_matrix_never_retries_an_ambiguous_remote_state() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote.git");
        let release = root.path().join("release");
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
        let target = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
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
        assert!(
            std::process::Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    remote.to_str().unwrap(),
                    release.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
        fixture_git(
            &release,
            &["checkout", "--quiet", "--detach", target.as_str()],
        );
        std::fs::write(release.join("file.txt"), "candidate\n").unwrap();
        fixture_git(&release, &["add", "."]);
        let candidate = CandidateWorkspace {
            path: release,
            target_commit: target.clone(),
            snapshot_commit: "a".repeat(40),
            snapshot_ref: "refs/reccursive/test".into(),
        };
        let request = CandidatePublishRequest {
            remote: remote.to_string_lossy().into_owned(),
            target_ref: "refs/heads/main".into(),
        };

        // Fault before persistence: no candidate can have reached the remote.
        assert_eq!(
            remote_ref(
                &candidate.path,
                &request.remote,
                &request.target_ref,
                Duration::from_secs(5),
            )
            .unwrap(),
            Some(target.clone())
        );
        let persisted = candidate
            .persist(
                &CandidateCommitRequest {
                    message: "Persist candidate".into(),
                    author_name: "Fixture Author".into(),
                    author_email: "fixture.author@example.invalid".into(),
                },
                Duration::from_secs(5),
            )
            .unwrap();

        // Fault after persistence but before push: recovery permits a leased retry.
        assert_eq!(
            candidate
                .recover_publication(&persisted, &request, Duration::from_secs(5))
                .unwrap(),
            PublicationRecovery::PendingRetry {
                target_ref: request.target_ref.clone(),
                target_commit: target.clone(),
            }
        );

        // Fault after push but before local confirmation: remote reachability confirms success.
        candidate
            .publish(&persisted, &request, Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            candidate
                .recover_publication(&persisted, &request, Duration::from_secs(5))
                .unwrap(),
            PublicationRecovery::Published(PublishedCandidate {
                commit: persisted.commit.clone(),
                target_ref: request.target_ref.clone(),
            })
        );

        // A different remote commit is ambiguous, so recovery cannot authorize another push.
        std::fs::write(source.join("concurrent.txt"), "other writer\n").unwrap();
        fixture_git(&source, &["add", "."]);
        fixture_git(&source, &["commit", "--quiet", "-m", "concurrent writer"]);
        let concurrent = git_text(&source, ["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
        fixture_git(
            &source,
            &["push", remote.to_str().unwrap(), "HEAD:refs/heads/race"],
        );
        fixture_git(
            &remote,
            &["update-ref", "refs/heads/main", concurrent.as_str()],
        );
        assert_eq!(
            candidate
                .recover_publication(&persisted, &request, Duration::from_secs(5))
                .unwrap(),
            PublicationRecovery::Ambiguous {
                target_ref: request.target_ref,
                expected_target: target,
                actual_commit: concurrent,
            }
        );
    }
}
