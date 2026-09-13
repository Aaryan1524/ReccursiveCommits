//! Durable schedule-slot records.
//!
//! A slot is created once for a release unit and its immutable package. The daemon can restart or
//! re-evaluate the queue without changing that selected time; later Phase 4 work explicitly
//! invalidates or reschedules affected slots rather than silently redrawing them.

use std::collections::BTreeSet;

use reccursive_core::{
    IanaTimeZone, PackageId, ReleaseUnitId, Revision, TargetMilestone, TaskStatus,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// A selected, durable UTC release time and the policy context that selected it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleSlot {
    pub release_unit_id: ReleaseUnitId,
    pub repository_id: reccursive_core::RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub policy_revision: Revision,
    pub timezone: IanaTimeZone,
    pub eligible_at_unix_ms: i64,
    pub selected_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
}

/// Why a repository is currently not handing out release work.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPause {
    pub reason: String,
    pub paused_at_unix_ms: i64,
}

/// What an adaptive recalculation actually changed.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRecalculation {
    /// Units whose selected time was withdrawn and which are queued for a fresh selection.
    pub withdrawn: Vec<ReleaseUnitId>,
    /// Units left untouched because an attempt already owns them.
    pub retained: Vec<ReleaseUnitId>,
}

/// One previously selected time and, if it is no longer live, why it was withdrawn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WithdrawnSlot {
    pub selected_at_unix_ms: i64,
    pub invalidated_at_unix_ms: Option<i64>,
    pub invalidated_reason: Option<String>,
}

/// Values required to record one previously selected slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewScheduleSlot {
    pub release_unit_id: ReleaseUnitId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub policy_revision: Revision,
    pub timezone: IanaTimeZone,
    pub eligible_at_unix_ms: i64,
    pub selected_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
}

impl Store {
    /// Loads the durable slot for a release unit, if one was previously selected.
    pub fn schedule_slot(
        &self,
        release_unit_id: ReleaseUnitId,
    ) -> Result<Option<ScheduleSlot>, StoreError> {
        self.connection
            .query_row(
                "SELECT release_unit_id, repository_id, package_id, package_revision,
                        policy_revision, timezone, eligible_at_unix_ms, selected_at_unix_ms,
                        created_at_unix_ms
                 FROM schedule_slots
                 WHERE release_unit_id = ?1 AND invalidated_at_unix_ms IS NULL",
                [release_unit_id.to_string()],
                RawScheduleSlot::from_row,
            )
            .optional()?
            .map(TryInto::try_into)
            .transpose()
    }

    /// Lists retained slots for a repository in execution order.
    pub fn schedule_slots(
        &self,
        repository_id: reccursive_core::RepositoryId,
    ) -> Result<Vec<ScheduleSlot>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT release_unit_id, repository_id, package_id, package_revision,
                    policy_revision, timezone, eligible_at_unix_ms, selected_at_unix_ms,
                    created_at_unix_ms
             FROM schedule_slots
             WHERE repository_id = ?1 AND invalidated_at_unix_ms IS NULL
             ORDER BY selected_at_unix_ms, release_unit_id",
        )?;
        statement
            .query_map([repository_id.to_string()], RawScheduleSlot::from_row)?
            .map(|row| row.map_err(StoreError::from)?.try_into())
            .collect()
    }

    /// Persists one selected time and moves every task in the unit from `queued` to `scheduled`.
    ///
    /// Calling this after a restart returns the existing record without changing it. A unit's
    /// package must deliver exactly its tasks, and every published-milestone prerequisite must
    /// already be confirmed, so a slot never turns an incomplete dependency chain into a release.
    pub fn persist_schedule_slot(
        &mut self,
        slot: &NewScheduleSlot,
    ) -> Result<ScheduleSlot, StoreError> {
        validate(slot)?;
        if let Some(existing) = self.schedule_slot(slot.release_unit_id)? {
            return Ok(existing);
        }
        let unit = self
            .release_unit(slot.release_unit_id)?
            .ok_or_else(|| StoreError::InvalidData("release unit is not stored".into()))?;
        let snapshot = self
            .snapshot(slot.package_id, slot.package_revision)?
            .ok_or_else(|| StoreError::InvalidData("snapshot package is not stored".into()))?;
        if snapshot.feature_id != unit.feature_id || snapshot.plan_revision != unit.plan_revision {
            return Err(StoreError::Conflict(
                "scheduled package and release unit belong to different plan revisions".into(),
            ));
        }
        if self
            .package_task_ids(slot.package_id, slot.package_revision)?
            .into_iter()
            .collect::<BTreeSet<_>>()
            != unit.task_ids
        {
            return Err(StoreError::Conflict(
                "scheduled package must deliver exactly the release unit's tasks".into(),
            ));
        }
        let plan = self
            .plan(unit.feature_id, Some(unit.plan_revision))?
            .ok_or_else(|| StoreError::InvalidData("release unit plan is not stored".into()))?;
        self.ensure_unit_is_eligible(&unit, &plan.plan.repository_id)?;

        let transaction = self.connection.transaction()?;
        transaction
            .execute(
                "INSERT INTO schedule_slots (
                release_unit_id, repository_id, package_id, package_revision, policy_revision,
                timezone, eligible_at_unix_ms, selected_at_unix_ms, created_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    slot.release_unit_id.to_string(),
                    plan.plan.repository_id.to_string(),
                    slot.package_id.to_string(),
                    slot.package_revision.get(),
                    slot.policy_revision.get(),
                    slot.timezone.as_str(),
                    slot.eligible_at_unix_ms,
                    slot.selected_at_unix_ms,
                    slot.created_at_unix_ms,
                ],
            )
            .map_err(map_schedule_write_error)?;
        for task_id in &unit.task_ids {
            let changed = transaction.execute(
                "UPDATE plan_tasks
                 SET status = 'scheduled', updated_at_unix_ms = ?4
                 WHERE feature_id = ?1 AND plan_revision = ?2 AND task_id = ?3
                   AND status = 'queued' AND reason_json IS NULL AND blocked_from IS NULL",
                params![
                    unit.feature_id.to_string(),
                    unit.plan_revision.get(),
                    task_id.to_string(),
                    slot.created_at_unix_ms,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict(format!(
                    "task {task_id} is no longer queued; schedule was not recorded"
                )));
            }
        }
        transaction.commit()?;
        self.schedule_slot(slot.release_unit_id)?.ok_or_else(|| {
            StoreError::Conflict("schedule slot disappeared after it was committed".into())
        })
    }

    /// Withdraws a unit's selected time so the work returns to the queue for a fresh selection.
    ///
    /// The slot row is kept and marked, never deleted: what was selected and why it stopped being
    /// valid is exactly the evidence that makes an adaptive reschedule auditable.
    ///
    /// Refused once any task in the unit has moved past `scheduled`. At that point an attempt owns
    /// the work and its identity, and a schedule change must not reach in and discard it.
    pub fn invalidate_schedule_slot(
        &mut self,
        release_unit_id: ReleaseUnitId,
        reason: &str,
        now_unix_ms: i64,
    ) -> Result<Option<ScheduleSlot>, StoreError> {
        if reason.trim().is_empty() || reason.len() > 256 {
            return Err(StoreError::InvalidData(
                "schedule invalidation reason must be a short non-empty explanation".into(),
            ));
        }
        let Some(slot) = self.schedule_slot(release_unit_id)? else {
            return Ok(None);
        };
        let unit = self
            .release_unit(release_unit_id)?
            .ok_or_else(|| StoreError::InvalidData("release unit is not stored".into()))?;

        let mut withdrawn = Vec::new();
        for task_id in &unit.task_ids {
            let task = self
                .task(unit.feature_id, unit.plan_revision, *task_id)?
                .ok_or_else(|| {
                    StoreError::InvalidData(format!("release-unit task {task_id} is missing"))
                })?;
            match task.state.status() {
                TaskStatus::Scheduled => {
                    let mut state = task.state.clone();
                    state
                        .withdraw_schedule()
                        .map_err(|error| StoreError::Conflict(error.to_string()))?;
                    withdrawn.push(*task_id);
                }
                // Terminal work keeps its outcome, and blocked work keeps the reason it stopped;
                // neither is future work a reschedule should disturb.
                status if status.is_terminal() || status == TaskStatus::Blocked => {}
                status => {
                    return Err(StoreError::Conflict(format!(
                        "task {task_id} is {status:?}; a live attempt owns this unit and its \
                         schedule cannot be withdrawn"
                    )));
                }
            }
        }

        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE schedule_slots
             SET invalidated_at_unix_ms = ?2, invalidated_reason = ?3
             WHERE release_unit_id = ?1 AND invalidated_at_unix_ms IS NULL",
            params![release_unit_id.to_string(), now_unix_ms, reason],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "schedule slot changed concurrently; re-read it before retrying".into(),
            ));
        }
        for task_id in &withdrawn {
            let updated = transaction.execute(
                "UPDATE plan_tasks
                 SET status = 'queued', updated_at_unix_ms = ?4
                 WHERE feature_id = ?1 AND plan_revision = ?2 AND task_id = ?3
                   AND status = 'scheduled'",
                params![
                    unit.feature_id.to_string(),
                    unit.plan_revision.get(),
                    task_id.to_string(),
                    now_unix_ms,
                ],
            )?;
            if updated != 1 {
                return Err(StoreError::Conflict(format!(
                    "task {task_id} left the scheduled state while its slot was being withdrawn"
                )));
            }
        }
        transaction.commit()?;
        Ok(Some(slot))
    }

    /// Withdraws every live slot for a repository, for use when its policy changes.
    ///
    /// Units with a live attempt are left alone and reported, rather than failing the whole
    /// recalculation: a settings change must not be able to disturb work already in flight.
    pub fn invalidate_repository_schedule(
        &mut self,
        repository_id: reccursive_core::RepositoryId,
        reason: &str,
        now_unix_ms: i64,
    ) -> Result<ScheduleRecalculation, StoreError> {
        let mut outcome = ScheduleRecalculation::default();
        for slot in self.schedule_slots(repository_id)? {
            match self.invalidate_schedule_slot(slot.release_unit_id, reason, now_unix_ms) {
                Ok(Some(_)) => outcome.withdrawn.push(slot.release_unit_id),
                Ok(None) => {}
                Err(StoreError::Conflict(_)) => outcome.retained.push(slot.release_unit_id),
                Err(error) => return Err(error),
            }
        }
        Ok(outcome)
    }

    /// Stops a repository from handing out due work until it is explicitly resumed.
    ///
    /// Pausing governs what is *started*, never what is already running. An attempt that has
    /// transmitted a push has an outcome on the remote that must still be resolved, and pausing
    /// must not orphan it — so this records the decision and leaves in-flight attempts alone.
    pub fn pause_repository(
        &mut self,
        repository_id: reccursive_core::RepositoryId,
        reason: &str,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        if reason.trim().is_empty() || reason.len() > 256 {
            return Err(StoreError::InvalidData(
                "pause reason must be a short non-empty explanation".into(),
            ));
        }
        self.connection
            .execute(
                "INSERT INTO repository_pauses (repository_id, reason, paused_at_unix_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(repository_id) DO UPDATE SET
                     reason = excluded.reason,
                     paused_at_unix_ms = excluded.paused_at_unix_ms",
                params![repository_id.to_string(), reason, now_unix_ms],
            )
            .map_err(map_schedule_write_error)?;
        Ok(())
    }

    /// Resumes a paused repository. Resuming one that is not paused is not an error.
    pub fn resume_repository(
        &mut self,
        repository_id: reccursive_core::RepositoryId,
    ) -> Result<bool, StoreError> {
        let changed = self.connection.execute(
            "DELETE FROM repository_pauses WHERE repository_id = ?1",
            [repository_id.to_string()],
        )?;
        Ok(changed == 1)
    }

    /// Returns why a repository is paused, if it is.
    pub fn repository_pause(
        &self,
        repository_id: reccursive_core::RepositoryId,
    ) -> Result<Option<RepositoryPause>, StoreError> {
        self.connection
            .query_row(
                "SELECT reason, paused_at_unix_ms FROM repository_pauses WHERE repository_id = ?1",
                [repository_id.to_string()],
                |row| {
                    Ok(RepositoryPause {
                        reason: row.get(0)?,
                        paused_at_unix_ms: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Moves a unit's selected time to now so it is due immediately.
    ///
    /// This is a schedule override, not a dependency override: the same eligibility rules that
    /// govern an ordinary selection are applied again, so a unit whose prerequisites have not
    /// reached their required milestone is refused rather than released early.
    pub fn release_unit_now(
        &mut self,
        release_unit_id: ReleaseUnitId,
        now_unix_ms: i64,
    ) -> Result<ScheduleSlot, StoreError> {
        let existing = self.schedule_slot(release_unit_id)?.ok_or_else(|| {
            StoreError::InvalidData("release unit has no live schedule slot".into())
        })?;
        self.invalidate_schedule_slot(release_unit_id, "released on request", now_unix_ms)?;
        self.persist_schedule_slot(&NewScheduleSlot {
            release_unit_id,
            package_id: existing.package_id,
            package_revision: existing.package_revision,
            policy_revision: existing.policy_revision,
            timezone: existing.timezone,
            eligible_at_unix_ms: now_unix_ms,
            selected_at_unix_ms: now_unix_ms,
            created_at_unix_ms: now_unix_ms,
        })
    }

    /// Lists live slots whose selected time has already passed, oldest first.
    ///
    /// These are the missed windows: work that should have been released while the machine was
    /// asleep, offline, or simply not running.
    pub fn overdue_schedule_slots(
        &self,
        repository_id: reccursive_core::RepositoryId,
        now_unix_ms: i64,
    ) -> Result<Vec<ScheduleSlot>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT release_unit_id, repository_id, package_id, package_revision,
                    policy_revision, timezone, eligible_at_unix_ms, selected_at_unix_ms,
                    created_at_unix_ms
             FROM schedule_slots
             WHERE repository_id = ?1 AND invalidated_at_unix_ms IS NULL
               AND selected_at_unix_ms <= ?2
             ORDER BY selected_at_unix_ms, release_unit_id",
        )?;
        statement
            .query_map(
                params![repository_id.to_string(), now_unix_ms],
                RawScheduleSlot::from_row,
            )?
            .map(|row| row.map_err(StoreError::from)?.try_into())
            .collect()
    }

    /// Lists every slot ever selected for a unit, including withdrawn ones, newest first.
    pub fn schedule_slot_history(
        &self,
        release_unit_id: ReleaseUnitId,
    ) -> Result<Vec<WithdrawnSlot>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT selected_at_unix_ms, invalidated_at_unix_ms, invalidated_reason
             FROM schedule_slots WHERE release_unit_id = ?1
             ORDER BY created_at_unix_ms DESC",
        )?;
        statement
            .query_map([release_unit_id.to_string()], |row| {
                Ok(WithdrawnSlot {
                    selected_at_unix_ms: row.get(0)?,
                    invalidated_at_unix_ms: row.get(1)?,
                    invalidated_reason: row.get(2)?,
                })
            })?
            .map(|row| row.map_err(StoreError::from))
            .collect()
    }

    fn ensure_unit_is_eligible(
        &self,
        unit: &crate::ReleaseUnitRecord,
        repository_id: &reccursive_core::RepositoryId,
    ) -> Result<(), StoreError> {
        for task_id in &unit.task_ids {
            let task = self
                .task(unit.feature_id, unit.plan_revision, *task_id)?
                .ok_or_else(|| {
                    StoreError::InvalidData(format!("release-unit task {task_id} is missing"))
                })?;
            if task.state.status() != TaskStatus::Queued {
                return Err(StoreError::Conflict(format!(
                    "task {task_id} is {:?}, not queued",
                    task.state.status()
                )));
            }
            for (prerequisite, milestone) in
                self.task_prerequisites(unit.feature_id, unit.plan_revision, *task_id)?
            {
                if unit.task_ids.contains(&prerequisite) {
                    continue;
                }
                let dependency = self
                    .task(unit.feature_id, unit.plan_revision, prerequisite)?
                    .ok_or_else(|| {
                        StoreError::InvalidData(format!("prerequisite {prerequisite} is missing"))
                    })?;
                let eligible = match milestone {
                    TargetMilestone::Captured => {
                        dependency.state.status() != TaskStatus::Planned
                            && dependency.state.status() != TaskStatus::Building
                    }
                    TargetMilestone::TargetPublished => {
                        dependency.state.status() == TaskStatus::Published
                    }
                    TargetMilestone::DevelopmentAvailable => false,
                };
                if !eligible {
                    return Err(StoreError::Conflict(format!(
                        "task {task_id} waits for prerequisite {prerequisite} at {milestone:?}"
                    )));
                }
            }
        }
        if self.repository(*repository_id)?.is_none() {
            return Err(StoreError::InvalidData(
                "scheduled repository is not enrolled".into(),
            ));
        }
        Ok(())
    }
}

fn validate(slot: &NewScheduleSlot) -> Result<(), StoreError> {
    if slot.eligible_at_unix_ms < 0
        || slot.selected_at_unix_ms < slot.eligible_at_unix_ms
        || slot.created_at_unix_ms < 0
    {
        return Err(StoreError::InvalidData(
            "schedule timestamps must be non-negative and selected time must not precede eligibility".into(),
        ));
    }
    Ok(())
}

fn map_schedule_write_error(error: rusqlite::Error) -> StoreError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        StoreError::Conflict(format!("schedule slot rejected by storage rules: {error}"))
    } else {
        StoreError::Sqlite(error)
    }
}

struct RawScheduleSlot {
    release_unit_id: String,
    repository_id: String,
    package_id: String,
    package_revision: u32,
    policy_revision: u32,
    timezone: String,
    eligible_at_unix_ms: i64,
    selected_at_unix_ms: i64,
    created_at_unix_ms: i64,
}

impl RawScheduleSlot {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            release_unit_id: row.get(0)?,
            repository_id: row.get(1)?,
            package_id: row.get(2)?,
            package_revision: row.get(3)?,
            policy_revision: row.get(4)?,
            timezone: row.get(5)?,
            eligible_at_unix_ms: row.get(6)?,
            selected_at_unix_ms: row.get(7)?,
            created_at_unix_ms: row.get(8)?,
        })
    }
}

impl TryFrom<RawScheduleSlot> for ScheduleSlot {
    type Error = StoreError;

    fn try_from(value: RawScheduleSlot) -> Result<Self, Self::Error> {
        Ok(Self {
            release_unit_id: value.release_unit_id.parse().map_err(invalid_id)?,
            repository_id: value.repository_id.parse().map_err(invalid_id)?,
            package_id: value.package_id.parse().map_err(invalid_id)?,
            package_revision: Revision::new(value.package_revision)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            policy_revision: Revision::new(value.policy_revision)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            timezone: IanaTimeZone::new(value.timezone)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            eligible_at_unix_ms: value.eligible_at_unix_ms,
            selected_at_unix_ms: value.selected_at_unix_ms,
            created_at_unix_ms: value.created_at_unix_ms,
        })
    }
}

fn invalid_id(error: reccursive_core::IdParseError) -> StoreError {
    StoreError::InvalidData(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
    };

    use reccursive_core::{
        AcceptanceCheck, FeatureId, FeaturePlan, IanaTimeZone, PLAN_SCHEMA_VERSION, PackageId,
        PlanPhase, PlanTask, PublicationMode, RepositoryId, RepositoryPolicy, TargetRef, TaskId,
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::{RepositoryRegistration, SnapshotRecord, WorkspaceRecord};

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn setup(store: &mut Store) -> (ReleaseUnitId, PackageId, FeatureId, TaskId, RepositoryId) {
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/schedule-source",
                    "ssh://git@example.invalid/schedule.git",
                    "/tmp/schedule-managed.git",
                    1,
                )
                .unwrap(),
                &RepositoryPolicy::new(
                    repository_id,
                    Revision::FIRST,
                    PublicationMode::ScheduledCreation,
                    target.clone(),
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
                    goal: "Schedule a verified change".into(),
                    target,
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: vec![PlanTask {
                            id: task_id,
                            name: "Add the change".into(),
                            dependencies: BTreeMap::new(),
                            acceptance_checks: vec![AcceptanceCheck {
                                id: "done".into(),
                                description: "The change is ready".into(),
                            }],
                        }],
                    }],
                },
                1,
            )
            .unwrap();
        store
            .record_workspace(&WorkspaceRecord {
                feature_id,
                revision: Revision::FIRST,
                path: "/tmp/schedule-workspace".into(),
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
                path: PathBuf::from("/tmp/schedule-package"),
                base_tree: object_id('a'),
                result_tree: object_id('b'),
                content_hash: std::iter::repeat_n('c', 64).collect(),
                parent_package_id: None,
                manifest: json!({"paths": []}),
                created_at_unix_ms: 2,
            })
            .unwrap();
        store
            .record_package_tasks(package_id, Revision::FIRST, [task_id])
            .unwrap();
        store
            .advance_task_to(feature_id, Revision::FIRST, task_id, TaskStatus::Queued, 3)
            .unwrap();
        let unit_id = ReleaseUnitId::new();
        store
            .create_release_unit(
                unit_id,
                feature_id,
                Revision::FIRST,
                BTreeSet::from([task_id]),
                BTreeSet::new(),
                4,
            )
            .unwrap();
        (unit_id, package_id, feature_id, task_id, repository_id)
    }

    #[test]
    fn persisted_slot_survives_restart_without_redrawing_and_schedules_its_task() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("queue.sqlite");
        let (unit_id, package_id, feature_id, task_id, repository_id) = {
            let mut store = Store::open(&database).unwrap();
            let values = setup(&mut store);
            let created = store
                .persist_schedule_slot(&NewScheduleSlot {
                    release_unit_id: values.0,
                    package_id: values.1,
                    package_revision: Revision::FIRST,
                    policy_revision: Revision::FIRST,
                    timezone: IanaTimeZone::new("America/New_York").unwrap(),
                    eligible_at_unix_ms: 10,
                    selected_at_unix_ms: 20,
                    created_at_unix_ms: 10,
                })
                .unwrap();
            assert_eq!(created.selected_at_unix_ms, 20);
            assert_eq!(
                store
                    .task(values.2, Revision::FIRST, values.3)
                    .unwrap()
                    .unwrap()
                    .state
                    .status(),
                TaskStatus::Scheduled
            );
            values
        };

        let mut restarted = Store::open(&database).unwrap();
        let retained = restarted.schedule_slot(unit_id).unwrap().unwrap();
        assert_eq!(retained.selected_at_unix_ms, 20);
        assert_eq!(
            restarted.schedule_slots(repository_id).unwrap(),
            vec![retained.clone()]
        );
        let idempotent = restarted
            .persist_schedule_slot(&NewScheduleSlot {
                release_unit_id: unit_id,
                package_id,
                package_revision: Revision::FIRST,
                policy_revision: Revision::FIRST,
                timezone: IanaTimeZone::new("America/New_York").unwrap(),
                eligible_at_unix_ms: 10,
                selected_at_unix_ms: 999,
                created_at_unix_ms: 10,
            })
            .unwrap();
        assert_eq!(idempotent, retained);
        assert_eq!(
            restarted
                .task(feature_id, Revision::FIRST, task_id)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Scheduled
        );
    }
}

#[cfg(test)]
mod recalculation_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use reccursive_core::{
        AcceptanceCheck, FeatureId, FeaturePlan, IanaTimeZone, PLAN_SCHEMA_VERSION, PackageId,
        PlanPhase, PlanTask, PublicationMode, RepositoryId, RepositoryPolicy, TargetRef, TaskId,
    };
    use serde_json::json;

    use super::*;
    use crate::{RepositoryRegistration, SnapshotRecord, WorkspaceRecord};

    struct Fixture {
        store: Store,
        repository_id: RepositoryId,
        feature_id: FeatureId,
        units: Vec<(ReleaseUnitId, PackageId, TaskId)>,
    }

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    /// Two independent units in one repository, both scheduled. Independence is the point: it is
    /// what makes "only affected future work moved" a testable claim rather than an assertion.
    fn fixture() -> Fixture {
        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/recalc-source",
                    "ssh://git@example.invalid/recalc.git",
                    "/tmp/recalc-managed.git",
                    1,
                )
                .unwrap(),
                &RepositoryPolicy::new(
                    repository_id,
                    Revision::FIRST,
                    PublicationMode::ScheduledCreation,
                    target.clone(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();

        let feature_id = FeatureId::new();
        let task_ids: Vec<TaskId> = (0..2).map(|_| TaskId::new()).collect();
        store
            .import_plan(
                &FeaturePlan {
                    schema_version: PLAN_SCHEMA_VERSION,
                    feature_id,
                    revision: Revision::FIRST,
                    repository_id,
                    goal: "Two independent units".into(),
                    target,
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: task_ids
                            .iter()
                            .enumerate()
                            .map(|(index, id)| PlanTask {
                                id: *id,
                                name: format!("Task {index}"),
                                dependencies: BTreeMap::new(),
                                acceptance_checks: vec![AcceptanceCheck {
                                    id: "done".into(),
                                    description: "ready".into(),
                                }],
                            })
                            .collect(),
                    }],
                },
                1,
            )
            .unwrap();
        store
            .record_workspace(&WorkspaceRecord {
                feature_id,
                revision: Revision::FIRST,
                path: "/tmp/recalc-workspace".into(),
                base_commit: object_id('a'),
                prerequisites: json!([]),
                created_at_unix_ms: 1,
            })
            .unwrap();

        let mut units = Vec::new();
        for (index, task_id) in task_ids.iter().enumerate() {
            let name = char::from(b'b' + u8::try_from(index).unwrap());
            let package_id = PackageId::new();
            store
                .record_snapshot(&SnapshotRecord {
                    package_id,
                    revision: Revision::FIRST,
                    feature_id,
                    plan_revision: Revision::FIRST,
                    path: std::path::PathBuf::from(format!("/tmp/recalc-package-{name}")),
                    base_tree: object_id('a'),
                    result_tree: object_id(name),
                    content_hash: std::iter::repeat_n(name, 64).collect(),
                    parent_package_id: None,
                    manifest: json!({"paths": []}),
                    created_at_unix_ms: 2,
                })
                .unwrap();
            store
                .record_package_tasks(package_id, Revision::FIRST, [*task_id])
                .unwrap();
            store
                .advance_task_to(feature_id, Revision::FIRST, *task_id, TaskStatus::Queued, 3)
                .unwrap();
            let unit_id = ReleaseUnitId::new();
            store
                .create_release_unit(
                    unit_id,
                    feature_id,
                    Revision::FIRST,
                    BTreeSet::from([*task_id]),
                    BTreeSet::new(),
                    4,
                )
                .unwrap();
            store
                .persist_schedule_slot(&NewScheduleSlot {
                    release_unit_id: unit_id,
                    package_id,
                    package_revision: Revision::FIRST,
                    policy_revision: Revision::FIRST,
                    timezone: IanaTimeZone::new("UTC").unwrap(),
                    eligible_at_unix_ms: 10,
                    selected_at_unix_ms: 100 + i64::try_from(index).unwrap(),
                    created_at_unix_ms: 10,
                })
                .unwrap();
            units.push((unit_id, package_id, *task_id));
        }

        Fixture {
            store,
            repository_id,
            feature_id,
            units,
        }
    }

    #[test]
    fn cancelling_a_task_withdraws_only_its_own_units_schedule() {
        let mut fixture = fixture();
        let (first_unit, _, first_task) = fixture.units[0];
        let (second_unit, _, second_task) = fixture.units[1];
        let untouched_before = fixture
            .store
            .schedule_slot(second_unit)
            .unwrap()
            .unwrap()
            .selected_at_unix_ms;

        let outcome = fixture
            .store
            .cancel_task(
                fixture.feature_id,
                Revision::FIRST,
                first_task,
                "no longer needed",
                50,
            )
            .unwrap();

        assert_eq!(outcome.withdrawn_schedules, vec![first_unit]);
        assert!(
            fixture.store.schedule_slot(first_unit).unwrap().is_none(),
            "a cancelled unit must not keep a live release time"
        );

        // The independent unit is the control: nothing about it may move.
        let untouched_after = fixture
            .store
            .schedule_slot(second_unit)
            .unwrap()
            .unwrap()
            .selected_at_unix_ms;
        assert_eq!(untouched_after, untouched_before);
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, second_task)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Scheduled
        );
    }

    #[test]
    fn a_withdrawn_unit_returns_to_the_queue_and_can_be_scheduled_again() {
        let mut fixture = fixture();
        let (unit_id, package_id, task_id) = fixture.units[0];

        let withdrawn = fixture
            .store
            .invalidate_schedule_slot(unit_id, "policy changed", 50)
            .unwrap();
        assert_eq!(withdrawn.unwrap().selected_at_unix_ms, 100);
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, task_id)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Queued,
            "withdrawing a slot must return the work to the queue, not strand it"
        );

        // The package is not permanently bound to its withdrawn slot.
        let rescheduled = fixture
            .store
            .persist_schedule_slot(&NewScheduleSlot {
                release_unit_id: unit_id,
                package_id,
                package_revision: Revision::FIRST,
                policy_revision: Revision::new(2).unwrap(),
                timezone: IanaTimeZone::new("UTC").unwrap(),
                eligible_at_unix_ms: 50,
                selected_at_unix_ms: 500,
                created_at_unix_ms: 50,
            })
            .unwrap();
        assert_eq!(rescheduled.selected_at_unix_ms, 500);

        // Both the withdrawn selection and the new one remain auditable.
        let history = fixture.store.schedule_slot_history(unit_id).unwrap();
        assert_eq!(history.len(), 2);
        assert!(
            history
                .iter()
                .any(|slot| slot.invalidated_reason.as_deref() == Some("policy changed"))
        );
    }

    #[test]
    fn a_live_attempt_keeps_its_schedule_through_a_repository_recalculation() {
        let mut fixture = fixture();
        let (live_unit, _, live_task) = fixture.units[0];
        let (queued_unit, _, _) = fixture.units[1];

        // This unit's attempt has started; its identity is no longer the schedule's to discard.
        fixture
            .store
            .advance_task(
                fixture.feature_id,
                Revision::FIRST,
                live_task,
                TaskStatus::Reconciling,
                None,
                60,
            )
            .unwrap();

        let outcome = fixture
            .store
            .invalidate_repository_schedule(fixture.repository_id, "policy revised", 70)
            .unwrap();

        assert_eq!(outcome.retained, vec![live_unit]);
        assert_eq!(outcome.withdrawn, vec![queued_unit]);
        assert!(
            fixture.store.schedule_slot(live_unit).unwrap().is_some(),
            "a unit with a live attempt must keep its slot"
        );
        assert!(fixture.store.schedule_slot(queued_unit).unwrap().is_none());
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, live_task)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Reconciling,
            "a settings change must not reach into work already in flight"
        );
    }
}

/// Simultaneous-client behaviour, exercised through two independent connections to one database
/// rather than one handle used twice. These are the races the leases and partial unique indexes
/// exist for: the guarantee is that a loser is told it lost, never that both sides quietly win.
#[cfg(test)]
mod concurrency_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use reccursive_core::{
        AcceptanceCheck, AttemptId, FeatureId, FeaturePlan, IanaTimeZone, PLAN_SCHEMA_VERSION,
        PackageId, PlanPhase, PlanTask, PublicationMode, RepositoryId, RepositoryPolicy, TargetRef,
        TaskId,
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        AttemptLease, NewReleaseAttempt, RepositoryRegistration, SnapshotRecord, WorkspaceRecord,
    };

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    /// Seeds a database on disk and returns the identifiers two clients will contend over.
    fn seed(path: &std::path::Path) -> (RepositoryId, ReleaseUnitId, PackageId, TargetRef) {
        let mut store = Store::open(path).unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/race-source",
                    "ssh://git@example.invalid/race.git",
                    "/tmp/race-managed.git",
                    1,
                )
                .unwrap(),
                &RepositoryPolicy::new(
                    repository_id,
                    Revision::FIRST,
                    PublicationMode::ScheduledCreation,
                    target.clone(),
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
                    goal: "Contended work".into(),
                    target: target.clone(),
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: vec![PlanTask {
                            id: task_id,
                            name: "The task".into(),
                            dependencies: BTreeMap::new(),
                            acceptance_checks: vec![AcceptanceCheck {
                                id: "done".into(),
                                description: "ready".into(),
                            }],
                        }],
                    }],
                },
                1,
            )
            .unwrap();
        store
            .record_workspace(&WorkspaceRecord {
                feature_id,
                revision: Revision::FIRST,
                path: "/tmp/race-workspace".into(),
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
                path: "/tmp/race-package".into(),
                base_tree: object_id('a'),
                result_tree: object_id('b'),
                content_hash: std::iter::repeat_n('c', 64).collect(),
                parent_package_id: None,
                manifest: json!({"paths": []}),
                created_at_unix_ms: 2,
            })
            .unwrap();
        store
            .record_package_tasks(package_id, Revision::FIRST, [task_id])
            .unwrap();
        store
            .advance_task_to(feature_id, Revision::FIRST, task_id, TaskStatus::Queued, 3)
            .unwrap();
        let unit_id = ReleaseUnitId::new();
        store
            .create_release_unit(
                unit_id,
                feature_id,
                Revision::FIRST,
                BTreeSet::from([task_id]),
                BTreeSet::new(),
                4,
            )
            .unwrap();
        (repository_id, unit_id, package_id, target)
    }

    fn slot(unit_id: ReleaseUnitId, package_id: PackageId, selected: i64) -> NewScheduleSlot {
        NewScheduleSlot {
            release_unit_id: unit_id,
            package_id,
            package_revision: Revision::FIRST,
            policy_revision: Revision::FIRST,
            timezone: IanaTimeZone::new("UTC").unwrap(),
            eligible_at_unix_ms: 10,
            selected_at_unix_ms: selected,
            created_at_unix_ms: 10,
        }
    }

    #[test]
    fn two_clients_scheduling_one_unit_produce_exactly_one_release_time() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("queue.sqlite");
        let (repository_id, unit_id, package_id, _) = seed(&database);

        let mut first = Store::open(&database).unwrap();
        let mut second = Store::open(&database).unwrap();

        // Both clients believe they are scheduling this unit, and each proposes a different time.
        let a = first
            .persist_schedule_slot(&slot(unit_id, package_id, 1_000))
            .unwrap();
        let b = second
            .persist_schedule_slot(&slot(unit_id, package_id, 9_999))
            .unwrap();

        // One selection exists and both clients are told the same thing. The second proposal does
        // not win, and does not silently create a rival slot.
        assert_eq!(a, b);
        assert_eq!(a.selected_at_unix_ms, 1_000);
        assert_eq!(first.schedule_slots(repository_id).unwrap().len(), 1);
    }

    #[test]
    fn two_clients_claiming_one_target_cannot_both_hold_a_release_lease() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("queue.sqlite");
        let (repository_id, unit_id, package_id, target) = seed(&database);

        let mut first = Store::open(&database).unwrap();
        let mut second = Store::open(&database).unwrap();
        first
            .persist_schedule_slot(&slot(unit_id, package_id, 1_000))
            .unwrap();

        let request = |owner: &str| NewReleaseAttempt {
            attempt_id: AttemptId::new(),
            repository_id,
            package_id,
            package_revision: Revision::FIRST,
            remote: "ssh://git@example.invalid/race.git".into(),
            target: target.clone(),
            base_commit: object_id('a'),
            lease: AttemptLease {
                owner: owner.into(),
                expires_at_unix_ms: 100_000,
            },
            created_at_unix_ms: 10,
        };

        first.open_release_attempt(&request("client-a")).unwrap();
        let contended = second.open_release_attempt(&request("client-b"));

        assert!(
            matches!(contended, Err(StoreError::Conflict(_))),
            "a second client must be refused the same remote and target, not allowed to race \
             for it: {contended:?}"
        );
    }

    #[test]
    fn two_clients_withdrawing_one_selection_do_not_both_report_success() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("queue.sqlite");
        let (_, unit_id, package_id, _) = seed(&database);

        let mut first = Store::open(&database).unwrap();
        let mut second = Store::open(&database).unwrap();
        first
            .persist_schedule_slot(&slot(unit_id, package_id, 1_000))
            .unwrap();

        let a = first
            .invalidate_schedule_slot(unit_id, "first client", 50)
            .unwrap();
        let b = second
            .invalidate_schedule_slot(unit_id, "second client", 50)
            .unwrap();

        assert!(a.is_some(), "the first withdrawal should take effect");
        assert!(
            b.is_none(),
            "the second client must observe that there is nothing left to withdraw"
        );
        // Exactly one withdrawal is recorded, with the reason that actually applied.
        let history = second.schedule_slot_history(unit_id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].invalidated_reason.as_deref(),
            Some("first client")
        );
    }
}
