//! Release units, cancellation, and supersession.
//!
//! A release unit is the smallest set of tasks that can be published together without leaving the
//! target broken. Grouping is decided by the milestone each dependency requires: a task that only
//! needs its prerequisite *captured* is coupled to it and must ship in the same unit, while a task
//! that needs its prerequisite *published* is sequential and belongs to a later one.

use std::collections::{BTreeMap, BTreeSet};

use reccursive_core::{
    FeatureId, ReasonCode, ReleaseUnit, ReleaseUnitId, Revision, StateReason, TargetMilestone,
    TaskId, TaskStatus,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// A stored release unit and the tasks it publishes together.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseUnitRecord {
    pub unit_id: ReleaseUnitId,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
    pub required_checks: BTreeSet<String>,
    pub created_at_unix_ms: i64,
}

/// What a cancellation or supersession actually changed.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LifecycleOutcome {
    /// Dependents blocked because the work they wait on will not arrive as planned.
    pub blocked_dependents: Vec<TaskId>,
    /// Validation results marked stale because the work they attest to changed.
    pub invalidated_evidence: usize,
}

impl Store {
    /// Groups tasks into one release unit, refusing a grouping that could break the target.
    ///
    /// The domain decides whether the grouping is coherent; this supplies it only the *coupling*
    /// dependencies — those satisfied by capture rather than publication — because a prerequisite
    /// that must be published first belongs to an earlier unit, not this one.
    pub fn create_release_unit(
        &mut self,
        unit_id: ReleaseUnitId,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_ids: BTreeSet<TaskId>,
        required_checks: BTreeSet<String>,
        created_at_unix_ms: i64,
    ) -> Result<ReleaseUnitRecord, StoreError> {
        if created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "release unit timestamp must not be negative".into(),
            ));
        }
        let planned: BTreeSet<_> = self
            .plan_tasks(feature_id, plan_revision)?
            .into_iter()
            .map(|task| task.task_id)
            .collect();
        if let Some(unknown) = task_ids.difference(&planned).next() {
            return Err(StoreError::InvalidData(format!(
                "task {unknown} is not part of plan revision {}",
                plan_revision.get()
            )));
        }

        let coupling = self.coupling_dependencies(feature_id, plan_revision)?;
        let unit = ReleaseUnit::new(unit_id, task_ids, &coupling, required_checks)
            .map_err(|error| StoreError::Conflict(error.to_string()))?;

        let transaction = self.connection.transaction()?;
        transaction
            .execute(
                "INSERT INTO release_units (
                    unit_id, feature_id, plan_revision, required_checks_json, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    unit.id.to_string(),
                    feature_id.to_string(),
                    plan_revision.get(),
                    serde_json::to_string(&unit.required_checks)?,
                    created_at_unix_ms,
                ],
            )
            .map_err(map_unit_write_error)?;
        for task_id in &unit.task_ids {
            transaction
                .execute(
                    "INSERT INTO release_unit_tasks (unit_id, feature_id, plan_revision, task_id)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        unit.id.to_string(),
                        feature_id.to_string(),
                        plan_revision.get(),
                        task_id.to_string()
                    ],
                )
                .map_err(map_unit_write_error)?;
        }
        transaction.commit()?;

        Ok(ReleaseUnitRecord {
            unit_id: unit.id,
            feature_id,
            plan_revision,
            task_ids: unit.task_ids,
            required_checks: unit.required_checks,
            created_at_unix_ms,
        })
    }

    /// Returns the unit a task publishes with, if it has been grouped.
    pub fn release_unit_for_task(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
    ) -> Result<Option<ReleaseUnitRecord>, StoreError> {
        let unit_id: Option<String> = self
            .connection
            .query_row(
                "SELECT unit_id FROM release_unit_tasks
                 WHERE feature_id = ?1 AND plan_revision = ?2 AND task_id = ?3",
                params![
                    feature_id.to_string(),
                    plan_revision.get(),
                    task_id.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        match unit_id {
            Some(unit_id) => {
                let unit_id = unit_id
                    .parse()
                    .map_err(|error: reccursive_core::IdParseError| {
                        StoreError::InvalidData(error.to_string())
                    })?;
                self.release_unit(unit_id)
            }
            None => Ok(None),
        }
    }

    /// Loads one release unit and its members.
    pub fn release_unit(
        &self,
        unit_id: ReleaseUnitId,
    ) -> Result<Option<ReleaseUnitRecord>, StoreError> {
        let row: Option<(String, u32, String, i64)> = self
            .connection
            .query_row(
                "SELECT feature_id, plan_revision, required_checks_json, created_at_unix_ms
                 FROM release_units WHERE unit_id = ?1",
                [unit_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((feature_id, plan_revision, checks, created_at_unix_ms)) = row else {
            return Ok(None);
        };

        let mut statement = self.connection.prepare(
            "SELECT task_id FROM release_unit_tasks WHERE unit_id = ?1 ORDER BY task_id",
        )?;
        let task_ids = statement
            .query_map([unit_id.to_string()], |row| row.get::<_, String>(0))?
            .map(|task| {
                task?
                    .parse::<TaskId>()
                    .map_err(|error| StoreError::InvalidData(error.to_string()))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;

        Ok(Some(ReleaseUnitRecord {
            unit_id,
            feature_id: feature_id
                .parse()
                .map_err(|error: reccursive_core::IdParseError| {
                    StoreError::InvalidData(error.to_string())
                })?,
            plan_revision: Revision::new(plan_revision)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            task_ids,
            required_checks: serde_json::from_str(&checks)?,
            created_at_unix_ms,
        }))
    }

    /// Cancels a task and blocks everything waiting on it.
    ///
    /// Published work is immutable, so cancelling it is refused by the domain rather than handled
    /// here. Dependents are blocked rather than silently released: a prerequisite that will never
    /// arrive must not leave its dependents eligible.
    pub fn cancel_task(
        &mut self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
        message: &str,
        now_unix_ms: i64,
    ) -> Result<LifecycleOutcome, StoreError> {
        let reason = StateReason::new(ReasonCode::UserRequested, message)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        self.advance_task(
            feature_id,
            plan_revision,
            task_id,
            TaskStatus::Cancelled,
            Some(reason),
            now_unix_ms,
        )?;
        let blocked_dependents = self.block_dependents(
            feature_id,
            plan_revision,
            task_id,
            ReasonCode::PrerequisiteCancelled,
            "a prerequisite was cancelled",
            now_unix_ms,
        )?;
        let invalidated_evidence =
            self.invalidate_task_evidence(task_id, "task cancelled", now_unix_ms)?;
        Ok(LifecycleOutcome {
            blocked_dependents,
            invalidated_evidence,
        })
    }

    /// Marks a task superseded by newer work, preserving the original and its evidence.
    pub fn supersede_task(
        &mut self,
        feature_id: FeatureId,
        plan_revision: Revision,
        task_id: TaskId,
        message: &str,
        now_unix_ms: i64,
    ) -> Result<LifecycleOutcome, StoreError> {
        let reason = StateReason::new(ReasonCode::SupersededByRevision, message)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        self.advance_task(
            feature_id,
            plan_revision,
            task_id,
            TaskStatus::Superseded,
            Some(reason),
            now_unix_ms,
        )?;
        let blocked_dependents = self.block_dependents(
            feature_id,
            plan_revision,
            task_id,
            ReasonCode::SupersededByRevision,
            "a prerequisite was superseded and needs reconciliation",
            now_unix_ms,
        )?;
        let invalidated_evidence =
            self.invalidate_task_evidence(task_id, "task superseded", now_unix_ms)?;
        Ok(LifecycleOutcome {
            blocked_dependents,
            invalidated_evidence,
        })
    }

    /// Counts validation results that still stand for a package revision.
    pub fn valid_evidence_count(
        &self,
        package_id: reccursive_core::PackageId,
        package_revision: Revision,
    ) -> Result<usize, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM validation_evidence
             WHERE package_id = ?1 AND package_revision = ?2 AND invalidated_at_unix_ms IS NULL",
            params![package_id.to_string(), package_revision.get()],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Dependency edges a unit must be closed under: those satisfied by capture alone.
    fn coupling_dependencies(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
    ) -> Result<BTreeMap<TaskId, BTreeSet<TaskId>>, StoreError> {
        let mut coupling: BTreeMap<TaskId, BTreeSet<TaskId>> = BTreeMap::new();
        for task in self.plan_tasks(feature_id, plan_revision)? {
            for (prerequisite, milestone) in
                self.task_prerequisites(feature_id, plan_revision, task.task_id)?
            {
                if milestone == TargetMilestone::Captured {
                    coupling
                        .entry(task.task_id)
                        .or_default()
                        .insert(prerequisite);
                }
            }
        }
        Ok(coupling)
    }

    /// Blocks every task waiting on a prerequisite, directly or transitively.
    fn block_dependents(
        &mut self,
        feature_id: FeatureId,
        plan_revision: Revision,
        prerequisite: TaskId,
        code: ReasonCode,
        message: &str,
        now_unix_ms: i64,
    ) -> Result<Vec<TaskId>, StoreError> {
        let reason = StateReason::new(code, message)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut blocked = Vec::new();
        let mut pending = vec![prerequisite];
        let mut seen = BTreeSet::from([prerequisite]);

        while let Some(current) = pending.pop() {
            for dependency in self.task_dependents(feature_id, plan_revision, current)? {
                if !seen.insert(dependency.task_id) {
                    continue;
                }
                pending.push(dependency.task_id);
                let Some(task) = self.task(feature_id, plan_revision, dependency.task_id)? else {
                    continue;
                };
                // Work that already reached a terminal state is history and stays as it is; a task
                // that is already blocked keeps its original reason.
                if task.state.status().is_terminal() || task.state.status() == TaskStatus::Blocked {
                    continue;
                }
                self.advance_task(
                    feature_id,
                    plan_revision,
                    dependency.task_id,
                    TaskStatus::Blocked,
                    Some(reason.clone()),
                    now_unix_ms,
                )?;
                blocked.push(dependency.task_id);
            }
        }
        blocked.sort_unstable();
        Ok(blocked)
    }

    /// Marks evidence for every package carrying a task as no longer standing.
    fn invalidate_task_evidence(
        &mut self,
        task_id: TaskId,
        reason: &str,
        now_unix_ms: i64,
    ) -> Result<usize, StoreError> {
        let changed = self.connection.execute(
            "UPDATE validation_evidence
             SET invalidated_at_unix_ms = ?2, invalidated_reason = ?3
             WHERE invalidated_at_unix_ms IS NULL
               AND (package_id, package_revision) IN (
                   SELECT package_id, package_revision FROM snapshot_package_tasks
                   WHERE task_id = ?1
               )",
            params![task_id.to_string(), now_unix_ms, reason],
        )?;
        let candidate_changed = self.connection.execute(
            "UPDATE candidate_validation_evidence
             SET invalidated_at_unix_ms = ?2, invalidated_reason = ?3
             WHERE invalidated_at_unix_ms IS NULL
               AND (package_id, package_revision) IN (
                   SELECT package_id, package_revision FROM snapshot_package_tasks
                   WHERE task_id = ?1
               )",
            params![task_id.to_string(), now_unix_ms, reason],
        )?;
        Ok(changed + candidate_changed)
    }
}

fn map_unit_write_error(error: rusqlite::Error) -> StoreError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        StoreError::Conflict(format!("release unit rejected by storage rules: {error}"))
    } else {
        StoreError::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use reccursive_core::{
        AcceptanceCheck, FeaturePlan, PLAN_SCHEMA_VERSION, PackageId, PlanPhase, PlanTask,
        PublicationMode, RepositoryId, RepositoryPolicy, TargetRef,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        RepositoryRegistration, SnapshotRecord, TrustedCheck, ValidationEvidence, WorkspaceRecord,
    };

    struct Fixture {
        store: Store,
        feature_id: FeatureId,
        interface: TaskId,
        caller: TaskId,
        docs: TaskId,
    }

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    /// interface <- caller (coupled: caller only needs it captured)
    /// caller    <- docs   (sequential: docs needs the caller published)
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
        let docs = TaskId::new();
        let task = |id, name: &str, dependencies| PlanTask {
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
                    goal: "Ship an interface, its caller, and docs".into(),
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
                                BTreeMap::from([(interface, TargetMilestone::Captured)]),
                            ),
                            task(
                                docs,
                                "Document it",
                                BTreeMap::from([(caller, TargetMilestone::TargetPublished)]),
                            ),
                        ],
                    }],
                },
                1,
            )
            .unwrap();

        Fixture {
            store,
            feature_id,
            interface,
            caller,
            docs,
        }
    }

    impl Fixture {
        fn unit(&mut self, tasks: BTreeSet<TaskId>) -> Result<ReleaseUnitRecord, StoreError> {
            self.store.create_release_unit(
                ReleaseUnitId::new(),
                self.feature_id,
                Revision::FIRST,
                tasks,
                BTreeSet::from(["integration".to_owned()]),
                2,
            )
        }

        /// Captures a package delivering `tasks` and records passing evidence for it.
        fn captured_package(&mut self, tasks: &[TaskId], name: char) -> PackageId {
            if self
                .store
                .workspace(self.feature_id, Revision::FIRST)
                .unwrap()
                .is_none()
            {
                self.store
                    .record_workspace(&WorkspaceRecord {
                        feature_id: self.feature_id,
                        revision: Revision::FIRST,
                        path: "/tmp/managed/workspace".into(),
                        base_commit: object_id('a'),
                        prerequisites: json!([]),
                        created_at_unix_ms: 1,
                    })
                    .unwrap();
            }
            let package_id = PackageId::new();
            self.store
                .record_snapshot(&SnapshotRecord {
                    package_id,
                    revision: Revision::FIRST,
                    feature_id: self.feature_id,
                    plan_revision: Revision::FIRST,
                    path: PathBuf::from(format!("/tmp/managed/package-{name}")),
                    base_tree: object_id(name),
                    result_tree: object_id(name),
                    content_hash: std::iter::repeat_n(name, 64).collect(),
                    parent_package_id: None,
                    manifest: json!({ "paths": [] }),
                    created_at_unix_ms: 1,
                })
                .unwrap();
            self.store
                .record_package_tasks(package_id, Revision::FIRST, tasks.iter().copied())
                .unwrap();
            self.store
                .add_trusted_check(&TrustedCheck {
                    repository_id: self
                        .store
                        .repositories()
                        .unwrap()
                        .first()
                        .unwrap()
                        .registration
                        .id,
                    id: format!("check-{name}"),
                    command: vec!["true".into()],
                    timeout_seconds: 30,
                    enabled: true,
                })
                .ok();
            self.store
                .record_validation_evidence(&ValidationEvidence {
                    package_id,
                    revision: Revision::FIRST,
                    check_id: format!("check-{name}"),
                    command: vec!["true".into()],
                    exit_code: Some(0),
                    timed_out: false,
                    output_summary: String::new(),
                    executed_at_unix_ms: 2,
                })
                .unwrap();
            package_id
        }
    }

    use std::path::PathBuf;

    #[test]
    fn a_coupled_caller_cannot_be_released_without_its_interface() {
        let mut fixture = fixture();

        // The caller only needs the interface captured, so shipping it alone would leave the
        // target referring to code that is not there.
        let broken = fixture.unit(BTreeSet::from([fixture.caller]));
        assert!(
            matches!(broken, Err(StoreError::Conflict(_))),
            "a coupled caller must not form a unit on its own: {broken:?}"
        );

        let coherent = fixture
            .unit(BTreeSet::from([fixture.interface, fixture.caller]))
            .unwrap();
        assert_eq!(coherent.task_ids.len(), 2);

        // Docs waits for the caller to be published, so it is sequential and stands alone.
        let sequential = fixture.unit(BTreeSet::from([fixture.docs])).unwrap();
        assert_eq!(sequential.task_ids, BTreeSet::from([fixture.docs]));

        assert_eq!(
            fixture
                .store
                .release_unit_for_task(fixture.feature_id, Revision::FIRST, fixture.caller)
                .unwrap()
                .map(|unit| unit.unit_id),
            Some(coherent.unit_id)
        );
    }

    #[test]
    fn a_task_cannot_be_claimed_by_two_release_units() {
        let mut fixture = fixture();
        fixture
            .unit(BTreeSet::from([fixture.interface, fixture.caller]))
            .unwrap();
        let duplicate = fixture.unit(BTreeSet::from([fixture.interface, fixture.caller]));
        assert!(
            matches!(duplicate, Err(StoreError::Conflict(_))),
            "one task cannot be published by two units: {duplicate:?}"
        );
    }

    #[test]
    fn a_cancelled_prerequisite_blocks_dependents_and_invalidates_their_evidence() {
        let mut fixture = fixture();
        let package = fixture.captured_package(&[fixture.interface], 'b');
        assert_eq!(
            fixture
                .store
                .valid_evidence_count(package, Revision::FIRST)
                .unwrap(),
            1
        );

        let outcome = fixture
            .store
            .cancel_task(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                "no longer needed",
                10,
            )
            .unwrap();

        // The caller depends on the interface directly and docs depends on it through the caller:
        // both must stop, or a cancelled prerequisite silently unlocks its dependents.
        assert_eq!(outcome.blocked_dependents.len(), 2);
        for task_id in [fixture.caller, fixture.docs] {
            let task = fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, task_id)
                .unwrap()
                .unwrap();
            assert_eq!(task.state.status(), TaskStatus::Blocked);
            assert_eq!(
                task.state.reason().map(|reason| reason.code.clone()),
                Some(ReasonCode::PrerequisiteCancelled)
            );
        }

        // Evidence is invalidated, never deleted: the original result stays auditable.
        assert_eq!(outcome.invalidated_evidence, 1);
        assert_eq!(
            fixture
                .store
                .valid_evidence_count(package, Revision::FIRST)
                .unwrap(),
            0
        );
        let preserved: i64 = fixture
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM validation_evidence WHERE package_id = ?1",
                [package.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, 1);
    }

    #[test]
    fn published_work_cannot_be_cancelled_or_superseded() {
        let mut fixture = fixture();
        for status in [
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
            fixture
                .store
                .advance_task(
                    fixture.feature_id,
                    Revision::FIRST,
                    fixture.interface,
                    status,
                    None,
                    10,
                )
                .unwrap();
        }

        for outcome in [
            fixture.store.cancel_task(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                "too late",
                11,
            ),
            fixture.store.supersede_task(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                "too late",
                11,
            ),
        ] {
            assert!(
                matches!(outcome, Err(StoreError::Conflict(_))),
                "published work is immutable: {outcome:?}"
            );
        }

        // The dependents were never touched by the refused operation.
        assert_eq!(
            fixture
                .store
                .task(fixture.feature_id, Revision::FIRST, fixture.caller)
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Planned
        );
    }

    #[test]
    fn supersession_preserves_the_original_and_marks_dependents_for_reconciliation() {
        let mut fixture = fixture();
        let package = fixture.captured_package(&[fixture.interface], 'c');
        let outcome = fixture
            .store
            .supersede_task(
                fixture.feature_id,
                Revision::FIRST,
                fixture.interface,
                "replaced by a newer revision",
                10,
            )
            .unwrap();

        let original = fixture
            .store
            .task(fixture.feature_id, Revision::FIRST, fixture.interface)
            .unwrap()
            .unwrap();
        assert_eq!(original.state.status(), TaskStatus::Superseded);
        assert_eq!(
            original.state.reason().map(|reason| reason.code.clone()),
            Some(ReasonCode::SupersededByRevision)
        );

        assert_eq!(outcome.blocked_dependents.len(), 2);
        assert_eq!(outcome.invalidated_evidence, 1);
        assert_eq!(
            fixture
                .store
                .valid_evidence_count(package, Revision::FIRST)
                .unwrap(),
            0
        );
        // The package itself is untouched: supersession invalidates evidence, not captured work.
        assert!(
            fixture
                .store
                .snapshot(package, Revision::FIRST)
                .unwrap()
                .is_some()
        );
    }
}
