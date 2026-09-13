//! Daemon-owned bridge from persisted scheduling policy to durable queue slots.

use reccursive_core::{PackageId, ReleaseUnitId, Revision, SlotGenerator};
use reccursive_store::{NewScheduleSlot, ScheduleSlot, Store, StoreError};

/// Selects a durable future release time without redrawing an existing choice.
pub struct Scheduler;

impl Scheduler {
    /// Schedules one eligible release unit using its repository's active policy.
    pub fn schedule(
        store: &mut Store,
        release_unit_id: ReleaseUnitId,
        package_id: PackageId,
        package_revision: Revision,
        seed: u64,
        now_unix_ms: i64,
    ) -> Result<ScheduleSlot, SchedulerError> {
        if let Some(slot) = store.schedule_slot(release_unit_id)? {
            return Ok(slot);
        }
        let unit = store
            .release_unit(release_unit_id)?
            .ok_or(SchedulerError::MissingReleaseUnit(release_unit_id))?;
        let plan = store
            .plan(unit.feature_id, Some(unit.plan_revision))?
            .ok_or(SchedulerError::MissingPlan)?;
        let policy = store
            .schedule_policy(plan.plan.repository_id)?
            .ok_or(SchedulerError::MissingPolicy(plan.plan.repository_id))?;
        let existing = store
            .schedule_slots(plan.plan.repository_id)?
            .into_iter()
            .map(|slot| slot.selected_at_unix_ms)
            .collect::<Vec<_>>();
        let selected = SlotGenerator::seeded(seed)
            .select(&policy.policy, now_unix_ms, 1, &existing)?
            .pop()
            .ok_or(SchedulerError::NoSelectableSlot)?;
        Ok(store.persist_schedule_slot(&NewScheduleSlot {
            release_unit_id,
            package_id,
            package_revision,
            policy_revision: policy.revision,
            timezone: selected.timezone,
            eligible_at_unix_ms: now_unix_ms,
            selected_at_unix_ms: selected.selected_at_unix_ms,
            created_at_unix_ms: now_unix_ms,
        })?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Policy(#[from] reccursive_core::SchedulePolicyError),
    #[error("release unit {0} is not stored")]
    MissingReleaseUnit(ReleaseUnitId),
    #[error("release unit plan is not stored")]
    MissingPlan,
    #[error("repository {0} has no active schedule policy")]
    MissingPolicy(reccursive_core::RepositoryId),
    #[error("policy selected no future release slot")]
    NoSelectableSlot,
}
