//! Daemon-owned bridge from persisted scheduling policy to durable queue slots.

use reccursive_core::{MissedWindowBehavior, PackageId, ReleaseUnitId, Revision, SlotGenerator};
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

/// What handling a set of missed windows actually did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MissedWindowOutcome {
    /// Units left due for immediate release under an explicit catch-up allowance.
    pub released_now: Vec<ReleaseUnitId>,
    /// Units whose overdue time was withdrawn and replaced with a future one.
    pub rescheduled: Vec<ReleaseUnitId>,
    /// Units left alone because an attempt already owns them.
    pub retained: Vec<ReleaseUnitId>,
}

impl Scheduler {
    /// Applies a repository's missed-window policy to every slot whose time has already passed.
    ///
    /// Two things this deliberately never does. It never moves a selection backwards: a
    /// replacement time is drawn from `now` forward, so an offline gap cannot produce a commit
    /// dated earlier than the moment it was actually made. And it never releases a whole backlog
    /// at once by default: `RescheduleForward` spreads everything back into future windows, and
    /// `CatchUp` releases only as many as its own configured limit allows, oldest first.
    pub fn reconcile_missed_windows(
        store: &mut Store,
        repository_id: reccursive_core::RepositoryId,
        seed: u64,
        now_unix_ms: i64,
    ) -> Result<MissedWindowOutcome, SchedulerError> {
        let overdue = store.overdue_schedule_slots(repository_id, now_unix_ms)?;
        if overdue.is_empty() {
            return Ok(MissedWindowOutcome::default());
        }
        let policy = store
            .schedule_policy(repository_id)?
            .ok_or(SchedulerError::MissingPolicy(repository_id))?;

        let allowance = match policy.policy.missed_window_behavior {
            MissedWindowBehavior::RescheduleForward => 0,
            MissedWindowBehavior::CatchUp { max_releases } => usize::from(max_releases),
        };

        let mut outcome = MissedWindowOutcome::default();
        for (index, slot) in overdue.into_iter().enumerate() {
            // Oldest first: work that has waited longest is released first under a catch-up
            // allowance, rather than whichever unit happens to be cheapest to process.
            if index < allowance {
                outcome.released_now.push(slot.release_unit_id);
                continue;
            }
            match store.invalidate_schedule_slot(
                slot.release_unit_id,
                "release window was missed",
                now_unix_ms,
            ) {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                // An attempt already owns this unit; its own recovery path handles it.
                Err(StoreError::Conflict(_)) => {
                    outcome.retained.push(slot.release_unit_id);
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            Self::schedule(
                store,
                slot.release_unit_id,
                slot.package_id,
                slot.package_revision,
                seed.wrapping_add(index as u64),
                now_unix_ms,
            )?;
            outcome.rescheduled.push(slot.release_unit_id);
        }
        Ok(outcome)
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use reccursive_core::{
        AcceptanceCheck, DailyReleaseRange, DailyTime, DailyWindow, FeatureId, FeaturePlan,
        IanaTimeZone, PLAN_SCHEMA_VERSION, PlanPhase, PlanTask, PublicationMode, RepositoryId,
        RepositoryPolicy, SchedulePolicy, TargetRef, TaskId, TaskStatus, Weekday,
    };
    use reccursive_store::{
        RepositoryRegistration, SnapshotRecord, StoredSchedulePolicy, WorkspaceRecord,
    };
    use serde_json::json;

    use super::*;

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn policy(behavior: MissedWindowBehavior) -> SchedulePolicy {
        SchedulePolicy::new(
            IanaTimeZone::new("UTC").unwrap(),
            BTreeSet::from([
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
                Weekday::Saturday,
                Weekday::Sunday,
            ]),
            vec![
                DailyWindow::new(
                    DailyTime::new(0, 0).unwrap(),
                    DailyTime::new(23, 59).unwrap(),
                )
                .unwrap(),
            ],
            DailyReleaseRange::new(1, 5).unwrap(),
            30,
            behavior,
        )
        .unwrap()
    }

    /// A repository with `count` scheduled units whose release times are already in the past.
    fn overdue_fixture(
        behavior: MissedWindowBehavior,
        count: usize,
        now_unix_ms: i64,
    ) -> (
        Store,
        RepositoryId,
        Vec<ReleaseUnitId>,
        FeatureId,
        Vec<TaskId>,
    ) {
        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/missed-source",
                    "ssh://git@example.invalid/missed.git",
                    "/tmp/missed-managed.git",
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
        store
            .activate_schedule_policy(&StoredSchedulePolicy {
                repository_id,
                revision: Revision::FIRST,
                policy: policy(behavior),
                created_at_unix_ms: 1,
            })
            .unwrap();

        let feature_id = FeatureId::new();
        let task_ids: Vec<TaskId> = (0..count).map(|_| TaskId::new()).collect();
        store
            .import_plan(
                &FeaturePlan {
                    schema_version: PLAN_SCHEMA_VERSION,
                    feature_id,
                    revision: Revision::FIRST,
                    repository_id,
                    goal: "Units that missed their windows".into(),
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
                path: "/tmp/missed-workspace".into(),
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
                    path: std::path::PathBuf::from(format!("/tmp/missed-package-{name}")),
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
            // Selected days ago: the machine was asleep or offline through the window.
            store
                .persist_schedule_slot(&NewScheduleSlot {
                    release_unit_id: unit_id,
                    package_id,
                    package_revision: Revision::FIRST,
                    policy_revision: Revision::FIRST,
                    timezone: IanaTimeZone::new("UTC").unwrap(),
                    eligible_at_unix_ms: now_unix_ms - 4 * DAY_MS,
                    selected_at_unix_ms: now_unix_ms - 3 * DAY_MS + i64::try_from(index).unwrap(),
                    created_at_unix_ms: now_unix_ms - 4 * DAY_MS,
                })
                .unwrap();
            units.push(unit_id);
        }
        (store, repository_id, units, feature_id, task_ids)
    }

    #[test]
    fn a_multi_day_gap_reschedules_forward_and_never_backdates() {
        let now = 1_800_000_000_000;
        let (mut store, repository_id, units, _, _) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 3, now);

        let outcome =
            Scheduler::reconcile_missed_windows(&mut store, repository_id, 7, now).unwrap();

        // The default releases nothing immediately: a three-day backlog must not become a burst.
        assert!(
            outcome.released_now.is_empty(),
            "reschedule-forward must not dump the queue at once"
        );
        assert_eq!(outcome.rescheduled.len(), 3);

        for unit_id in units {
            let slot = store.schedule_slot(unit_id).unwrap().unwrap();
            assert!(
                slot.selected_at_unix_ms > now,
                "a replacement time must be in the future, never backdated: {} vs {now}",
                slot.selected_at_unix_ms
            );
        }
        assert!(
            store
                .overdue_schedule_slots(repository_id, now)
                .unwrap()
                .is_empty(),
            "nothing should remain overdue after reconciliation"
        );
    }

    #[test]
    fn catch_up_releases_only_its_configured_allowance_oldest_first() {
        let now = 1_800_000_000_000;
        let (mut store, repository_id, units, _, _) =
            overdue_fixture(MissedWindowBehavior::CatchUp { max_releases: 2 }, 5, now);

        let outcome =
            Scheduler::reconcile_missed_windows(&mut store, repository_id, 11, now).unwrap();

        // Bounded by the policy's own limit, not by how much work happens to be waiting.
        assert_eq!(outcome.released_now.len(), 2);
        assert_eq!(outcome.rescheduled.len(), 3);
        assert_eq!(outcome.released_now, units[..2].to_vec());

        for unit_id in &units[2..] {
            let slot = store.schedule_slot(*unit_id).unwrap().unwrap();
            assert!(slot.selected_at_unix_ms > now, "the rest must move forward");
        }
    }

    #[test]
    fn a_unit_with_a_live_attempt_is_left_for_its_own_recovery() {
        let now = 1_800_000_000_000;
        let (mut store, repository_id, units, feature_id, tasks) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 2, now);

        store
            .advance_task(
                feature_id,
                Revision::FIRST,
                tasks[0],
                TaskStatus::Reconciling,
                None,
                now,
            )
            .unwrap();

        let outcome =
            Scheduler::reconcile_missed_windows(&mut store, repository_id, 3, now).unwrap();

        assert_eq!(outcome.retained, vec![units[0]]);
        assert_eq!(outcome.rescheduled, vec![units[1]]);
        assert_eq!(
            store
                .task(feature_id, Revision::FIRST, tasks[0])
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Reconciling,
            "missed-window handling must not reach into a live attempt"
        );
    }
}
