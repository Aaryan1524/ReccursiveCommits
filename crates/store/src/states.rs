//! Shared encoding for durable task and attempt states.
//!
//! Two tables store a lifecycle state (`plan_tasks` and `release_attempts`) and both constrain the
//! column with a SQL `CHECK` list. Keeping one total mapping here means a state added to the domain
//! cannot silently disagree with either constraint.

use reccursive_core::{ReasonCode, StateReason, TaskState, TaskStatus};
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Serializable mirror of the domain reason, so stored rows survive domain refactors.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoreReason {
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

/// Total mapping from a domain status to its stored column value.
pub(crate) const fn status_column(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Planned => "planned",
        TaskStatus::Building => "building",
        TaskStatus::Captured => "captured",
        TaskStatus::Validated => "validated",
        TaskStatus::Queued => "queued",
        TaskStatus::Scheduled => "scheduled",
        TaskStatus::Reconciling => "reconciling",
        TaskStatus::Verifying => "verifying",
        TaskStatus::CommitPrepared => "commit_prepared",
        TaskStatus::PushPending => "push_pending",
        TaskStatus::RemoteConfirmed => "remote_confirmed",
        TaskStatus::Published => "published",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Superseded => "superseded",
    }
}

/// Inverse of [`status_column`], rejecting any value the domain does not define.
pub(crate) fn status_from_column(value: &str) -> Result<TaskStatus, StoreError> {
    match value {
        "planned" => Ok(TaskStatus::Planned),
        "building" => Ok(TaskStatus::Building),
        "captured" => Ok(TaskStatus::Captured),
        "validated" => Ok(TaskStatus::Validated),
        "queued" => Ok(TaskStatus::Queued),
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
            "stored status {other} is not a known lifecycle state"
        ))),
    }
}

pub(crate) fn encode_reason(reason: Option<&StateReason>) -> Result<Option<String>, StoreError> {
    reason
        .map(|reason| serde_json::to_string(&StoreReason::from(reason)))
        .transpose()
        .map_err(StoreError::from)
}

pub(crate) fn decode_reason(raw: Option<String>) -> Result<Option<StateReason>, StoreError> {
    raw.map(|raw| serde_json::from_str::<StoreReason>(&raw))
        .transpose()
        .map(|reason| reason.map(StateReason::from))
        .map_err(StoreError::from)
}

/// Rebuilds a durable state from its three stored columns.
pub(crate) fn restore_state(
    status: &str,
    reason_json: Option<String>,
    blocked_from: Option<String>,
) -> Result<TaskState, StoreError> {
    let status = status_from_column(status)?;
    let reason = decode_reason(reason_json)?;
    let blocked_from = blocked_from
        .map(|value| status_from_column(&value))
        .transpose()?;
    TaskState::restore(status, reason, blocked_from)
        .map_err(|error| StoreError::InvalidData(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_domain_status_round_trips_through_its_column_value() {
        for status in [
            TaskStatus::Planned,
            TaskStatus::Building,
            TaskStatus::Captured,
            TaskStatus::Validated,
            TaskStatus::Queued,
            TaskStatus::Scheduled,
            TaskStatus::Reconciling,
            TaskStatus::Verifying,
            TaskStatus::CommitPrepared,
            TaskStatus::PushPending,
            TaskStatus::RemoteConfirmed,
            TaskStatus::Published,
            TaskStatus::Blocked,
            TaskStatus::Cancelled,
            TaskStatus::Superseded,
        ] {
            assert_eq!(status_from_column(status_column(status)).unwrap(), status);
        }
        assert!(status_from_column("not_a_state").is_err());
    }
}
