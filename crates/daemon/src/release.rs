//! Daemon-owned release worker.
//!
//! This is the only place publishing Git operations run. `reccursive-git` provides the primitives;
//! this module decides when they run and makes every stage durable *before* the operation it names,
//! so a crash always leaves an attempt record that says what was in flight.
//!
//! Git and SQLite cannot share a transaction, which is why the order matters: the attempt reaches
//! `push_pending` and carries its candidate commit and push intent before the remote is contacted.
//! A process that dies mid-push therefore restarts knowing exactly which commit may already be on
//! the remote, and resolves that against the remote rather than building a second candidate.

use std::{path::PathBuf, time::Duration};

use reccursive_capture::SnapshotPackage;
use reccursive_git::{
    CandidateApplyOutcome, CandidateCommitRequest, CandidatePublishError, CandidatePublishRequest,
    CandidateVerificationError, CandidateWorkspace, GitError, ManagedClone, PersistedCandidate,
    PublicationRecovery, PublicationResolution, PublicationRetryPolicy,
};
use reccursive_protocol::{
    AttemptId, PackageId, ReasonCode, Revision, StateReason, TargetRef, TaskStatus,
};
use reccursive_store::{
    AttemptLease, NewReleaseAttempt, ReleaseAttempt, SnapshotRecord, Store, StoreError,
};

/// How long any single Git operation may run before it is treated as unresponsive.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a release lease is held before another owner may reclaim it.
const LEASE_DURATION_MS: i64 = 5 * 60 * 1000;

/// Everything the worker needs to publish one captured package.
#[derive(Clone, Debug)]
pub struct ReleaseRequest {
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

/// The observable result of one release attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    /// The remote target now contains the candidate commit.
    Published {
        attempt_id: AttemptId,
        commit: String,
    },
    /// The attempt stopped and needs a person. The reason is durable on the attempt record.
    Blocked {
        attempt_id: AttemptId,
        classification: String,
        detail: String,
    },
    /// A retryable transport failure was recorded for a scheduler to resume later.
    Deferred {
        attempt_id: AttemptId,
        retry_not_before_unix_ms: i64,
        detail: String,
    },
}

/// Result of reconciling all release attempts whose previous push may have reached a remote.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    pub resolved_attempt_ids: Vec<AttemptId>,
    pub failures: Vec<RecoveryFailure>,
}

/// One recovery failure that must remain visible without preventing other targets from resolving.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryFailure {
    pub attempt_id: AttemptId,
    pub detail: String,
}

/// A release could not be started at all, so no attempt state was changed.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Publish(#[from] CandidatePublishError),
    #[error("release storage is unavailable: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Unavailable(String),
}

/// Owns the managed clones and release workspaces used to publish captured work.
pub struct ReleaseWorker {
    managed_root: PathBuf,
    workspace_root: PathBuf,
    lease_owner: String,
    retry: PublicationRetryPolicy,
}

impl ReleaseWorker {
    #[must_use]
    pub fn new(
        managed_root: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
        lease_owner: impl Into<String>,
    ) -> Self {
        Self {
            managed_root: managed_root.into(),
            workspace_root: workspace_root.into(),
            lease_owner: lease_owner.into(),
            retry: PublicationRetryPolicy::default(),
        }
    }

    /// Publishes one captured package to its repository's configured target.
    pub fn release(
        &self,
        store: &mut Store,
        request: &ReleaseRequest,
        now_unix_ms: i64,
    ) -> Result<ReleaseOutcome, ReleaseError> {
        let snapshot = store
            .snapshot(request.package_id, request.package_revision)?
            .ok_or_else(|| {
                ReleaseError::Unavailable(format!("package {} is not stored", request.package_id))
            })?;
        // The package names its plan revision, and the plan names the repository whose policy
        // decides where this work is published.
        let plan = store
            .plan(snapshot.feature_id, Some(snapshot.plan_revision))?
            .ok_or_else(|| {
                ReleaseError::Unavailable("the package's plan revision is not stored".to_owned())
            })?;
        let repository = store.repository(plan.plan.repository_id)?.ok_or_else(|| {
            ReleaseError::Unavailable("the package's repository is no longer enrolled".to_owned())
        })?;

        let remote = repository.registration.canonical_remote.clone();
        let target = repository.active_policy.target.clone();

        // Nothing has touched Git yet. Open the attempt first so the target is claimed and any
        // crash from here on leaves a record explaining what was underway.
        let attempt = store.open_release_attempt(&NewReleaseAttempt {
            attempt_id: AttemptId::new(),
            repository_id: repository.registration.id,
            package_id: snapshot.package_id,
            package_revision: snapshot.revision,
            remote: remote.clone(),
            target: target.clone(),
            base_commit: snapshot_base_commit(&snapshot)?,
            lease: AttemptLease {
                owner: self.lease_owner.clone(),
                expires_at_unix_ms: now_unix_ms + LEASE_DURATION_MS,
            },
            created_at_unix_ms: now_unix_ms,
        })?;

        match self.run_attempt(
            store,
            &attempt,
            &snapshot,
            &remote,
            &target,
            request,
            now_unix_ms,
        ) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                // Any unexpected failure still leaves an explained, durable attempt.
                let detail = error.to_string();
                self.block(store, &attempt, "internal_error", &detail, now_unix_ms)?;
                Ok(ReleaseOutcome::Blocked {
                    attempt_id: attempt.attempt_id,
                    classification: "internal_error".to_owned(),
                    detail,
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_attempt(
        &self,
        store: &mut Store,
        attempt: &ReleaseAttempt,
        snapshot: &SnapshotRecord,
        remote: &str,
        target: &TargetRef,
        request: &ReleaseRequest,
        now_unix_ms: i64,
    ) -> Result<ReleaseOutcome, ReleaseError> {
        let id = attempt.attempt_id;

        // Reconcile against whatever the remote holds right now.
        store.advance_release_attempt(id, TaskStatus::Reconciling, None, now_unix_ms)?;
        let clone = self.managed_clone(remote, attempt.repository_id.to_string().as_str())?;
        clone.fetch_ref(remote, target.as_str(), GIT_TIMEOUT)?;
        let target_commit = clone.resolve(target.as_str(), GIT_TIMEOUT)?;

        let package = SnapshotPackage::open(snapshot.path.clone())
            .map_err(|error| ReleaseError::Unavailable(error.to_string()))?;
        // The mirror creates the workspace itself, but its parent must exist first.
        std::fs::create_dir_all(&self.workspace_root)?;
        let candidate_path = self.workspace_root.join(format!("release-{id}"));
        let workspace = clone.prepare_candidate_workspace(
            candidate_path,
            &target_commit,
            &package,
            GIT_TIMEOUT,
        )?;

        if let CandidateApplyOutcome::Conflicted { paths } =
            workspace.apply_snapshot(GIT_TIMEOUT)?
        {
            let detail = format!("unresolved paths: {}", paths.join(", "));
            self.block(store, attempt, "conflict", &detail, now_unix_ms)?;
            return Ok(ReleaseOutcome::Blocked {
                attempt_id: id,
                classification: "conflict".to_owned(),
                detail,
            });
        }

        // Verify the combined result before any commit exists.
        store.advance_release_attempt(id, TaskStatus::Verifying, None, now_unix_ms)?;
        if let Err(error) = workspace.verify(GIT_TIMEOUT) {
            let classification = match error {
                CandidateVerificationError::TargetChanged { .. } => "target_changed",
                CandidateVerificationError::UnmergedPaths { .. } => "conflict",
                CandidateVerificationError::Git(_) => "git_failure",
            };
            let detail = error.to_string();
            self.block(store, attempt, classification, &detail, now_unix_ms)?;
            return Ok(ReleaseOutcome::Blocked {
                attempt_id: id,
                classification: classification.to_owned(),
                detail,
            });
        }

        // Create the commit, then make its identity and the intent to push it durable before the
        // remote is contacted. This is the invariant the whole recovery path rests on.
        store.advance_release_attempt(id, TaskStatus::CommitPrepared, None, now_unix_ms)?;
        let candidate = workspace.persist(
            &CandidateCommitRequest {
                message: request.message.clone(),
                author_name: request.author_name.clone(),
                author_email: request.author_email.clone(),
            },
            GIT_TIMEOUT,
        );
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                let detail = error.to_string();
                self.block(store, attempt, "commit_failed", &detail, now_unix_ms)?;
                return Ok(ReleaseOutcome::Blocked {
                    attempt_id: id,
                    classification: "commit_failed".to_owned(),
                    detail,
                });
            }
        };
        store.record_push_intent(id, &candidate.commit, &candidate.parent_commit, now_unix_ms)?;

        self.publish_with_retry(
            store,
            attempt,
            &workspace,
            &candidate,
            remote,
            target,
            now_unix_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_with_retry(
        &self,
        store: &mut Store,
        attempt: &ReleaseAttempt,
        workspace: &CandidateWorkspace,
        candidate: &PersistedCandidate,
        remote: &str,
        target: &TargetRef,
        now_unix_ms: i64,
    ) -> Result<ReleaseOutcome, ReleaseError> {
        let id = attempt.attempt_id;
        let publish_request = CandidatePublishRequest {
            remote: remote.to_owned(),
            target_ref: target.as_str().to_owned(),
        };

        store.advance_release_attempt(id, TaskStatus::PushPending, None, now_unix_ms)?;
        match workspace.publish(candidate, &publish_request, GIT_TIMEOUT) {
            Ok(published) => {
                self.confirm_publication(store, attempt, &published.commit, now_unix_ms)?;
                Ok(ReleaseOutcome::Published {
                    attempt_id: id,
                    commit: published.commit,
                })
            }
            Err(error) => {
                let classification = classify(&error);
                let failures = attempt.failed_attempts.saturating_add(1);
                match self.retry.decide(failures, &error) {
                    // Delays are not slept here: the caller owns scheduling, and a worker that
                    // blocks a thread for minutes would hold its lease against everything else.
                    PublicationResolution::Retry { after } => {
                        let retry_not_before_unix_ms = retry_not_before(now_unix_ms, after)?;
                        store.defer_publication_retry(
                            id,
                            classification,
                            retry_not_before_unix_ms,
                            now_unix_ms,
                        )?;
                        Ok(ReleaseOutcome::Deferred {
                            attempt_id: id,
                            retry_not_before_unix_ms,
                            detail: format!("{classification}: {error}"),
                        })
                    }
                    PublicationResolution::RecoverRemote => self.resolve_against_remote(
                        store,
                        attempt,
                        workspace,
                        candidate,
                        &publish_request,
                        now_unix_ms,
                    ),
                    PublicationResolution::RebuildCandidate => {
                        let detail = format!("target moved during publication: {error}");
                        self.block(store, attempt, "target_changed", &detail, now_unix_ms)?;
                        Ok(ReleaseOutcome::Blocked {
                            attempt_id: id,
                            classification: "target_changed".to_owned(),
                            detail,
                        })
                    }
                    PublicationResolution::Manual { block } => {
                        let detail = format!("{block:?}: {error}");
                        let classification = format!("{block:?}").to_lowercase();
                        self.block(store, attempt, &classification, &detail, now_unix_ms)?;
                        Ok(ReleaseOutcome::Blocked {
                            attempt_id: id,
                            classification,
                            detail,
                        })
                    }
                }
            }
        }
    }

    /// Asks the remote what actually happened, rather than assuming a push failed.
    fn resolve_against_remote(
        &self,
        store: &mut Store,
        attempt: &ReleaseAttempt,
        workspace: &CandidateWorkspace,
        candidate: &PersistedCandidate,
        request: &CandidatePublishRequest,
        now_unix_ms: i64,
    ) -> Result<ReleaseOutcome, ReleaseError> {
        let id = attempt.attempt_id;
        match workspace.recover_publication(candidate, request, GIT_TIMEOUT)? {
            PublicationRecovery::Published(published) => {
                // The push landed after all. Recording it is what stops a duplicate commit.
                self.confirm_publication(store, attempt, &published.commit, now_unix_ms)?;
                Ok(ReleaseOutcome::Published {
                    attempt_id: id,
                    commit: published.commit,
                })
            }
            PublicationRecovery::PendingRetry { target_commit, .. } => {
                let detail = format!("remote is still at {target_commit}; the push never landed");
                self.block(store, attempt, "transport", &detail, now_unix_ms)?;
                Ok(ReleaseOutcome::Blocked {
                    attempt_id: id,
                    classification: "transport".to_owned(),
                    detail,
                })
            }
            PublicationRecovery::Ambiguous {
                expected_target,
                actual_commit,
                ..
            } => {
                let detail = format!(
                    "remote holds {actual_commit}, expected {expected_target}; another writer \
                     advanced the target"
                );
                self.block(store, attempt, "ambiguous_remote", &detail, now_unix_ms)?;
                Ok(ReleaseOutcome::Blocked {
                    attempt_id: id,
                    classification: "ambiguous_remote".to_owned(),
                    detail,
                })
            }
        }
    }

    /// Confirms that a candidate reached the remote and publishes the tasks it carries.
    fn confirm_publication(
        &self,
        store: &mut Store,
        attempt: &ReleaseAttempt,
        candidate_sha: &str,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let current = store
            .release_attempt(attempt.attempt_id)?
            .ok_or_else(|| StoreError::InvalidData("release attempt disappeared".into()))?;
        // A prior transport failure can have blocked the attempt at `push_pending`. Recovery is
        // allowed to resume precisely that state, never skip over it.
        if current.state.status() == TaskStatus::Blocked {
            store.advance_release_attempt(
                attempt.attempt_id,
                TaskStatus::PushPending,
                None,
                now_unix_ms,
            )?;
        }
        store.record_remote_observation(attempt.attempt_id, candidate_sha, now_unix_ms)?;
        store.advance_release_attempt(
            attempt.attempt_id,
            TaskStatus::RemoteConfirmed,
            None,
            now_unix_ms,
        )?;
        store.advance_release_attempt(
            attempt.attempt_id,
            TaskStatus::Published,
            None,
            now_unix_ms,
        )?;
        self.publish_tasks(store, attempt, now_unix_ms)
    }

    /// Marks the tasks a published package delivers as published.
    fn publish_tasks(
        &self,
        store: &mut Store,
        attempt: &ReleaseAttempt,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let Some(snapshot) = store.snapshot(attempt.package_id, attempt.package_revision)? else {
            return Ok(());
        };
        for task_id in store.package_task_ids(attempt.package_id, attempt.package_revision)? {
            store.advance_task_to(
                snapshot.feature_id,
                snapshot.plan_revision,
                task_id,
                TaskStatus::Published,
                now_unix_ms,
            )?;
        }
        Ok(())
    }

    /// Stops an attempt with a durable, machine-readable reason and blocks its tasks.
    fn block(
        &self,
        store: &mut Store,
        attempt: &ReleaseAttempt,
        classification: &str,
        detail: &str,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        // A terminal or already-blocked attempt keeps the state it has; this must never mask the
        // original explanation with a later one.
        let current = store
            .release_attempt(attempt.attempt_id)?
            .ok_or_else(|| StoreError::InvalidData("release attempt disappeared".into()))?;
        if current.state.status().is_terminal() || current.state.status() == TaskStatus::Blocked {
            return Ok(());
        }
        store.record_publication_failure(attempt.attempt_id, classification, now_unix_ms)?;
        let reason = StateReason::new(reason_code(classification), detail)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        store.advance_release_attempt(
            attempt.attempt_id,
            TaskStatus::Blocked,
            Some(reason.clone()),
            now_unix_ms,
        )?;

        let Some(snapshot) = store.snapshot(attempt.package_id, attempt.package_revision)? else {
            return Ok(());
        };
        for task_id in store.package_task_ids(attempt.package_id, attempt.package_revision)? {
            let Some(task) = store.task(snapshot.feature_id, snapshot.plan_revision, task_id)?
            else {
                continue;
            };
            if task.state.status().is_terminal() || task.state.status() == TaskStatus::Blocked {
                continue;
            }
            store.advance_task(
                snapshot.feature_id,
                snapshot.plan_revision,
                task_id,
                TaskStatus::Blocked,
                Some(reason.clone()),
                now_unix_ms,
            )?;
        }
        Ok(())
    }

    /// Opens the repository's managed mirror, provisioning it the first time it is needed.
    fn managed_clone(&self, remote: &str, name: &str) -> Result<ManagedClone, GitError> {
        let path = self.managed_root.join(format!("{name}.git"));
        if path.exists() {
            return Ok(ManagedClone::adopt(path));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        ManagedClone::provision(remote, path, GIT_TIMEOUT)
    }
}

/// Resolves attempts whose push may have reached the remote before the process stopped.
///
/// Invariant 6: this runs before any new candidate is built, so a successful-but-unrecorded push
/// is discovered rather than repeated.
pub fn recover_interrupted_releases(
    store: &mut Store,
    worker: &ReleaseWorker,
    now_unix_ms: i64,
) -> Result<RecoveryReport, StoreError> {
    let mut report = RecoveryReport::default();
    for attempt in store.release_attempts_awaiting_remote_resolution()? {
        match recover_interrupted_release(store, worker, &attempt, now_unix_ms) {
            Ok(()) => report.resolved_attempt_ids.push(attempt.attempt_id),
            Err(error) => report.failures.push(RecoveryFailure {
                attempt_id: attempt.attempt_id,
                detail: error.to_string(),
            }),
        }
    }
    Ok(report)
}

fn recover_interrupted_release(
    store: &mut Store,
    worker: &ReleaseWorker,
    attempt: &ReleaseAttempt,
    now_unix_ms: i64,
) -> Result<(), ReleaseError> {
    let candidate_sha = attempt.candidate_sha.as_deref().ok_or_else(|| {
        ReleaseError::Unavailable("transmitted push is missing its candidate commit".into())
    })?;
    let clone = worker.managed_clone(&attempt.remote, &attempt.repository_id.to_string())?;
    clone.fetch_ref(&attempt.remote, attempt.target.as_str(), GIT_TIMEOUT)?;
    let observed = clone.resolve(attempt.target.as_str(), GIT_TIMEOUT)?;

    if observed == candidate_sha {
        worker.confirm_publication(store, attempt, candidate_sha, now_unix_ms)?;
        return Ok(());
    }

    // A prior worker already classified this as a retryable transport failure. Startup must
    // inspect the remote, but it must not erase that durable backoff merely because the target
    // still has the candidate's parent.
    if attempt.candidate_parent_sha.as_deref() == Some(observed.as_str())
        && attempt.retry_not_before_unix_ms.is_some()
    {
        return Ok(());
    }

    let classification = if attempt.candidate_parent_sha.as_deref() == Some(observed.as_str()) {
        "transport"
    } else {
        "ambiguous_remote"
    };
    let detail = format!(
        "remote holds {observed}, not candidate {candidate_sha}; manual resolution is required"
    );
    worker.block(store, attempt, classification, &detail, now_unix_ms)?;
    Ok(())
}

fn retry_not_before(now_unix_ms: i64, delay: Duration) -> Result<i64, ReleaseError> {
    let delay_ms = i64::try_from(delay.as_millis())
        .map_err(|_| ReleaseError::Unavailable("publication retry delay is too large".into()))?;
    now_unix_ms
        .checked_add(delay_ms)
        .ok_or_else(|| ReleaseError::Unavailable("publication retry time overflows".into()))
}

fn snapshot_base_commit(snapshot: &SnapshotRecord) -> Result<String, ReleaseError> {
    snapshot
        .manifest
        .get("base_commit")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ReleaseError::Unavailable("package manifest does not record a base commit".to_owned())
        })
}

fn classify(error: &CandidatePublishError) -> &'static str {
    match error {
        CandidatePublishError::Git(_) => "transport",
        CandidatePublishError::TargetAdvanced { .. } => "target_changed",
        _ => "publication_failed",
    }
}

fn reason_code(classification: &str) -> ReasonCode {
    match classification {
        "conflict" | "target_changed" => ReasonCode::Conflict,
        "transport" | "ambiguous_remote" => ReasonCode::DeviceUnavailable,
        "authentication" => ReasonCode::AuthenticationRequired,
        other => ReasonCode::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, process::Command};

    use reccursive_capture::{ContentValidationPolicy, SnapshotRequest};
    use reccursive_protocol::{
        AcceptanceCheck, FeatureId, FeaturePlan, PLAN_SCHEMA_VERSION, PlanPhase, PlanTask,
        PublicationMode, RepositoryId, RepositoryPolicy, TaskId,
    };
    use reccursive_store::{RepositoryRegistration, SnapshotRecord, WorkspaceRecord};
    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _root: TempDir,
        store: Store,
        worker: ReleaseWorker,
        remote: PathBuf,
        repository_id: RepositoryId,
        package_id: PackageId,
        feature_id: FeatureId,
        task_id: TaskId,
    }

    fn git(dir: &std::path::Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(arguments)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Builds a real bare remote, a captured package, and the store rows that link them.
    fn fixture() -> Fixture {
        let root = TempDir::new().unwrap();
        let base = root.path();
        let remote = base.join("remote.git");
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        git(
            base,
            &[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                "remote.git",
            ],
        );
        git(
            base,
            &["init", "--quiet", "--initial-branch=main", "source"],
        );
        git(&source, &["config", "user.name", "Fixture"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(source.join("README.md"), "base\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "--quiet", "-m", "base"]);
        git(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&source, &["push", "--quiet", "-u", "origin", "main"]);

        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    source.to_string_lossy(),
                    remote.to_string_lossy(),
                    base.join("managed").to_string_lossy(),
                    1,
                )
                .unwrap(),
                &RepositoryPolicy::new(
                    repository_id,
                    Revision::FIRST,
                    PublicationMode::ScheduledCreation,
                    target,
                    None,
                )
                .unwrap(),
            )
            .unwrap();

        let feature_id = FeatureId::new();
        let task_id = TaskId::new();
        store
            .import_plan(
                &FeaturePlan {
                    schema_version: PLAN_SCHEMA_VERSION,
                    feature_id,
                    revision: Revision::FIRST,
                    repository_id,
                    goal: "Publish one unit".into(),
                    target: TargetRef::new("refs/heads/main").unwrap(),
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: vec![PlanTask {
                            id: task_id,
                            name: "Add a feature file".into(),
                            dependencies: BTreeMap::new(),
                            acceptance_checks: vec![AcceptanceCheck {
                                id: "present".into(),
                                description: "the file exists".into(),
                            }],
                        }],
                    }],
                },
                1,
            )
            .unwrap();

        // A real owned workspace, cloned from the source, with one unit of work in it.
        let workspace = base.join("workspace");
        git(
            base,
            &["clone", "--quiet", source.to_str().unwrap(), "workspace"],
        );
        git(&workspace, &["config", "user.name", "Worker"]);
        git(
            &workspace,
            &["config", "user.email", "worker@example.invalid"],
        );
        let workspace_base = git(&workspace, &["rev-parse", "HEAD"]);
        fs::write(workspace.join("feature.txt"), "the unit\n").unwrap();

        let package = SnapshotPackage::capture(SnapshotRequest {
            package_id: PackageId::new(),
            revision: Revision::FIRST,
            feature_id,
            plan_revision: Revision::FIRST,
            task_ids: [task_id].into(),
            workspace: &workspace,
            package_root: &base.join("packages"),
            expected_base_commit: &workspace_base,
            parent_package_id: None,
            validation_policy: ContentValidationPolicy::default(),
        })
        .unwrap();

        store
            .record_workspace(&WorkspaceRecord {
                feature_id,
                revision: Revision::FIRST,
                path: workspace.clone(),
                base_commit: workspace_base,
                prerequisites: serde_json::json!([]),
                created_at_unix_ms: 1,
            })
            .unwrap();
        store
            .record_snapshot(&SnapshotRecord {
                package_id: package.manifest.package_id,
                revision: package.manifest.revision,
                feature_id,
                plan_revision: Revision::FIRST,
                path: package.path.clone(),
                base_tree: package.manifest.base_tree.clone(),
                result_tree: package.manifest.result_tree.clone(),
                content_hash: package.manifest.content_hash.clone(),
                parent_package_id: None,
                manifest: serde_json::to_value(&package.manifest).unwrap(),
                created_at_unix_ms: 1,
            })
            .unwrap();
        store
            .record_package_tasks(package.manifest.package_id, Revision::FIRST, [task_id])
            .unwrap();
        store
            .advance_task_to(feature_id, Revision::FIRST, task_id, TaskStatus::Queued, 1)
            .unwrap();

        let worker = ReleaseWorker::new(base.join("managed"), base.join("release"), "daemon-test");
        Fixture {
            _root: root,
            store,
            worker,
            remote,
            repository_id,
            package_id: package.manifest.package_id,
            feature_id,
            task_id,
        }
    }

    impl Fixture {
        fn request(&self) -> ReleaseRequest {
            ReleaseRequest {
                package_id: self.package_id,
                package_revision: Revision::FIRST,
                message: "feat: add the unit".into(),
                author_name: "Worker".into(),
                author_email: "worker@example.invalid".into(),
            }
        }

        fn remote_head(&self) -> String {
            git(&self.remote, &["rev-parse", "refs/heads/main"])
        }
    }

    #[test]
    fn the_daemon_publishes_a_captured_unit_to_the_real_remote() {
        let mut fixture = fixture();
        let before = fixture.remote_head();

        let request = fixture.request();
        let outcome = fixture
            .worker
            .release(&mut fixture.store, &request, 10)
            .unwrap();
        let ReleaseOutcome::Published { attempt_id, commit } = outcome else {
            panic!("expected a published release, got {outcome:?}");
        };

        // The remote actually moved to the candidate commit.
        assert_ne!(fixture.remote_head(), before);
        assert_eq!(fixture.remote_head(), commit);

        // Every stage is durable, and the attempt ended where it should.
        let attempt = fixture.store.release_attempt(attempt_id).unwrap().unwrap();
        assert_eq!(attempt.state.status(), TaskStatus::Published);
        assert_eq!(attempt.candidate_sha.as_deref(), Some(commit.as_str()));
        assert!(attempt.push_intent_at_unix_ms.is_some());
        assert_eq!(
            attempt.observed_remote_sha.as_deref(),
            Some(commit.as_str())
        );

        // The task the package delivers is published too.
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, fixture.task_id)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Published
        );
        assert!(
            fixture
                .store
                .release_attempts_awaiting_remote_resolution()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_competing_push_blocks_the_attempt_instead_of_overwriting_it() {
        let mut fixture = fixture();

        // Another writer advances the target after the package was captured.
        let source = fixture._root.path().join("source");
        fs::write(source.join("other.txt"), "someone else\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "--quiet", "-m", "competing work"]);
        git(&source, &["push", "--quiet", "origin", "main"]);
        let competing = fixture.remote_head();

        let request = fixture.request();
        let outcome = fixture
            .worker
            .release(&mut fixture.store, &request, 10)
            .unwrap();

        // The invariant is not that the head stops moving, but that the other writer's commit
        // survives: a forced update would drop it out of the remote's history entirely.
        let head = fixture.remote_head();
        let survived = Command::new("git")
            .current_dir(&fixture.remote)
            .args(["merge-base", "--is-ancestor", &competing, &head])
            .status()
            .expect("git runs")
            .success();
        assert!(
            survived,
            "the competing commit {competing} was dropped from the remote history at {head}"
        );

        match outcome {
            ReleaseOutcome::Published { commit, .. } => {
                // Reconciling on top of the competing work is correct; replacing it is not.
                assert_eq!(head, commit);
                let parent = git(&fixture.remote, &["rev-parse", &format!("{commit}^")]);
                assert_eq!(parent, competing);
            }
            ReleaseOutcome::Blocked {
                attempt_id,
                classification,
                ..
            } => {
                assert_eq!(
                    head, competing,
                    "a blocked attempt must not move the remote"
                );
                let attempt = fixture.store.release_attempt(attempt_id).unwrap().unwrap();
                assert_eq!(attempt.state.status(), TaskStatus::Blocked);
                assert!(
                    attempt.failure_classification.is_some(),
                    "a blocked attempt must record why: {classification}"
                );
                // The task stops with it rather than looking releasable.
                assert_eq!(
                    fixture
                        .store
                        .task(fixture.feature_id, Revision::FIRST, fixture.task_id)
                        .unwrap()
                        .unwrap()
                        .state
                        .status(),
                    TaskStatus::Blocked
                );
            }
            ReleaseOutcome::Deferred { .. } => {
                panic!("a competing writer must not be treated as a transport retry")
            }
        }
    }

    #[test]
    fn restart_confirms_a_push_that_landed_before_its_outcome_was_durable() {
        let mut fixture = fixture();
        let source = fixture._root.path().join("source");
        let parent = fixture.remote_head();
        fs::write(source.join("feature.txt"), "the unit\n").unwrap();
        git(&source, &["add", "."]);
        git(
            &source,
            &["commit", "--quiet", "-m", "candidate that landed"],
        );
        git(&source, &["push", "--quiet", "origin", "main"]);
        let candidate = fixture.remote_head();

        let attempt_id = AttemptId::new();
        fixture
            .store
            .open_release_attempt(&NewReleaseAttempt {
                attempt_id,
                repository_id: fixture.repository_id,
                package_id: fixture.package_id,
                package_revision: Revision::FIRST,
                remote: fixture.remote.to_string_lossy().into_owned(),
                target: TargetRef::new("refs/heads/main").unwrap(),
                base_commit: parent.clone(),
                lease: AttemptLease {
                    owner: "daemon-before-crash".into(),
                    expires_at_unix_ms: 10_000,
                },
                created_at_unix_ms: 1,
            })
            .unwrap();
        for (at, status) in [
            TaskStatus::Reconciling,
            TaskStatus::Verifying,
            TaskStatus::CommitPrepared,
        ]
        .into_iter()
        .enumerate()
        {
            fixture
                .store
                .advance_release_attempt(attempt_id, status, None, at as i64 + 2)
                .unwrap();
        }
        fixture
            .store
            .record_push_intent(attempt_id, &candidate, &parent, 5)
            .unwrap();
        fixture
            .store
            .advance_release_attempt(attempt_id, TaskStatus::PushPending, None, 6)
            .unwrap();

        let report = recover_interrupted_releases(&mut fixture.store, &fixture.worker, 20).unwrap();
        assert_eq!(report.resolved_attempt_ids, vec![attempt_id]);
        assert!(report.failures.is_empty());
        assert_eq!(
            fixture.remote_head(),
            candidate,
            "recovery must not push again"
        );
        assert_eq!(
            fixture
                .store
                .release_attempt(attempt_id)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Published
        );
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, fixture.task_id)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Published
        );
    }
}
