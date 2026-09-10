//! Durable task state and dependencies, projected from the immutable plan document.
//!
//! `feature_plans.document_json` stays the authoritative record of what was imported. These rows
//! exist so the two questions everything downstream is built on — what depends on a task, and what
//! state a task is in — can be answered by a query rather than by parsing a blob.

use std::{collections::BTreeMap, str::FromStr};

use reccursive_core::{
    FeatureId, FeaturePlan, PackageId, Revision, StateReason, TargetMilestone, TaskId, TaskState,
    TaskStatus,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::{
    Store, StoreError,
    states::{encode_reason, restore_state, status_column},
};

/// One task of a plan revision, with its durable lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_id: TaskId,
    pub name: String,
    pub state: TaskState,
    pub updated_at_unix_ms: i64,
}

/// A prerequisite edge and the milestone the prerequisite must reach.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskDependency {
    pub task_id: TaskId,
    pub depends_on: TaskId,
    pub required_milestone: TargetMilestone,
}

const fn milestone_column(milestone: TargetMilestone) -> &'static str {
    match milestone {
        TargetMilestone::Captured => "captured",
        TargetMilestone::DevelopmentAvailable => "development_available",
        TargetMilestone::TargetPublished => "target_published",
    }
}

fn milestone_from_column(value: &str) -> Result<TargetMilestone, StoreError> {
    match value {
        "captured" => Ok(TargetMilestone::Captured),
        "development_available" => Ok(TargetMilestone::DevelopmentAvailable),
        "target_published" => Ok(TargetMilestone::TargetPublished),
        other => Err(StoreError::InvalidData(format!(
            "stored dependency milestone {other} is not defined"
        ))),
    }
}

const TASK_COLUMNS: &str = "feature_id, plan_revision, task_id, name, status, reason_json, blocked_from, \
     updated_at_unix_ms";

fn task_from_row(row: &rusqlite::Row<'_>) -> Result<TaskRecord, rusqlite::Error> {
    let convert = |column: usize, error: StoreError| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    };
    let feature_id = FeatureId::from_str(&row.get::<_, String>(0)?)
        .map_err(|error| convert(0, StoreError::InvalidData(error.to_string())))?;
    let plan_revision = Revision::new(row.get::<_, u32>(1)?)
        .map_err(|error| convert(1, StoreError::InvalidData(error.to_string())))?;
    let task_id = TaskId::from_str(&row.get::<_, String>(2)?)
        .map_err(|error| convert(2, StoreError::InvalidData(error.to_string())))?;
    let state = restore_state(&row.get::<_, String>(4)?, row.get(5)?, row.get(6)?)
        .map_err(|error| convert(4, error))?;
    Ok(TaskRecord {
        feature_id,
        plan_revision,
        task_id,
        name: row.get(3)?,
        state,
        updated_at_unix_ms: row.get(7)?,
    })
}

/// Writes a plan's tasks and dependency edges inside the caller's plan-import transaction.
///
/// Import already rejects cycles and unknown prerequisites through the domain plan validator, so
/// these rows never need to re-derive the graph; they only make it queryable.
pub(crate) fn insert_plan_tasks(
    transaction: &Transaction<'_>,
    plan: &FeaturePlan,
    created_at_unix_ms: i64,
) -> Result<(), StoreError> {
    for task in plan.phases.iter().flat_map(|phase| phase.tasks.iter()) {
        transaction.execute(
            "INSERT INTO plan_tasks (
                feature_id, plan_revision, task_id, name, status, updated_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, 'planned', ?5)",
            params![
                plan.feature_id.to_string(),
                plan.revision.get(),
                task.id.to_string(),
                task.name,
                created_at_unix_ms,
            ],
        )?;
    }
    // Every prerequisite row is inserted after all tasks exist, so the self-referential foreign key
    // on depends_on_task_id resolves regardless of the order tasks appear in the document.
    for task in plan.phases.iter().flat_map(|phase| phase.tasks.iter()) {
        for (prerequisite, milestone) in &task.dependencies {
            transaction.execute(
                "INSERT INTO plan_task_dependencies (
                    feature_id, plan_revision, task_id, depends_on_task_id, required_milestone
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    plan.feature_id.to_string(),
                    plan.revision.get(),
                    task.id.to_string(),
                    prerequisite.to_string(),
                    milestone_column(*milestone),
                ],
            )?;
        }
    }
    Ok(())
}

impl Store {
    /// Loads one task's durable state.
    pub fn task(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
    ) -> Result<Option<TaskRecord>, StoreError> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {TASK_COLUMNS} FROM plan_tasks
                     WHERE feature_id = ?1 AND plan_revision = ?2 AND task_id = ?3"
                ),
                params![
                    feature_id.to_string(),
                    plan_revision.get(),
                    task_id.to_string()
                ],
                task_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Lists every task of a plan revision in stable identifier order.
    pub fn plan_tasks(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM plan_tasks
             WHERE feature_id = ?1 AND plan_revision = ?2 ORDER BY task_id"
        ))?;
        let rows = statement.query_map(
            params![feature_id.to_string(), plan_revision.get()],
            task_from_row,
        )?;
        rows.map(|row| row.map_err(StoreError::from)).collect()
    }

    /// Lists tasks of a plan revision currently in one lifecycle state.
    pub fn tasks_in_status(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
        status: TaskStatus,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM plan_tasks
             WHERE feature_id = ?1 AND plan_revision = ?2 AND status = ?3 ORDER BY task_id"
        ))?;
        let rows = statement.query_map(
            params![
                feature_id.to_string(),
                plan_revision.get(),
                status_column(status)
            ],
            task_from_row,
        )?;
        rows.map(|row| row.map_err(StoreError::from)).collect()
    }

    /// Returns the prerequisites one task waits on, with the milestone each must reach.
    pub fn task_prerequisites(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
    ) -> Result<BTreeMap<TaskId, TargetMilestone>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT depends_on_task_id, required_milestone FROM plan_task_dependencies
             WHERE feature_id = ?1 AND plan_revision = ?2 AND task_id = ?3
             ORDER BY depends_on_task_id",
        )?;
        let rows = statement.query_map(
            params![
                feature_id.to_string(),
                plan_revision.get(),
                task_id.to_string()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (task, milestone) = row?;
            Ok((
                TaskId::from_str(&task)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                milestone_from_column(&milestone)?,
            ))
        })
        .collect()
    }

    /// Returns the tasks that wait on one prerequisite.
    ///
    /// This is the "what depends on task X" query. Cancelling or revising a task uses it to find
    /// the work that must be blocked or reconciled.
    pub fn task_dependents(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
        prerequisite: TaskId,
    ) -> Result<Vec<TaskDependency>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT task_id, required_milestone FROM plan_task_dependencies
             WHERE feature_id = ?1 AND plan_revision = ?2 AND depends_on_task_id = ?3
             ORDER BY task_id",
        )?;
        let rows = statement.query_map(
            params![
                feature_id.to_string(),
                plan_revision.get(),
                prerequisite.to_string()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (task, milestone) = row?;
            Ok(TaskDependency {
                task_id: TaskId::from_str(&task)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                depends_on: prerequisite,
                required_milestone: milestone_from_column(&milestone)?,
            })
        })
        .collect()
    }

    /// Applies one legal lifecycle transition to a task.
    ///
    /// The domain owns which transitions are legal; this makes the decision durable atomically by
    /// repeating the state it was decided from as a write condition.
    pub fn advance_task(
        &mut self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
        target: TaskStatus,
        reason: Option<StateReason>,
        now_unix_ms: i64,
    ) -> Result<TaskRecord, StoreError> {
        let mut record = self.expect_task(feature_id, plan_revision, task_id)?;
        let observed_status = status_column(record.state.status());
        let observed_blocked_from = record.state.blocked_from().map(status_column);
        record
            .state
            .transition(target, reason)
            .map_err(|error| StoreError::Conflict(error.to_string()))?;

        let changed = self.connection.execute(
            "UPDATE plan_tasks
             SET status = ?4, reason_json = ?5, blocked_from = ?6, updated_at_unix_ms = ?7
             WHERE feature_id = ?1 AND plan_revision = ?2 AND task_id = ?3
               AND status = ?8 AND blocked_from IS ?9",
            params![
                feature_id.to_string(),
                plan_revision.get(),
                task_id.to_string(),
                status_column(record.state.status()),
                encode_reason(record.state.reason())?,
                record.state.blocked_from().map(status_column),
                now_unix_ms,
                observed_status,
                observed_blocked_from,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "task changed concurrently; re-read it before retrying".into(),
            ));
        }
        self.expect_task(feature_id, plan_revision, task_id)
    }

    /// Walks a task forward through the ordinary lifecycle until it reaches `target`.
    ///
    /// Only reason-free forward steps are taken, so this can never skip a state or invent a
    /// justification for blocking, cancelling, or superseding. Callers use it where one durable
    /// event establishes several steps at once: capture proves a task was built *and* captured.
    pub fn advance_task_to(
        &mut self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
        target: TaskStatus,
        now_unix_ms: i64,
    ) -> Result<TaskRecord, StoreError> {
        let mut record = self.expect_task(feature_id, plan_revision, task_id)?;
        let mut guard = 0;
        while record.state.status() != target {
            guard += 1;
            if guard > 16 {
                return Err(StoreError::Conflict(format!(
                    "task {task_id} cannot reach {target:?} by ordinary transitions"
                )));
            }
            let next = record.state.status().next_ordinary().ok_or_else(|| {
                StoreError::Conflict(format!(
                    "task {task_id} is {:?} and cannot advance to {target:?}",
                    record.state.status()
                ))
            })?;
            record =
                self.advance_task(feature_id, plan_revision, task_id, next, None, now_unix_ms)?;
        }
        Ok(record)
    }

    /// Records which tasks a captured package delivers.
    pub fn record_package_tasks(
        &mut self,
        package_id: PackageId,
        package_revision: Revision,
        task_ids: impl IntoIterator<Item = TaskId>,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        for task_id in task_ids {
            transaction.execute(
                "INSERT INTO snapshot_package_tasks (package_id, package_revision, task_id)
                 VALUES (?1, ?2, ?3)",
                params![
                    package_id.to_string(),
                    package_revision.get(),
                    task_id.to_string()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns the tasks one package delivers.
    pub fn package_task_ids(
        &self,
        package_id: PackageId,
        package_revision: Revision,
    ) -> Result<Vec<TaskId>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT task_id FROM snapshot_package_tasks
             WHERE package_id = ?1 AND package_revision = ?2 ORDER BY task_id",
        )?;
        let rows = statement.query_map(
            params![package_id.to_string(), package_revision.get()],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            TaskId::from_str(&row?).map_err(|error| StoreError::InvalidData(error.to_string()))
        })
        .collect()
    }

    fn expect_task(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
    ) -> Result<TaskRecord, StoreError> {
        self.task(feature_id, plan_revision, task_id)?
            .ok_or_else(|| {
                StoreError::InvalidData(format!(
                    "task {task_id} is not stored for plan revision {}",
                    plan_revision.get()
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use reccursive_core::{
        AcceptanceCheck, PLAN_SCHEMA_VERSION, PlanPhase, PlanTask, PublicationMode, ReasonCode,
        RepositoryId, RepositoryPolicy, TargetRef,
    };

    use super::*;
    use crate::RepositoryRegistration;

    struct Fixture {
        store: Store,
        feature_id: FeatureId,
        interface: TaskId,
        caller: TaskId,
    }

    /// A two-task plan in the BF-01 shape: the caller needs the interface published first.
    fn fixture() -> Fixture {
        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/example",
                    "ssh://git@example.invalid/project.git",
                    "/tmp/managed/example.git",
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
        let interface = TaskId::new();
        let caller = TaskId::new();
        let task =
            |id: TaskId, name: &str, dependencies: BTreeMap<TaskId, TargetMilestone>| PlanTask {
                id,
                name: name.into(),
                dependencies,
                acceptance_checks: vec![AcceptanceCheck {
                    id: "done".into(),
                    description: "it works".into(),
                }],
            };
        store
            .import_plan(
                &FeaturePlan {
                    schema_version: PLAN_SCHEMA_VERSION,
                    feature_id,
                    revision: Revision::FIRST,
                    repository_id,
                    goal: "Ship the interface and its caller".into(),
                    target,
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: vec![
                            task(interface, "Add the interface", BTreeMap::new()),
                            task(
                                caller,
                                "Add the caller",
                                BTreeMap::from([(interface, TargetMilestone::TargetPublished)]),
                            ),
                        ],
                    }],
                },
                5,
            )
            .unwrap();

        Fixture {
            store,
            feature_id,
            interface,
            caller,
        }
    }

    #[test]
    fn importing_a_plan_makes_its_tasks_and_dependencies_queryable() {
        let fixture = fixture();
        let tasks = fixture
            .store
            .plan_tasks(fixture.feature_id, Revision::FIRST)
            .unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(
            tasks
                .iter()
                .all(|task| task.state.status() == TaskStatus::Planned),
            "an imported task has not been built yet"
        );

        // The question a JSON blob could not answer.
        let dependents = fixture
            .store
            .task_dependents(fixture.feature_id, Revision::FIRST, fixture.interface)
            .unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].task_id, fixture.caller);
        assert_eq!(
            dependents[0].required_milestone,
            TargetMilestone::TargetPublished
        );

        assert_eq!(
            fixture
                .store
                .task_prerequisites(fixture.feature_id, Revision::FIRST, fixture.caller)
                .unwrap(),
            BTreeMap::from([(fixture.interface, TargetMilestone::TargetPublished)])
        );
        assert!(
            fixture
                .store
                .task_dependents(fixture.feature_id, Revision::FIRST, fixture.caller)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_task_reaches_queued_through_every_durable_step() {
        let mut fixture = fixture();
        let queued = fixture
            .store
            .advance_task_to(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                TaskStatus::Queued,
                10,
            )
            .unwrap();
        assert_eq!(queued.state.status(), TaskStatus::Queued);

        assert_eq!(
            fixture
                .store
                .tasks_in_status(fixture.feature_id, Revision::FIRST, TaskStatus::Queued)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            fixture
                .store
                .tasks_in_status(fixture.feature_id, Revision::FIRST, TaskStatus::Planned)
                .unwrap()
                .len(),
            1,
            "an unrelated task must not be dragged forward"
        );

        // The state survives a reload rather than living in memory.
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, fixture.interface)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Queued
        );
    }

    #[test]
    fn storage_refuses_a_transition_the_domain_forbids() {
        let mut fixture = fixture();
        let skipped = fixture.store.advance_task(
            fixture.feature_id,
            Revision::FIRST,
            fixture.interface,
            TaskStatus::Queued,
            None,
            10,
        );
        assert!(
            matches!(skipped, Err(StoreError::Conflict(_))),
            "planned cannot jump straight to queued: {skipped:?}"
        );

        // Blocking records why, and resumes at the exact state it paused from.
        fixture
            .store
            .advance_task_to(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                TaskStatus::Captured,
                10,
            )
            .unwrap();
        let blocked = fixture
            .store
            .advance_task(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                TaskStatus::Blocked,
                Some(StateReason::new(ReasonCode::ValidationFailed, "check failed").unwrap()),
                11,
            )
            .unwrap();
        assert_eq!(blocked.state.blocked_from(), Some(TaskStatus::Captured));
        assert_eq!(
            blocked.state.reason().map(|reason| reason.code.clone()),
            Some(ReasonCode::ValidationFailed)
        );

        let resumed = fixture
            .store
            .advance_task(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                TaskStatus::Captured,
                None,
                12,
            )
            .unwrap();
        assert_eq!(resumed.state.status(), TaskStatus::Captured);
        assert!(resumed.state.reason().is_none());
    }

    #[test]
    fn a_package_records_the_tasks_it_delivers() {
        let mut fixture = fixture();
        let package_id = stored_package(&mut fixture);
        fixture
            .store
            .record_package_tasks(package_id, Revision::FIRST, [fixture.interface])
            .unwrap();
        assert_eq!(
            fixture
                .store
                .package_task_ids(package_id, Revision::FIRST)
                .unwrap(),
            vec![fixture.interface]
        );
    }

    /// Builds the workspace and snapshot rows a package-task link depends on.
    fn stored_package(fixture: &mut Fixture) -> PackageId {
        use crate::{SnapshotRecord, WorkspaceRecord};
        use serde_json::json;
        let object_id = |byte: char| -> String { std::iter::repeat_n(byte, 40).collect() };
        fixture
            .store
            .record_workspace(&WorkspaceRecord {
                feature_id: fixture.feature_id,
                revision: Revision::FIRST,
                path: "/tmp/managed/workspace".into(),
                base_commit: object_id('a'),
                prerequisites: json!([]),
                created_at_unix_ms: 1,
            })
            .unwrap();
        let package_id = PackageId::new();
        fixture
            .store
            .record_snapshot(&SnapshotRecord {
                package_id,
                revision: Revision::FIRST,
                feature_id: fixture.feature_id,
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
        package_id
    }
}
