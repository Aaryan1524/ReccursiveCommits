//! Durable release-attempt records.
//!
//! A release attempt is the unit of publication ownership: it names the exact package revision
//! being published, the remote and target it is being published to, the candidate commit that was
//! built for it, and whether a push has already been transmitted. Git operations and SQLite cannot
//! share one atomic transaction, so these rows are the only durable evidence a restarting daemon
//! has about work that was in flight.

use std::str::FromStr;

use reccursive_core::{
    AttemptId, PackageId, ReasonCode, RepositoryId, Revision, StateReason, TargetRef, TaskState,
    TaskStatus,
};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// Publication ownership held by one daemon for one target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttemptLease {
    pub owner: String,
    pub expires_at_unix_ms: i64,
}

impl AttemptLease {
    fn validate(&self) -> Result<(), StoreError> {
        if self.owner.trim().is_empty() || self.owner.len() > 256 {
            return Err(StoreError::InvalidData(
                "attempt lease owner must be a non-empty identifier".into(),
            ));
        }
        if self.expires_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "attempt lease expiry must not be negative".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_expired_at(&self, now_unix_ms: i64) -> bool {
        self.expires_at_unix_ms <= now_unix_ms
    }
}

/// Everything required to open a new attempt against a target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewReleaseAttempt {
    pub attempt_id: AttemptId,
    pub repository_id: RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub remote: String,
    pub target: TargetRef,
    pub base_commit: String,
    pub lease: AttemptLease,
    pub created_at_unix_ms: i64,
}

/// One durable publication attempt and its observed remote outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttempt {
    pub attempt_id: AttemptId,
    pub repository_id: RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub remote: String,
    pub target: TargetRef,
    pub base_commit: String,
    /// Present from `commit_prepared` onward; recorded before the remote is contacted.
    pub candidate_sha: Option<String>,
    /// The target commit the candidate was built on. This can differ from `base_commit` after
    /// reconciliation, so it is required to classify an interrupted push safely.
    pub candidate_parent_sha: Option<String>,
    /// When the intent to push the candidate became durable.
    pub push_intent_at_unix_ms: Option<i64>,
    /// What the remote was actually observed to hold, once it could be read.
    pub observed_remote_sha: Option<String>,
    pub failure_classification: Option<String>,
    /// Unsuccessful publication attempts recorded so far, durable across restart so bounded
    /// retry stays bounded when the process dies between failures.
    pub failed_attempts: u8,
    /// Earliest time a scheduler may retry a transport failure. The release worker itself never
    /// waits or retries in-process.
    pub retry_not_before_unix_ms: Option<i64>,
    pub state: TaskState,
    pub lease: AttemptLease,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

impl ReleaseAttempt {
    /// The stage this attempt actually reached, seeing through a block to the state it paused at.
    ///
    /// Blocking does not undo a transmitted push, so recovery decisions must read the resume state
    /// rather than the block itself.
    #[must_use]
    pub const fn reached_stage(&self) -> TaskStatus {
        match self.state.status() {
            TaskStatus::Blocked => match self.state.blocked_from() {
                Some(resume) => resume,
                None => TaskStatus::Blocked,
            },
            other => other,
        }
    }

    /// Reports whether this attempt must be resolved against the remote before more work is built.
    ///
    /// A push that was transmitted but never confirmed is indistinguishable locally from one that
    /// never left the machine, so the remote is the only authority. An attempt that blocked while
    /// its push was in flight is in exactly that position and must not be skipped.
    #[must_use]
    pub const fn needs_remote_resolution(&self) -> bool {
        matches!(self.reached_stage(), TaskStatus::PushPending)
    }

    /// Reports whether this attempt still owns its remote and target.
    ///
    /// Mirrors the schema's partial unique index exactly: a blocked attempt keeps the target only
    /// when it blocked at or after its push.
    #[must_use]
    pub const fn holds_target(&self) -> bool {
        match self.state.status() {
            TaskStatus::Published | TaskStatus::Cancelled | TaskStatus::Superseded => false,
            TaskStatus::Blocked => matches!(
                self.state.blocked_from(),
                Some(TaskStatus::PushPending | TaskStatus::RemoteConfirmed)
            ),
            _ => true,
        }
    }
}

/// Rows still owning their remote and target; kept identical to the schema's partial unique index.
const TARGET_HELD_PREDICATE: &str = "(status IN ('scheduled', 'reconciling', 'verifying', \
     'commit_prepared', 'push_pending', 'remote_confirmed') \
     OR (status = 'blocked' AND blocked_from IN ('push_pending', 'remote_confirmed')))";

/// Rows whose push may have reached the remote without a recorded outcome.
const AWAITING_RESOLUTION_PREDICATE: &str =
    "(status = 'push_pending' OR (status = 'blocked' AND blocked_from = 'push_pending'))";

fn validate_object_id(name: &str, value: &str) -> Result<(), StoreError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StoreError::InvalidData(format!(
            "{name} must be a full hexadecimal object ID"
        )));
    }
    Ok(())
}

fn validate_remote(remote: &str) -> Result<(), StoreError> {
    if remote.trim().is_empty() || remote.len() > 2048 {
        return Err(StoreError::InvalidData(
            "attempt remote must be a non-empty reference".into(),
        ));
    }
    Ok(())
}

const fn status_column(status: TaskStatus) -> Option<&'static str> {
    match status {
        TaskStatus::Scheduled => Some("scheduled"),
        TaskStatus::Reconciling => Some("reconciling"),
        TaskStatus::Verifying => Some("verifying"),
        TaskStatus::CommitPrepared => Some("commit_prepared"),
        TaskStatus::PushPending => Some("push_pending"),
        TaskStatus::RemoteConfirmed => Some("remote_confirmed"),
        TaskStatus::Published => Some("published"),
        TaskStatus::Blocked => Some("blocked"),
        TaskStatus::Cancelled => Some("cancelled"),
        TaskStatus::Superseded => Some("superseded"),
        TaskStatus::Planned
        | TaskStatus::Building
        | TaskStatus::Captured
        | TaskStatus::Validated
        | TaskStatus::Queued => None,
    }
}

fn status_from_column(value: &str) -> Result<TaskStatus, StoreError> {
    match value {
        "scheduled" => Ok(TaskStatus::Scheduled),
        "reconciling" => Ok(TaskStatus::Reconciling),
        "verifying" => Ok(TaskStatus::Verifying),
        "commit_prepared" => Ok(TaskStatus::CommitPrepared),
        "push_pending" => Ok(TaskStatus::PushPending),
        "remote_confirmed" => Ok(TaskStatus::RemoteConfirmed),
        "published" => Ok(TaskStatus::Published),
        "blocked" => Ok(TaskStatus::Blocked),
        "cancelled" => Ok(TaskStatus::Cancelled),
        "superseded" => Ok(TaskStatus::Superseded),
        other => Err(StoreError::InvalidData(format!(
            "stored attempt status {other} is not a publication stage"
        ))),
    }
}

fn require_publication_stage(status: TaskStatus) -> Result<&'static str, StoreError> {
    status_column(status).ok_or_else(|| {
        StoreError::InvalidData(format!(
            "{status:?} is a production state and cannot be a release attempt state"
        ))
    })
}

const ATTEMPT_COLUMNS: &str = "attempt_id, repository_id, package_id, package_revision, \
     target_remote, target_ref, base_commit, candidate_sha, candidate_parent_sha, \
     push_intent_at_unix_ms, observed_remote_sha, failure_classification, status, reason_json, \
     blocked_from, failed_attempts, retry_not_before_unix_ms, lease_owner, \
     lease_expires_at_unix_ms, created_at_unix_ms, updated_at_unix_ms";

fn attempt_from_row(row: &Row<'_>) -> Result<ReleaseAttempt, rusqlite::Error> {
    let parse = |column: usize, value: String| -> Result<_, rusqlite::Error> {
        AttemptId::from_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let conversion = |column: usize, error: StoreError| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    };

    let attempt_id = parse(0, row.get(0)?)?;
    let repository_id = RepositoryId::from_str(&row.get::<_, String>(1)?)
        .map_err(|error| conversion(1, StoreError::InvalidData(error.to_string())))?;
    let package_id = PackageId::from_str(&row.get::<_, String>(2)?)
        .map_err(|error| conversion(2, StoreError::InvalidData(error.to_string())))?;
    let package_revision = Revision::new(row.get::<_, u32>(3)?)
        .map_err(|error| conversion(3, StoreError::InvalidData(error.to_string())))?;
    let target = TargetRef::new(row.get::<_, String>(5)?)
        .map_err(|error| conversion(5, StoreError::InvalidData(error.to_string())))?;

    let status =
        status_from_column(&row.get::<_, String>(12)?).map_err(|error| conversion(12, error))?;
    let reason = row
        .get::<_, Option<String>>(13)?
        .map(|raw| serde_json::from_str::<StoreReason>(&raw))
        .transpose()
        .map_err(|error| conversion(13, StoreError::Json(error)))?
        .map(StateReason::from);
    let blocked_from = row
        .get::<_, Option<String>>(14)?
        .map(|raw| status_from_column(&raw))
        .transpose()
        .map_err(|error| conversion(14, error))?;
    let state = TaskState::restore(status, reason, blocked_from)
        .map_err(|error| conversion(12, StoreError::InvalidData(error.to_string())))?;

    Ok(ReleaseAttempt {
        attempt_id,
        repository_id,
        package_id,
        package_revision,
        remote: row.get(4)?,
        target,
        base_commit: row.get(6)?,
        candidate_sha: row.get(7)?,
        candidate_parent_sha: row.get(8)?,
        push_intent_at_unix_ms: row.get(9)?,
        observed_remote_sha: row.get(10)?,
        failure_classification: row.get(11)?,
        failed_attempts: row.get(15)?,
        retry_not_before_unix_ms: row.get(16)?,
        state,
        lease: AttemptLease {
            owner: row.get(17)?,
            expires_at_unix_ms: row.get(18)?,
        },
        created_at_unix_ms: row.get(19)?,
        updated_at_unix_ms: row.get(20)?,
    })
}

/// Serializable mirror of the domain reason, so stored rows survive domain refactors.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoreReason {
    code: ReasonCode,
    message: String,
}

impl From<&StateReason> for StoreReason {
    fn from(value: &StateReason) -> Self {
        Self {
            code: value.code.clone(),
            message: value.message.clone(),
        }
    }
}

impl From<StoreReason> for StateReason {
    fn from(value: StoreReason) -> Self {
        Self {
            code: value.code,
            message: value.message,
        }
    }
}

fn encode_reason(reason: Option<&StateReason>) -> Result<Option<String>, StoreError> {
    reason
        .map(|reason| serde_json::to_string(&StoreReason::from(reason)))
        .transpose()
        .map_err(StoreError::from)
}

impl Store {
    /// Opens an attempt at `scheduled`, taking exclusive ownership of the target.
    ///
    /// A live attempt already holding the same remote and target is rejected rather than
    /// duplicated, so two publishers can never race for one branch.
    pub fn open_release_attempt(
        &mut self,
        request: &NewReleaseAttempt,
    ) -> Result<ReleaseAttempt, StoreError> {
        validate_remote(&request.remote)?;
        validate_object_id("attempt base commit", &request.base_commit)?;
        request.lease.validate()?;
        if request.created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "attempt creation timestamp must not be negative".into(),
            ));
        }

        self.connection
            .execute(
                "INSERT INTO release_attempts (
                    attempt_id, repository_id, package_id, package_revision, target_remote,
                    target_ref, base_commit, status, lease_owner, lease_expires_at_unix_ms,
                    created_at_unix_ms, updated_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'scheduled', ?8, ?9, ?10, ?10)",
                params![
                    request.attempt_id.to_string(),
                    request.repository_id.to_string(),
                    request.package_id.to_string(),
                    request.package_revision.get(),
                    request.remote,
                    request.target.as_str(),
                    request.base_commit,
                    request.lease.owner,
                    request.lease.expires_at_unix_ms,
                    request.created_at_unix_ms,
                ],
            )
            .map_err(map_attempt_write_error)?;

        self.release_attempt(request.attempt_id)?
            .ok_or_else(|| StoreError::InvalidData("opened attempt could not be read back".into()))
    }

    /// Loads one attempt by identifier.
    pub fn release_attempt(&self, id: AttemptId) -> Result<Option<ReleaseAttempt>, StoreError> {
        self.connection
            .query_row(
                &format!("SELECT {ATTEMPT_COLUMNS} FROM release_attempts WHERE attempt_id = ?1"),
                [id.to_string()],
                attempt_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Records the candidate commit and the intent to push it, before the remote is contacted.
    ///
    /// This is the durable half of invariant 5: after this returns, a crash can still discover
    /// exactly which commit may have reached the remote.
    pub fn record_push_intent(
        &mut self,
        id: AttemptId,
        candidate_sha: &str,
        candidate_parent_sha: &str,
        now_unix_ms: i64,
    ) -> Result<ReleaseAttempt, StoreError> {
        validate_object_id("candidate commit", candidate_sha)?;
        validate_object_id("candidate parent commit", candidate_parent_sha)?;
        let attempt = self.expect_attempt(id)?;
        if attempt.state.status() != TaskStatus::CommitPrepared {
            return Err(StoreError::Conflict(format!(
                "push intent requires a prepared candidate; attempt is {:?}",
                attempt.state.status()
            )));
        }
        if let Some(existing) = attempt.candidate_sha.as_deref()
            && existing != candidate_sha
        {
            return Err(StoreError::Conflict(
                "attempt already recorded a different candidate commit".into(),
            ));
        }

        let changed = self
            .connection
            .execute(
                "UPDATE release_attempts
                 SET candidate_sha = ?2, candidate_parent_sha = ?3, push_intent_at_unix_ms = ?4,
                     updated_at_unix_ms = ?4
                 WHERE attempt_id = ?1 AND status = 'commit_prepared'
                   AND (candidate_sha IS NULL OR candidate_sha = ?2)
                   AND (candidate_parent_sha IS NULL OR candidate_parent_sha = ?3)",
                params![
                    id.to_string(),
                    candidate_sha,
                    candidate_parent_sha,
                    now_unix_ms
                ],
            )
            .map_err(map_attempt_write_error)?;
        expect_one_row(changed)?;
        self.expect_attempt(id)
    }

    /// Applies one durable state change, rejecting any transition the domain forbids.
    pub fn advance_release_attempt(
        &mut self,
        id: AttemptId,
        target: TaskStatus,
        reason: Option<StateReason>,
        now_unix_ms: i64,
    ) -> Result<ReleaseAttempt, StoreError> {
        let column = require_publication_stage(target)?;
        let mut attempt = self.expect_attempt(id)?;
        if now_unix_ms < attempt.updated_at_unix_ms {
            return Err(StoreError::InvalidData(
                "attempt update time must not move backwards".into(),
            ));
        }
        let observed_status = require_publication_stage(attempt.state.status())?;
        let observed_blocked_from = attempt.state.blocked_from().and_then(status_column);
        attempt
            .state
            .transition(target, reason)
            .map_err(|error| StoreError::Conflict(error.to_string()))?;

        let changed = self
            .connection
            .execute(
                "UPDATE release_attempts
                 SET status = ?2, reason_json = ?3, blocked_from = ?4,
                     retry_not_before_unix_ms = CASE
                        WHEN ?2 = 'push_pending' THEN retry_not_before_unix_ms
                        ELSE NULL
                     END,
                     updated_at_unix_ms = ?5
                 WHERE attempt_id = ?1 AND status = ?6 AND blocked_from IS ?7",
                params![
                    id.to_string(),
                    column,
                    encode_reason(attempt.state.reason())?,
                    attempt.state.blocked_from().and_then(status_column),
                    now_unix_ms,
                    observed_status,
                    observed_blocked_from,
                ],
            )
            .map_err(map_attempt_write_error)?;
        expect_one_row(changed)?;
        self.expect_attempt(id)
    }

    /// Records what the remote was actually observed to hold for this attempt's target.
    pub fn record_remote_observation(
        &mut self,
        id: AttemptId,
        observed_remote_sha: &str,
        now_unix_ms: i64,
    ) -> Result<ReleaseAttempt, StoreError> {
        validate_object_id("observed remote commit", observed_remote_sha)?;
        let attempt = self.expect_attempt(id)?;
        let observed_status = require_publication_stage(attempt.state.status())?;
        let changed = self
            .connection
            .execute(
                "UPDATE release_attempts
                 SET observed_remote_sha = ?2, updated_at_unix_ms = ?3
                 WHERE attempt_id = ?1 AND status = ?4",
                params![
                    id.to_string(),
                    observed_remote_sha,
                    now_unix_ms,
                    observed_status,
                ],
            )
            .map_err(map_attempt_write_error)?;
        expect_one_row(changed)?;
        self.expect_attempt(id)
    }

    /// Records one unsuccessful publication and its classification.
    ///
    /// The counter is durable so a process that dies between failures cannot restart a retry
    /// budget it has already spent.
    pub fn record_publication_failure(
        &mut self,
        id: AttemptId,
        classification: &str,
        now_unix_ms: i64,
    ) -> Result<ReleaseAttempt, StoreError> {
        if classification.trim().is_empty() || classification.len() > 256 {
            return Err(StoreError::InvalidData(
                "failure classification must be a short non-empty label".into(),
            ));
        }
        let attempt = self.expect_attempt(id)?;
        if attempt.failed_attempts == u8::MAX {
            return Err(StoreError::Conflict(
                "attempt has already recorded the maximum number of failures".into(),
            ));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE release_attempts
                 SET failure_classification = ?2, failed_attempts = failed_attempts + 1,
                     updated_at_unix_ms = ?3
                 WHERE attempt_id = ?1 AND failed_attempts = ?4 AND status = ?5",
                params![
                    id.to_string(),
                    classification,
                    now_unix_ms,
                    attempt.failed_attempts,
                    require_publication_stage(attempt.state.status())?,
                ],
            )
            .map_err(map_attempt_write_error)?;
        expect_one_row(changed)?;
        self.expect_attempt(id)
    }

    /// Records a retryable transport failure and the earliest time it may run again.
    ///
    /// The attempt stays at `push_pending`: the candidate may already have reached the remote,
    /// so no other candidate may take the target while the scheduler waits to reconcile it.
    pub fn defer_publication_retry(
        &mut self,
        id: AttemptId,
        classification: &str,
        retry_not_before_unix_ms: i64,
        now_unix_ms: i64,
    ) -> Result<ReleaseAttempt, StoreError> {
        if classification.trim().is_empty() || classification.len() > 256 {
            return Err(StoreError::InvalidData(
                "failure classification must be a short non-empty label".into(),
            ));
        }
        if retry_not_before_unix_ms < now_unix_ms {
            return Err(StoreError::InvalidData(
                "publication retry time must not precede the failure".into(),
            ));
        }
        let attempt = self.expect_attempt(id)?;
        if attempt.state.status() != TaskStatus::PushPending {
            return Err(StoreError::Conflict(
                "only a transmitted push can be deferred for retry".into(),
            ));
        }
        if attempt.failed_attempts == u8::MAX {
            return Err(StoreError::Conflict(
                "attempt has already recorded the maximum number of failures".into(),
            ));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE release_attempts
                 SET failure_classification = ?2, failed_attempts = failed_attempts + 1,
                     retry_not_before_unix_ms = ?3, updated_at_unix_ms = ?4
                 WHERE attempt_id = ?1 AND status = 'push_pending' AND failed_attempts = ?5
                   AND retry_not_before_unix_ms IS ?6",
                params![
                    id.to_string(),
                    classification,
                    retry_not_before_unix_ms,
                    now_unix_ms,
                    attempt.failed_attempts,
                    attempt.retry_not_before_unix_ms,
                ],
            )
            .map_err(map_attempt_write_error)?;
        expect_one_row(changed)?;
        self.expect_attempt(id)
    }

    /// Lists attempts whose push may have reached the remote without a recorded outcome.
    ///
    /// Invariant 6: these must be reconciled against the remote before any new candidate is built,
    /// so a successful-but-unrecorded push is never published twice.
    pub fn release_attempts_awaiting_remote_resolution(
        &self,
    ) -> Result<Vec<ReleaseAttempt>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM release_attempts
             WHERE {AWAITING_RESOLUTION_PREDICATE} ORDER BY updated_at_unix_ms"
        ))?;
        let rows = statement.query_map([], attempt_from_row)?;
        rows.map(|row| row.map_err(StoreError::from)).collect()
    }

    /// Returns the attempt currently holding a remote and target, if any.
    pub fn live_release_attempt(
        &self,
        repository_id: RepositoryId,
        remote: &str,
        target: &TargetRef,
    ) -> Result<Option<ReleaseAttempt>, StoreError> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {ATTEMPT_COLUMNS} FROM release_attempts
                     WHERE repository_id = ?1 AND target_remote = ?2 AND target_ref = ?3
                       AND {TARGET_HELD_PREDICATE}"
                ),
                params![repository_id.to_string(), remote, target.as_str()],
                attempt_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Transfers an attempt whose lease has expired to a new owner.
    ///
    /// A live lease is never stolen; recovery only reclaims ownership the previous owner has
    /// demonstrably stopped renewing.
    pub fn claim_expired_release_attempt(
        &mut self,
        id: AttemptId,
        lease: &AttemptLease,
        now_unix_ms: i64,
    ) -> Result<ReleaseAttempt, StoreError> {
        lease.validate()?;
        let attempt = self.expect_attempt(id)?;
        if !attempt.lease.is_expired_at(now_unix_ms) && attempt.lease.owner != lease.owner {
            return Err(StoreError::Conflict(format!(
                "attempt lease is held by {} until {}",
                attempt.lease.owner, attempt.lease.expires_at_unix_ms
            )));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE release_attempts
                 SET lease_owner = ?2, lease_expires_at_unix_ms = ?3, updated_at_unix_ms = ?4
                 WHERE attempt_id = ?1 AND lease_owner = ?5 AND lease_expires_at_unix_ms = ?6",
                params![
                    id.to_string(),
                    lease.owner,
                    lease.expires_at_unix_ms,
                    now_unix_ms,
                    attempt.lease.owner,
                    attempt.lease.expires_at_unix_ms,
                ],
            )
            .map_err(map_attempt_write_error)?;
        expect_one_row(changed)?;
        self.expect_attempt(id)
    }

    fn expect_attempt(&self, id: AttemptId) -> Result<ReleaseAttempt, StoreError> {
        self.release_attempt(id)?
            .ok_or_else(|| StoreError::InvalidData(format!("release attempt {id} is not stored")))
    }
}

/// Rejects a guarded update whose row changed between the read and the write.
///
/// Every mutation here decides from a value it read first, so the write repeats that value as a
/// condition and the decision is applied atomically. Losing the race is a conflict, never a silent
/// overwrite.
///
/// A single-threaded caller cannot reach this branch, because each mutation re-reads immediately
/// before writing. It exists for two owners racing across processes — the case the lease exists to
/// arbitrate and WAL mode permits. Deterministic coverage arrives with the P4-T07 concurrency
/// tests; until then this is the guard that keeps that race a conflict rather than lost work.
fn expect_one_row(changed: usize) -> Result<(), StoreError> {
    if changed == 1 {
        return Ok(());
    }
    Err(StoreError::Conflict(
        "release attempt changed concurrently; re-read it before retrying".into(),
    ))
}

fn map_attempt_write_error(error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, message)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let detail = message.as_deref().unwrap_or("constraint violated");
            StoreError::Conflict(format!(
                "release attempt rejected by storage rules: {detail}"
            ))
        }
        _ => StoreError::from(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reccursive_core::{
        AcceptanceCheck, FeatureId, FeaturePlan, PLAN_SCHEMA_VERSION, PlanPhase, PlanTask,
        PublicationMode, RepositoryPolicy, TaskId,
    };
    use serde_json::json;

    use super::*;
    use crate::{RepositoryRegistration, SnapshotRecord, WorkspaceRecord};

    const REMOTE: &str = "ssh://git@example.invalid/project.git";

    struct Fixture {
        store: Store,
        repository_id: RepositoryId,
        package_id: PackageId,
        target: TargetRef,
    }

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn fixture() -> Fixture {
        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        let registration = RepositoryRegistration::new(
            repository_id,
            "/tmp/example",
            REMOTE,
            "/tmp/managed/example.git",
            1,
        )
        .unwrap();
        let policy = RepositoryPolicy::new(
            repository_id,
            Revision::FIRST,
            PublicationMode::ScheduledCreation,
            target.clone(),
            None,
        )
        .unwrap();
        store.enroll_repository(&registration, &policy).unwrap();

        let feature_id = FeatureId::new();
        let plan = FeaturePlan {
            schema_version: PLAN_SCHEMA_VERSION,
            feature_id,
            revision: Revision::FIRST,
            repository_id,
            goal: "Publish one release unit".into(),
            target: target.clone(),
            sealed: true,
            phases: vec![PlanPhase {
                id: "delivery".into(),
                name: "Delivery".into(),
                tasks: vec![PlanTask {
                    id: TaskId::new(),
                    name: "Publish".into(),
                    dependencies: BTreeMap::new(),
                    acceptance_checks: vec![AcceptanceCheck {
                        id: "published".into(),
                        description: "Candidate reaches the target".into(),
                    }],
                }],
            }],
        };
        store.import_plan(&plan, 1).unwrap();

        store
            .record_workspace(&WorkspaceRecord {
                feature_id,
                revision: Revision::FIRST,
                path: "/tmp/managed/workspace".into(),
                base_commit: object_id('a'),
                prerequisites: json!([]),
                created_at_unix_ms: 1,
            })
            .unwrap();

        let package_id = PackageId::new();
        store
            .record_snapshot(&SnapshotRecord {
                package_id,
                revision: Revision::FIRST,
                feature_id,
                plan_revision: Revision::FIRST,
                path: "/tmp/managed/package".into(),
                base_tree: object_id('b'),
                result_tree: object_id('c'),
                content_hash: std::iter::repeat_n('d', 64).collect(),
                parent_package_id: None,
                manifest: json!({ "paths": [] }),
                created_at_unix_ms: 1,
            })
            .unwrap();

        Fixture {
            store,
            repository_id,
            package_id,
            target,
        }
    }

    impl Fixture {
        fn request(&self, owner: &str, lease_expires_at_unix_ms: i64) -> NewReleaseAttempt {
            NewReleaseAttempt {
                attempt_id: AttemptId::new(),
                repository_id: self.repository_id,
                package_id: self.package_id,
                package_revision: Revision::FIRST,
                remote: REMOTE.into(),
                target: self.target.clone(),
                base_commit: object_id('a'),
                lease: AttemptLease {
                    owner: owner.into(),
                    expires_at_unix_ms: lease_expires_at_unix_ms,
                },
                created_at_unix_ms: 1,
            }
        }

        fn advance_to_commit_prepared(&mut self, id: AttemptId) {
            for (step, status) in [
                TaskStatus::Reconciling,
                TaskStatus::Verifying,
                TaskStatus::CommitPrepared,
            ]
            .into_iter()
            .enumerate()
            {
                self.store
                    .advance_release_attempt(id, status, None, step as i64 + 2)
                    .unwrap();
            }
        }
    }

    #[test]
    fn attempt_survives_restart_with_its_publication_state() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        let opened = fixture.store.open_release_attempt(&request).unwrap();
        assert_eq!(opened.state.status(), TaskStatus::Scheduled);
        assert!(opened.candidate_sha.is_none());

        fixture.advance_to_commit_prepared(request.attempt_id);
        let prepared = fixture
            .store
            .record_push_intent(request.attempt_id, &object_id('e'), &object_id('a'), 5)
            .unwrap();
        assert_eq!(
            prepared.candidate_sha.as_deref(),
            Some(object_id('e').as_str())
        );
        assert_eq!(prepared.push_intent_at_unix_ms, Some(5));
        assert_eq!(
            prepared.candidate_parent_sha.as_deref(),
            Some(object_id('a').as_str())
        );

        let reloaded = fixture
            .store
            .release_attempt(request.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded, prepared);
    }

    #[test]
    fn push_state_is_unreachable_without_a_durable_candidate_and_intent() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&request).unwrap();
        fixture.advance_to_commit_prepared(request.attempt_id);

        let rejected = fixture.store.advance_release_attempt(
            request.attempt_id,
            TaskStatus::PushPending,
            None,
            5,
        );
        assert!(
            matches!(rejected, Err(StoreError::Conflict(_))),
            "storage must refuse a push state with no recorded candidate: {rejected:?}"
        );

        fixture
            .store
            .record_push_intent(request.attempt_id, &object_id('e'), &object_id('a'), 5)
            .unwrap();
        let pending = fixture
            .store
            .advance_release_attempt(request.attempt_id, TaskStatus::PushPending, None, 6)
            .unwrap();
        assert!(pending.state.status().may_have_reached_remote());
    }

    #[test]
    fn transmitted_push_is_listed_for_remote_resolution_until_it_is_confirmed() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&request).unwrap();
        fixture.advance_to_commit_prepared(request.attempt_id);
        fixture
            .store
            .record_push_intent(request.attempt_id, &object_id('e'), &object_id('a'), 5)
            .unwrap();
        assert!(
            fixture
                .store
                .release_attempts_awaiting_remote_resolution()
                .unwrap()
                .is_empty()
        );

        fixture
            .store
            .advance_release_attempt(request.attempt_id, TaskStatus::PushPending, None, 6)
            .unwrap();
        let awaiting = fixture
            .store
            .release_attempts_awaiting_remote_resolution()
            .unwrap();
        assert_eq!(awaiting.len(), 1);
        assert!(awaiting[0].needs_remote_resolution());
        assert_eq!(
            awaiting[0].candidate_sha.as_deref(),
            Some(object_id('e').as_str())
        );

        fixture
            .store
            .record_remote_observation(request.attempt_id, &object_id('e'), 7)
            .unwrap();
        fixture
            .store
            .advance_release_attempt(request.attempt_id, TaskStatus::RemoteConfirmed, None, 8)
            .unwrap();
        assert!(
            fixture
                .store
                .release_attempts_awaiting_remote_resolution()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn one_live_attempt_owns_a_target_and_blocking_releases_it() {
        let mut fixture = fixture();
        let first = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&first).unwrap();

        let duplicate = fixture
            .store
            .open_release_attempt(&fixture.request("daemon-a", 10_000));
        assert!(
            matches!(duplicate, Err(StoreError::Conflict(_))),
            "a second live attempt must not claim the same target: {duplicate:?}"
        );
        assert_eq!(
            fixture
                .store
                .live_release_attempt(fixture.repository_id, REMOTE, &fixture.target)
                .unwrap()
                .map(|attempt| attempt.attempt_id),
            Some(first.attempt_id)
        );

        fixture
            .store
            .advance_release_attempt(
                first.attempt_id,
                TaskStatus::Blocked,
                Some(StateReason::new(ReasonCode::Conflict, "manual resolution required").unwrap()),
                2,
            )
            .unwrap();
        fixture
            .store
            .open_release_attempt(&fixture.request("daemon-a", 10_000))
            .expect("a blocked attempt must not starve unrelated eligible work");

        let blocked = fixture
            .store
            .release_attempt(first.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(blocked.state.blocked_from(), Some(TaskStatus::Scheduled));
        assert_eq!(
            blocked.state.reason().map(|reason| reason.code.clone()),
            Some(ReasonCode::Conflict)
        );
    }

    #[test]
    fn a_live_lease_is_never_stolen_but_an_expired_one_recovers() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&request).unwrap();
        let successor = AttemptLease {
            owner: "daemon-b".into(),
            expires_at_unix_ms: 20_000,
        };

        let contested =
            fixture
                .store
                .claim_expired_release_attempt(request.attempt_id, &successor, 9_999);
        assert!(
            matches!(contested, Err(StoreError::Conflict(_))),
            "a live lease must not be stolen: {contested:?}"
        );

        let recovered = fixture
            .store
            .claim_expired_release_attempt(request.attempt_id, &successor, 10_000)
            .unwrap();
        assert_eq!(recovered.lease.owner, "daemon-b");
        assert_eq!(recovered.state.status(), TaskStatus::Scheduled);
    }

    #[test]
    fn blocking_after_a_push_keeps_the_target_and_stays_queued_for_resolution() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&request).unwrap();
        fixture.advance_to_commit_prepared(request.attempt_id);
        fixture
            .store
            .record_push_intent(request.attempt_id, &object_id('e'), &object_id('a'), 5)
            .unwrap();
        fixture
            .store
            .advance_release_attempt(request.attempt_id, TaskStatus::PushPending, None, 6)
            .unwrap();

        let blocked = fixture
            .store
            .advance_release_attempt(
                request.attempt_id,
                TaskStatus::Blocked,
                Some(StateReason::new(ReasonCode::DeviceUnavailable, "network went away").unwrap()),
                7,
            )
            .unwrap();

        assert!(
            blocked.needs_remote_resolution(),
            "a push that was in flight when the attempt blocked still has an unknown outcome"
        );
        assert_eq!(
            fixture
                .store
                .release_attempts_awaiting_remote_resolution()
                .unwrap()
                .len(),
            1,
            "blocking must not hide a transmitted push from restart reconciliation"
        );

        assert!(blocked.holds_target());
        let duplicate = fixture
            .store
            .open_release_attempt(&fixture.request("daemon-a", 10_000));
        assert!(
            matches!(duplicate, Err(StoreError::Conflict(_))),
            "a second attempt must not build a candidate while an earlier push may be live: \
             {duplicate:?}"
        );
        assert_eq!(
            fixture
                .store
                .live_release_attempt(fixture.repository_id, REMOTE, &fixture.target)
                .unwrap()
                .map(|attempt| attempt.attempt_id),
            Some(request.attempt_id)
        );
    }

    #[test]
    fn failure_count_is_durable_so_a_restart_cannot_refill_the_retry_budget() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        let opened = fixture.store.open_release_attempt(&request).unwrap();
        assert_eq!(opened.failed_attempts, 0);

        for expected in 1..=3 {
            let recorded = fixture
                .store
                .record_publication_failure(
                    request.attempt_id,
                    "transport",
                    i64::from(expected) + 1,
                )
                .unwrap();
            assert_eq!(recorded.failed_attempts, expected);
        }
        assert_eq!(
            fixture
                .store
                .release_attempt(request.attempt_id)
                .unwrap()
                .unwrap()
                .failed_attempts,
            3
        );
    }

    #[test]
    fn retry_deferral_is_durable_and_keeps_the_transmitted_push_owned() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&request).unwrap();
        fixture.advance_to_commit_prepared(request.attempt_id);
        fixture
            .store
            .record_push_intent(request.attempt_id, &object_id('e'), &object_id('a'), 5)
            .unwrap();
        fixture
            .store
            .advance_release_attempt(request.attempt_id, TaskStatus::PushPending, None, 6)
            .unwrap();

        let deferred = fixture
            .store
            .defer_publication_retry(request.attempt_id, "transport", 5_006, 6)
            .unwrap();
        assert_eq!(deferred.state.status(), TaskStatus::PushPending);
        assert_eq!(deferred.failed_attempts, 1);
        assert_eq!(deferred.retry_not_before_unix_ms, Some(5_006));
        assert!(deferred.holds_target());
        assert!(matches!(
            fixture
                .store
                .open_release_attempt(&fixture.request("daemon-b", 10_000)),
            Err(StoreError::Conflict(_))
        ));

        fixture
            .store
            .record_remote_observation(request.attempt_id, &object_id('e'), 7)
            .unwrap();
        let confirmed = fixture
            .store
            .advance_release_attempt(request.attempt_id, TaskStatus::RemoteConfirmed, None, 8)
            .unwrap();
        assert!(confirmed.retry_not_before_unix_ms.is_none());
    }

    #[test]
    fn production_states_are_refused_as_attempt_states() {
        let mut fixture = fixture();
        let request = fixture.request("daemon-a", 10_000);
        fixture.store.open_release_attempt(&request).unwrap();
        let rejected = fixture.store.advance_release_attempt(
            request.attempt_id,
            TaskStatus::Captured,
            None,
            2,
        );
        assert!(
            matches!(rejected, Err(StoreError::InvalidData(_))),
            "an attempt must not hold a production state: {rejected:?}"
        );
    }
}
