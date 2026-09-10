use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Durable task and publication states.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Planned,
    Building,
    Captured,
    Validated,
    Queued,
    Scheduled,
    Reconciling,
    Verifying,
    CommitPrepared,
    PushPending,
    RemoteConfirmed,
    Published,
    Blocked,
    Cancelled,
    Superseded,
}

impl TaskStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Published | Self::Cancelled | Self::Superseded)
    }

    /// Reports whether a release attempt may durably occupy this state.
    ///
    /// Attempts own the publication half of the lifecycle only; production states belong to the
    /// task that produced the package.
    #[must_use]
    pub const fn is_publication_stage(self) -> bool {
        matches!(
            self,
            Self::Scheduled
                | Self::Reconciling
                | Self::Verifying
                | Self::CommitPrepared
                | Self::PushPending
                | Self::RemoteConfirmed
                | Self::Published
                | Self::Blocked
                | Self::Cancelled
                | Self::Superseded
        )
    }

    /// Reports whether reaching this state may have already transmitted a push to the remote.
    ///
    /// Callers use this to distinguish queued work that can be stopped locally from work whose
    /// outcome must be resolved against the remote first.
    #[must_use]
    pub const fn may_have_reached_remote(self) -> bool {
        matches!(
            self,
            Self::PushPending | Self::RemoteConfirmed | Self::Published
        )
    }

    const fn normal_successor(self) -> Option<Self> {
        match self {
            Self::Planned => Some(Self::Building),
            Self::Building => Some(Self::Captured),
            Self::Captured => Some(Self::Validated),
            Self::Validated => Some(Self::Queued),
            Self::Queued => Some(Self::Scheduled),
            Self::Scheduled => Some(Self::Reconciling),
            Self::Reconciling => Some(Self::Verifying),
            Self::Verifying => Some(Self::CommitPrepared),
            Self::CommitPrepared => Some(Self::PushPending),
            Self::PushPending => Some(Self::RemoteConfirmed),
            Self::RemoteConfirmed => Some(Self::Published),
            Self::Published | Self::Blocked | Self::Cancelled | Self::Superseded => None,
        }
    }
}

/// Machine-readable category attached to a non-happy-path transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    UserRequested,
    PrerequisiteCancelled,
    Conflict,
    ValidationFailed,
    DeviceUnavailable,
    AuthenticationRequired,
    SupersededByRevision,
    Other(String),
}

/// Human and machine-readable context for blocking or terminal decisions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StateReason {
    pub code: ReasonCode,
    pub message: String,
}

impl StateReason {
    pub fn new(code: ReasonCode, message: impl Into<String>) -> Result<Self, TransitionError> {
        let message = message.into();
        if message.trim().is_empty() {
            return Err(TransitionError::EmptyReason);
        }
        Ok(Self { code, message })
    }
}

/// Current task state, including the exact state to restore after a recoverable block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskState {
    status: TaskStatus,
    reason: Option<StateReason>,
    blocked_from: Option<TaskStatus>,
}

impl TaskState {
    #[must_use]
    pub const fn planned() -> Self {
        Self {
            status: TaskStatus::Planned,
            reason: None,
            blocked_from: None,
        }
    }

    #[must_use]
    pub const fn status(&self) -> TaskStatus {
        self.status
    }

    #[must_use]
    pub fn reason(&self) -> Option<&StateReason> {
        self.reason.as_ref()
    }

    #[must_use]
    pub const fn blocked_from(&self) -> Option<TaskStatus> {
        self.blocked_from
    }

    /// Rebuilds a state that was previously written to durable storage.
    ///
    /// Persistence layers store the three fields separately so they remain queryable; this
    /// rejects any combination `transition` could not have produced, so a corrupted or
    /// hand-edited row cannot resume as a valid attempt.
    pub fn restore(
        status: TaskStatus,
        reason: Option<StateReason>,
        blocked_from: Option<TaskStatus>,
    ) -> Result<Self, TransitionError> {
        let coherent = match status {
            TaskStatus::Blocked => {
                reason.is_some()
                    && blocked_from.is_some_and(|resume| {
                        resume != TaskStatus::Blocked && !resume.is_terminal()
                    })
            }
            TaskStatus::Cancelled | TaskStatus::Superseded => {
                reason.is_some() && blocked_from.is_none()
            }
            _ => reason.is_none() && blocked_from.is_none(),
        };
        if !coherent {
            return Err(TransitionError::IncoherentPersistedState { status });
        }
        Ok(Self {
            status,
            reason,
            blocked_from,
        })
    }

    pub fn transition(
        &mut self,
        target: TaskStatus,
        reason: Option<StateReason>,
    ) -> Result<(), TransitionError> {
        if self.status.is_terminal() {
            return Err(TransitionError::Terminal {
                current: self.status,
                requested: target,
            });
        }

        if self.status == TaskStatus::Blocked {
            return self.transition_from_blocked(target, reason);
        }

        if target == TaskStatus::Blocked {
            let reason = reason.ok_or(TransitionError::ReasonRequired { target })?;
            self.blocked_from = Some(self.status);
            self.status = target;
            self.reason = Some(reason);
            return Ok(());
        }

        if matches!(target, TaskStatus::Cancelled | TaskStatus::Superseded) {
            let reason = reason.ok_or(TransitionError::ReasonRequired { target })?;
            self.status = target;
            self.reason = Some(reason);
            self.blocked_from = None;
            return Ok(());
        }

        if reason.is_some() {
            return Err(TransitionError::UnexpectedReason { target });
        }

        if self.status.normal_successor() != Some(target) {
            return Err(TransitionError::Invalid {
                current: self.status,
                requested: target,
                expected: self.status.normal_successor(),
            });
        }

        self.status = target;
        self.reason = None;
        self.blocked_from = None;
        Ok(())
    }

    fn transition_from_blocked(
        &mut self,
        target: TaskStatus,
        reason: Option<StateReason>,
    ) -> Result<(), TransitionError> {
        if matches!(target, TaskStatus::Cancelled | TaskStatus::Superseded) {
            let reason = reason.ok_or(TransitionError::ReasonRequired { target })?;
            self.status = target;
            self.reason = Some(reason);
            self.blocked_from = None;
            return Ok(());
        }

        let expected = self
            .blocked_from
            .ok_or(TransitionError::CorruptBlockedState)?;
        if target != expected {
            return Err(TransitionError::InvalidResume {
                requested: target,
                expected,
            });
        }
        if reason.is_some() {
            return Err(TransitionError::UnexpectedReason { target });
        }
        self.status = target;
        self.reason = None;
        self.blocked_from = None;
        Ok(())
    }
}

impl Default for TaskState {
    fn default() -> Self {
        Self::planned()
    }
}

/// Invalid task-state operation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TransitionError {
    #[error("cannot transition terminal state {current:?} to {requested:?}")]
    Terminal {
        current: TaskStatus,
        requested: TaskStatus,
    },
    #[error("cannot transition {current:?} to {requested:?}; expected {expected:?}")]
    Invalid {
        current: TaskStatus,
        requested: TaskStatus,
        expected: Option<TaskStatus>,
    },
    #[error("transition to {target:?} requires a non-empty reason")]
    ReasonRequired { target: TaskStatus },
    #[error("transition to {target:?} must not include a reason")]
    UnexpectedReason { target: TaskStatus },
    #[error("reason message must not be empty")]
    EmptyReason,
    #[error("blocked state is missing its resume state")]
    CorruptBlockedState,
    #[error("persisted {status:?} state carries incoherent reason or resume information")]
    IncoherentPersistedState { status: TaskStatus },
    #[error("cannot resume blocked task at {requested:?}; resume at {expected:?}")]
    InvalidResume {
        requested: TaskStatus,
        expected: TaskStatus,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(code: ReasonCode) -> StateReason {
        StateReason::new(code, "action is required").unwrap()
    }

    #[test]
    fn happy_path_requires_each_durable_step() {
        let mut state = TaskState::planned();
        for expected in [
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
        ] {
            state.transition(expected, None).unwrap();
        }
        assert_eq!(state.status(), TaskStatus::Published);
        assert!(state.status().is_terminal());
    }

    #[test]
    fn skipped_state_is_rejected_with_expected_successor() {
        let mut state = TaskState::planned();
        assert_eq!(
            state.transition(TaskStatus::Captured, None),
            Err(TransitionError::Invalid {
                current: TaskStatus::Planned,
                requested: TaskStatus::Captured,
                expected: Some(TaskStatus::Building),
            })
        );
    }

    #[test]
    fn blocked_state_resumes_at_the_exact_prior_state() {
        let mut state = TaskState::planned();
        state.transition(TaskStatus::Building, None).unwrap();
        state
            .transition(
                TaskStatus::Blocked,
                Some(reason(ReasonCode::AuthenticationRequired)),
            )
            .unwrap();
        assert_eq!(state.blocked_from(), Some(TaskStatus::Building));
        assert!(matches!(
            state.transition(TaskStatus::Captured, None),
            Err(TransitionError::InvalidResume { .. })
        ));
        state.transition(TaskStatus::Building, None).unwrap();
        assert_eq!(state.status(), TaskStatus::Building);
        assert!(state.reason().is_none());
    }

    #[test]
    fn blocking_and_cancellation_require_reasons() {
        let mut state = TaskState::planned();
        assert_eq!(
            state.transition(TaskStatus::Blocked, None),
            Err(TransitionError::ReasonRequired {
                target: TaskStatus::Blocked
            })
        );
        assert_eq!(
            state.transition(TaskStatus::Cancelled, None),
            Err(TransitionError::ReasonRequired {
                target: TaskStatus::Cancelled
            })
        );
    }

    #[test]
    fn restore_accepts_only_states_a_transition_could_have_produced() {
        let mut blocked = TaskState::planned();
        blocked.transition(TaskStatus::Building, None).unwrap();
        blocked
            .transition(
                TaskStatus::Blocked,
                Some(reason(ReasonCode::AuthenticationRequired)),
            )
            .unwrap();
        let rehydrated = TaskState::restore(
            blocked.status(),
            blocked.reason().cloned(),
            blocked.blocked_from(),
        )
        .unwrap();
        assert_eq!(rehydrated, blocked);

        assert_eq!(
            TaskState::restore(
                TaskStatus::Blocked,
                Some(reason(ReasonCode::Conflict)),
                None
            ),
            Err(TransitionError::IncoherentPersistedState {
                status: TaskStatus::Blocked
            })
        );
        assert_eq!(
            TaskState::restore(TaskStatus::PushPending, None, Some(TaskStatus::Verifying)),
            Err(TransitionError::IncoherentPersistedState {
                status: TaskStatus::PushPending
            })
        );
        assert_eq!(
            TaskState::restore(TaskStatus::Cancelled, None, None),
            Err(TransitionError::IncoherentPersistedState {
                status: TaskStatus::Cancelled
            })
        );
    }

    #[test]
    fn publication_stages_exclude_production_and_name_transmitted_pushes() {
        for production in [
            TaskStatus::Planned,
            TaskStatus::Building,
            TaskStatus::Captured,
            TaskStatus::Validated,
            TaskStatus::Queued,
        ] {
            assert!(!production.is_publication_stage(), "{production:?}");
            assert!(!production.may_have_reached_remote(), "{production:?}");
        }
        for publication in [
            TaskStatus::Scheduled,
            TaskStatus::Reconciling,
            TaskStatus::Verifying,
            TaskStatus::CommitPrepared,
            TaskStatus::PushPending,
            TaskStatus::RemoteConfirmed,
            TaskStatus::Published,
            TaskStatus::Blocked,
        ] {
            assert!(publication.is_publication_stage(), "{publication:?}");
        }
        assert!(!TaskStatus::Verifying.may_have_reached_remote());
        assert!(TaskStatus::PushPending.may_have_reached_remote());
        assert!(TaskStatus::Published.may_have_reached_remote());
    }

    #[test]
    fn terminal_state_cannot_be_reopened() {
        let mut state = TaskState::planned();
        state
            .transition(
                TaskStatus::Cancelled,
                Some(reason(ReasonCode::UserRequested)),
            )
            .unwrap();
        assert!(matches!(
            state.transition(TaskStatus::Planned, None),
            Err(TransitionError::Terminal { .. })
        ));
    }
}
