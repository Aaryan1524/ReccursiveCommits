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
        Self::schedule_at(
            store,
            release_unit_id,
            package_id,
            package_revision,
            seed,
            None,
            now_unix_ms,
        )
    }

    /// Selects a release time, either drawn from the policy or the exact one a person asked for.
    ///
    /// `requested_at_unix_ms` does not bypass the policy. It is checked against the same rules the
    /// generator draws within, and refused when it is not a time this repository may publish at.
    /// A requested time that passes is stored exactly as given: silently moving somebody's release
    /// would tell them it is scheduled without telling them when.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_at(
        store: &mut Store,
        release_unit_id: ReleaseUnitId,
        package_id: PackageId,
        package_revision: Revision,
        seed: u64,
        requested_at_unix_ms: Option<i64>,
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
        // A repository override refines the policy at the moment of selection, so changing either
        // one independently stays coherent. The domain validates the combination as a whole.
        let effective = match store.schedule_override(plan.plan.repository_id)? {
            Some(override_policy) => policy.policy.with_override(&override_policy)?,
            None => policy.policy.clone(),
        };
        let existing = store
            .schedule_slots(plan.plan.repository_id)?
            .into_iter()
            .map(|slot| slot.selected_at_unix_ms)
            .collect::<Vec<_>>();
        let selected = match requested_at_unix_ms {
            Some(requested) => effective
                .validate_requested_slot(requested, now_unix_ms, &existing)
                .map_err(|error| {
                    // A refusal that only says no leaves the asker guessing. The policy is in hand
                    // here, so the answer can carry what *is* allowed on the day they chose.
                    SchedulerError::RequestedTimeRefused {
                        detail: describe_refusal(&error, &effective, requested),
                    }
                })?,
            None => SlotGenerator::seeded(seed)
                .select(&effective, now_unix_ms, 1, &existing)?
                .pop()
                .ok_or(SchedulerError::NoSelectableSlot)?,
        };
        Ok(store.persist_schedule_slot(&NewScheduleSlot {
            release_unit_id,
            package_id,
            package_revision,
            policy_revision: policy.revision,
            timezone: selected.timezone,
            eligible_at_unix_ms: now_unix_ms,
            selected_at_unix_ms: selected.selected_at_unix_ms,
            created_at_unix_ms: now_unix_ms,
            // The scheduler chose this time, so a passed one really is a missed window.
            released_on_request: false,
        })?)
    }
}

/// A unit that is due and may be claimed for release right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DueUnit {
    pub release_unit_id: ReleaseUnitId,
    pub repository_id: reccursive_core::RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub selected_at_unix_ms: i64,
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
    /// Lists the units due for release now, fairly across repositories and within a global limit.
    ///
    /// Fairness is the point rather than throughput. Taking every due unit from one repository
    /// before looking at the next would let a repository with a deep queue, or one that keeps
    /// failing and retrying, hold the worker while everything else waits. Instead this takes one
    /// unit per repository per round, in due order, so a noisy repository delays only itself.
    ///
    /// `concurrency_limit` bounds how much work is handed out at once, which is what keeps a large
    /// queue from turning into a burst of simultaneous pushes.
    pub fn due_units(
        store: &Store,
        now_unix_ms: i64,
        concurrency_limit: usize,
    ) -> Result<Vec<DueUnit>, SchedulerError> {
        if concurrency_limit == 0 {
            return Ok(Vec::new());
        }
        // Each repository's due work, oldest selection first.
        let mut per_repository: Vec<Vec<DueUnit>> = Vec::new();
        for repository in store.repositories()? {
            let repository_id = repository.registration.id;
            // Pausing governs what is started. Work already transmitted keeps its own recovery
            // path; this only declines to hand out anything new.
            if store.repository_pause(repository_id)?.is_some() {
                continue;
            }
            // A repository whose remote is failing waits out its own backoff. Scoped to this
            // repository so a single unreachable remote cannot stall every other one, and durable
            // so a restart mid-outage resumes the wait instead of retrying immediately.
            if !store.integration_is_ready(
                reccursive_core::Integration::Git,
                &repository_id.to_string(),
                now_unix_ms,
            )? {
                continue;
            }
            let mut due = Vec::new();
            for slot in store.overdue_schedule_slots(repository_id, now_unix_ms)? {
                // A unit is claimable only while every one of its tasks is still merely scheduled.
                // Anything further along is already owned by an attempt, and handing it out again
                // would be a second publisher for the same work.
                if !Self::is_claimable(
                    store,
                    slot.release_unit_id,
                    slot.package_id,
                    slot.package_revision,
                )? {
                    continue;
                }
                due.push(DueUnit {
                    release_unit_id: slot.release_unit_id,
                    repository_id,
                    package_id: slot.package_id,
                    package_revision: slot.package_revision,
                    selected_at_unix_ms: slot.selected_at_unix_ms,
                });
            }
            due.sort_by_key(|unit| (unit.selected_at_unix_ms, unit.release_unit_id));
            if !due.is_empty() {
                per_repository.push(due);
            }
        }

        let mut claimed = Vec::new();
        let mut round = 0;
        while claimed.len() < concurrency_limit {
            let mut took_any = false;
            for repository_due in &per_repository {
                if claimed.len() == concurrency_limit {
                    break;
                }
                if let Some(unit) = repository_due.get(round) {
                    claimed.push(unit.clone());
                    took_any = true;
                }
            }
            if !took_any {
                break;
            }
            round += 1;
        }
        Ok(claimed)
    }

    /// Reports whether a unit is genuinely free to be picked up for release.
    ///
    /// Two independent things have to be true, because they move independently. The unit's tasks
    /// must still be waiting at their selected time — anything further along is finished, blocked,
    /// or cancelled. And no attempt may already be publishing this package: the release worker
    /// advances the *attempt* through its stages and leaves the tasks at `scheduled` for the whole
    /// flight, so task state alone would happily offer work that is already on its way to the
    /// remote.
    fn is_claimable(
        store: &Store,
        release_unit_id: ReleaseUnitId,
        package_id: PackageId,
        package_revision: Revision,
    ) -> Result<bool, SchedulerError> {
        if store
            .live_attempt_for_package(package_id, package_revision)?
            .is_some()
        {
            return Ok(false);
        }
        let Some(unit) = store.release_unit(release_unit_id)? else {
            return Ok(false);
        };
        for task_id in &unit.task_ids {
            let Some(task) = store.task(unit.feature_id, unit.plan_revision, *task_id)? else {
                return Ok(false);
            };
            if task.state.status() != reccursive_core::TaskStatus::Scheduled {
                return Ok(false);
            }
        }
        Ok(true)
    }

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
            // A time a person asked for is not a missed window. Both look identical here — a slot
            // whose time has passed — so without this `schedule release-now`, issued outside a
            // release window, was undone by the very next pass and the command did nothing at all.
            if slot.released_on_request {
                outcome.retained.push(slot.release_unit_id);
                continue;
            }
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

/// Turns a policy refusal into something worth reading, naming the hours that are open.
///
/// Deliberately does not offer to reschedule. Somebody who asked for 10:30 should be told why
/// 10:30 is not possible, not quietly given 14:05 and left to discover it later.
pub(crate) fn describe_refusal(
    error: &reccursive_core::SchedulePolicyError,
    policy: &reccursive_core::SchedulePolicy,
    requested_unix_ms: i64,
) -> String {
    use reccursive_core::SchedulePolicyError as PolicyError;
    let hours = || {
        let windows = policy.windows_on(requested_unix_ms);
        if windows.is_empty() {
            format!(
                "This repository publishes on {}.",
                policy
                    .allowed_days
                    .iter()
                    .map(|day| format!("{day:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "Publishing hours that day are {}.",
                windows
                    .iter()
                    .map(|window| format!(
                        "{:02}:{:02}-{:02}:{:02}",
                        window.start.hour, window.start.minute, window.end.hour, window.end.minute
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    match error {
        PolicyError::RequestedTimeOutsideWindows { .. }
        | PolicyError::RequestedDayNotAllowed { .. } => format!("{error}. {}", hours()),
        other => other.to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Policy(#[from] reccursive_core::SchedulePolicyError),
    #[error("{detail}")]
    RequestedTimeRefused { detail: String },
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
                    released_on_request: false,
                })
                .unwrap();
            units.push(unit_id);
        }
        (store, repository_id, units, feature_id, task_ids)
    }

    /// Adds a second enrolled repository to an existing fixture, with one overdue unit.
    fn add_second_repository(store: &mut Store, now_unix_ms: i64) -> (RepositoryId, ReleaseUnitId) {
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/quiet-source",
                    "ssh://git@example.invalid/quiet.git",
                    "/tmp/quiet-managed.git",
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
                policy: policy(MissedWindowBehavior::RescheduleForward),
                created_at_unix_ms: 1,
            })
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
                    goal: "One quiet unit".into(),
                    target,
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: vec![PlanTask {
                            id: task_id,
                            name: "Quiet task".into(),
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
                path: "/tmp/quiet-workspace".into(),
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
                path: std::path::PathBuf::from("/tmp/quiet-package"),
                base_tree: object_id('a'),
                result_tree: object_id('e'),
                content_hash: std::iter::repeat_n('e', 64).collect(),
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
                eligible_at_unix_ms: now_unix_ms - 2 * DAY_MS,
                // Deliberately the newest overdue selection, so a naive oldest-first global sweep
                // would serve it last. Fair rotation must still reach it in the first round.
                selected_at_unix_ms: now_unix_ms - 1,
                created_at_unix_ms: now_unix_ms - 2 * DAY_MS,
                released_on_request: false,
            })
            .unwrap();
        (repository_id, unit_id)
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
    fn due_work_is_taken_fairly_so_one_deep_queue_cannot_starve_another() {
        let now = 1_800_000_000_000;
        // Three overdue units in one repository, one in another.
        let (mut busy_store, busy_repo, busy_units, _, _) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 3, now);
        let (quiet_repo, quiet_unit) = add_second_repository(&mut busy_store, now);

        // A limit of two must not be consumed entirely by the repository that happens to be first.
        let claimed = Scheduler::due_units(&busy_store, now, 2).unwrap();
        assert_eq!(claimed.len(), 2);
        let repositories: BTreeSet<_> = claimed.iter().map(|unit| unit.repository_id).collect();
        assert_eq!(
            repositories,
            BTreeSet::from([busy_repo, quiet_repo]),
            "each repository must get a turn before any repository gets a second"
        );

        // With room for everything, all four appear and the quiet repository is not last-served.
        let all = Scheduler::due_units(&busy_store, now, 10).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.iter().any(|unit| unit.release_unit_id == quiet_unit));
        for unit_id in busy_units {
            assert!(all.iter().any(|unit| unit.release_unit_id == unit_id));
        }

        // The global limit is a real bound, not advisory.
        assert!(
            Scheduler::due_units(&busy_store, now, 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(Scheduler::due_units(&busy_store, now, 1).unwrap().len(), 1);
    }

    #[test]
    fn a_repository_override_refines_the_policy_used_for_selection() {
        let now = 1_800_000_000_000;
        let (mut store, repository_id, units, _, _) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 1, now);
        let unit_id = units[0];
        let package_id = store.schedule_slot(unit_id).unwrap().unwrap().package_id;

        // Narrow this repository to a single weekday. The shared policy still permits every day.
        store
            .activate_schedule_override(
                repository_id,
                &reccursive_core::SchedulePolicyOverride {
                    timezone: None,
                    allowed_days: Some(BTreeSet::from([Weekday::Wednesday])),
                    windows: None,
                    daily_releases: None,
                    minimum_spacing_minutes: None,
                    missed_window_behavior: None,
                },
                now,
            )
            .unwrap();
        store
            .invalidate_schedule_slot(unit_id, "applying a repository override", now)
            .unwrap();

        let slot =
            Scheduler::schedule(&mut store, unit_id, package_id, Revision::FIRST, 5, now).unwrap();

        // The policy's zone is UTC, so the civil weekday is plain arithmetic on the instant.
        // 1970-01-01 was a Thursday; counting Sunday as 0 makes Thursday 4 and Wednesday 3.
        let days_since_epoch = slot.selected_at_unix_ms.div_euclid(DAY_MS);
        let weekday = (days_since_epoch + 4).rem_euclid(7);
        assert_eq!(
            weekday, 3,
            "the override must constrain selection to Wednesday even though the policy allows \
             every day"
        );
    }

    #[test]
    fn pausing_stops_new_work_without_disturbing_a_transmitted_push() {
        use reccursive_store::{AttemptLease, NewReleaseAttempt};

        let now = 1_800_000_000_000;
        let (mut store, repository_id, units, feature_id, tasks) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 2, now);

        // A real in-flight push, produced the way the release worker produces one: the attempt
        // advances, the unit's tasks stay `scheduled`.
        let slot = store.schedule_slot(units[0]).unwrap().unwrap();
        let attempt = store
            .open_release_attempt(&NewReleaseAttempt {
                attempt_id: reccursive_core::AttemptId::new(),
                repository_id,
                package_id: slot.package_id,
                package_revision: slot.package_revision,
                remote: "ssh://git@example.invalid/missed.git".into(),
                target: reccursive_core::TargetRef::new("refs/heads/main").unwrap(),
                base_commit: object_id('a'),
                lease: AttemptLease {
                    owner: "worker".into(),
                    expires_at_unix_ms: now + 300_000,
                },
                created_at_unix_ms: now,
            })
            .unwrap();
        for status in [
            TaskStatus::Reconciling,
            TaskStatus::Verifying,
            TaskStatus::CommitPrepared,
        ] {
            store
                .advance_release_attempt(attempt.attempt_id, status, None, now)
                .unwrap();
        }
        store
            .record_push_intent(attempt.attempt_id, &object_id('f'), &object_id('a'), now)
            .unwrap();
        store
            .advance_release_attempt(attempt.attempt_id, TaskStatus::PushPending, None, now)
            .unwrap();
        assert!(
            store
                .release_attempt(attempt.attempt_id)
                .unwrap()
                .unwrap()
                .state
                .status()
                .may_have_reached_remote(),
            "this attempt's push is on the wire"
        );

        // Only the second unit is free; the first is already being published.
        assert_eq!(Scheduler::due_units(&store, now, 10).unwrap().len(), 1);

        store
            .pause_repository(repository_id, "investigating a failure", now)
            .unwrap();
        assert!(
            Scheduler::due_units(&store, now, 10).unwrap().is_empty(),
            "a paused repository must not hand out new work"
        );

        // The transmitted push keeps its own state and its own recovery path. Pausing governs what
        // starts, and this one already started.
        let during_pause = store.release_attempt(attempt.attempt_id).unwrap().unwrap();
        assert_eq!(during_pause.state.status(), TaskStatus::PushPending);
        assert_eq!(
            during_pause.candidate_sha.as_deref(),
            Some(object_id('f').as_str())
        );
        assert_eq!(
            store
                .release_attempts_awaiting_remote_resolution()
                .unwrap()
                .len(),
            1,
            "an in-flight push must still be resolved against the remote despite the pause"
        );
        assert_eq!(
            store
                .task(feature_id, Revision::FIRST, tasks[0])
                .unwrap()
                .unwrap()
                .state
                .status(),
            TaskStatus::Scheduled
        );

        assert!(store.resume_repository(repository_id).unwrap());
        let resumed = Scheduler::due_units(&store, now, 10).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].release_unit_id, units[1]);
    }

    #[test]
    fn releasing_now_still_refuses_a_unit_whose_prerequisite_has_not_arrived() {
        let now = 1_800_000_000_000;
        let (mut store, _, units, feature_id, tasks) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 1, now);

        // A unit with satisfied prerequisites moves to the front of the queue on request.
        let released = store.release_unit_now(units[0], now).unwrap();
        assert_eq!(released.selected_at_unix_ms, now);

        // Now make the unit ineligible the way a real prerequisite failure would.
        store
            .advance_task(
                feature_id,
                Revision::FIRST,
                tasks[0],
                TaskStatus::Blocked,
                Some(
                    reccursive_core::StateReason::new(
                        reccursive_core::ReasonCode::ValidationFailed,
                        "a check failed",
                    )
                    .unwrap(),
                ),
                now,
            )
            .unwrap();

        let refused = store.release_unit_now(units[0], now + 1);
        assert!(
            refused.is_err(),
            "release-now must not bypass the eligibility rules: {refused:?}"
        );
    }

    #[test]
    fn a_failing_remote_delays_only_its_own_repository() {
        let now = 1_800_000_000_000;
        let (mut store, busy_repo, _, _, _) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 2, now);
        let (quiet_repo, quiet_unit) = add_second_repository(&mut store, now);

        // The first repository's remote is unreachable.
        store
            .record_integration_failure(
                reccursive_core::Integration::Git,
                &busy_repo.to_string(),
                reccursive_core::ConnectivityFault::Unreachable,
                "could not reach the remote",
                reccursive_core::BackoffPolicy::default(),
                now,
            )
            .unwrap();

        let due = Scheduler::due_units(&store, now, 10).unwrap();

        assert!(
            due.iter().all(|unit| unit.repository_id == quiet_repo),
            "a repository waiting out a remote outage must not be handed work"
        );
        assert!(
            due.iter().any(|unit| unit.release_unit_id == quiet_unit),
            "an unrelated repository must keep working through another's outage"
        );

        // Once the backoff elapses, the repository is offered work again.
        let health = store
            .integration_health(reccursive_core::Integration::Git, &busy_repo.to_string())
            .unwrap()
            .unwrap();
        let after = health.next_attempt_at_unix_ms.unwrap();
        assert!(
            Scheduler::due_units(&store, after, 10)
                .unwrap()
                .iter()
                .any(|unit| unit.repository_id == busy_repo)
        );
    }

    #[test]
    fn a_unit_whose_attempt_is_already_running_is_not_offered_again() {
        use reccursive_store::{AttemptLease, NewReleaseAttempt};

        let now = 1_800_000_000_000;
        let (mut store, repository_id, units, _, _) =
            overdue_fixture(MissedWindowBehavior::RescheduleForward, 2, now);

        let first = store.schedule_slot(units[0]).unwrap().unwrap();
        // Exactly what the release worker does when it picks work up: it opens an attempt and
        // advances *the attempt*. The unit's tasks stay `scheduled` for the whole flight.
        let attempt = store
            .open_release_attempt(&NewReleaseAttempt {
                attempt_id: reccursive_core::AttemptId::new(),
                repository_id,
                package_id: first.package_id,
                package_revision: first.package_revision,
                remote: "ssh://git@example.invalid/missed.git".into(),
                target: reccursive_core::TargetRef::new("refs/heads/main").unwrap(),
                base_commit: object_id('a'),
                lease: AttemptLease {
                    owner: "worker".into(),
                    expires_at_unix_ms: now + 300_000,
                },
                created_at_unix_ms: now,
            })
            .unwrap();
        store
            .advance_release_attempt(attempt.attempt_id, TaskStatus::Reconciling, None, now)
            .unwrap();

        let due = Scheduler::due_units(&store, now, 10).unwrap();

        assert!(
            !due.iter().any(|unit| unit.release_unit_id == units[0]),
            "a unit already being published must not be offered for release again"
        );
        assert!(
            due.iter().any(|unit| unit.release_unit_id == units[1]),
            "the untouched unit should still be offered"
        );
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
