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
                 FROM schedule_slots WHERE release_unit_id = ?1",
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
             FROM schedule_slots WHERE repository_id = ?1
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
