//! Periodic maintenance and orderly shutdown.
//!
//! The daemon is mostly idle. What it must not do while idle is poll: a loop that wakes constantly
//! to ask whether anything is due costs battery and CPU for a question whose answer is almost
//! always no. Instead it wakes rarely, and each time it asks the two questions that cannot be
//! answered by having been asleep — has time jumped, and is anything overdue.
//!
//! Shutdown is cooperative rather than immediate. Every stage of a release is durable before the
//! operation it names, so a hard kill is already safe; draining exists so the common case is also
//! *tidy*, finishing what is in flight rather than relying on recovery to clean up after it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use reccursive_store::{Store, StoreError};

use crate::lifecycle::{TimeTransition, WakeDetector};

/// A shutdown request that every long-running part of the daemon can observe.
///
/// Shared rather than per-thread so one request stops the whole service: the accept loop stops
/// taking connections, and maintenance stops handing out work, without either needing to know
/// about the other.
#[derive(Clone, Debug, Default)]
pub struct ShutdownSignal(Arc<AtomicBool>);

impl ShutdownSignal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks every observer to stop starting new work.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Reports whether shutdown has been requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What one maintenance pass found and did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MaintenanceReport {
    /// Time passed that the process did not observe, such as a sleep.
    pub resumed_after: Option<Duration>,
    /// Repositories whose overdue work was reconciled.
    pub repositories_reconciled: usize,
    /// Repositories whose reconciliation failed and was recorded instead.
    pub repositories_failed: usize,
}

/// Runs the daemon's periodic work.
///
/// The interval is deliberately coarse. Scheduling resolution is minutes, and a slot whose time
/// arrives between two ticks is simply overdue at the next one — which is the same state a slot
/// reaches after a sleep, and is handled by the same path. Nothing depends on a tick landing at any
/// particular moment.
pub struct Maintenance {
    detector: WakeDetector,
    shutdown: ShutdownSignal,
    interval: Duration,
}

impl Maintenance {
    #[must_use]
    pub fn new(
        shutdown: ShutdownSignal,
        interval: Duration,
        monotonic: Instant,
        wall_clock_ms: i64,
    ) -> Self {
        Self {
            // Tolerance is a fraction of the interval: a pass that ran late because the machine was
            // busy is ordinary, while a gap far larger than the interval means time passed without
            // the process running through it.
            detector: WakeDetector::started_at(monotonic, wall_clock_ms, interval / 2),
            shutdown,
            interval,
        }
    }

    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Performs one maintenance pass.
    ///
    /// Takes both clocks so the caller decides what "now" means, which is what makes a multi-day
    /// sleep something a test can express rather than something only a real machine can produce.
    pub fn tick(
        &mut self,
        store: &mut Store,
        monotonic: Instant,
        wall_clock_ms: i64,
        seed: u64,
    ) -> Result<MaintenanceReport, StoreError> {
        let mut report = MaintenanceReport::default();
        match self.detector.observe(monotonic, wall_clock_ms) {
            TimeTransition::Resumed { unobserved } => report.resumed_after = Some(unobserved),
            // A backwards clock is not a resume, but anything computed from the previous reading
            // is now suspect, so the pass still runs rather than trusting stale conclusions.
            TimeTransition::ClockMovedBackwards { .. } | TimeTransition::Continuous => {}
        }

        // Shutdown stops new work, not this: reconciling is what records what is overdue, and a
        // service going down should leave that written rather than discovered later.
        for repository in store.repositories()? {
            let repository_id = repository.registration.id;
            if store
                .overdue_schedule_slots(repository_id, wall_clock_ms)?
                .is_empty()
            {
                continue;
            }
            match crate::scheduler::Scheduler::reconcile_missed_windows(
                store,
                repository_id,
                seed,
                wall_clock_ms,
            ) {
                Ok(_) => report.repositories_reconciled += 1,
                // A repository with no policy configured yet is not an error worth stopping for.
                Err(_) => report.repositories_failed += 1,
            }
        }
        Ok(report)
    }

    /// Reports whether the loop should stop before its next pass.
    #[must_use]
    pub fn should_stop(&self) -> bool {
        self.shutdown.is_requested()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use reccursive_core::{
        AcceptanceCheck, DailyReleaseRange, DailyTime, DailyWindow, FeatureId, FeaturePlan,
        IanaTimeZone, MissedWindowBehavior, PLAN_SCHEMA_VERSION, PackageId, PlanPhase, PlanTask,
        PublicationMode, ReleaseUnitId, RepositoryId, RepositoryPolicy, Revision, SchedulePolicy,
        TargetRef, TaskId, TaskStatus, Weekday,
    };
    use reccursive_store::{
        NewScheduleSlot, RepositoryRegistration, SnapshotRecord, StoredSchedulePolicy,
        WorkspaceRecord,
    };
    use serde_json::json;

    use super::*;

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    /// A repository with one release time already three days in the past.
    fn overdue_store(now_unix_ms: i64) -> (Store, ReleaseUnitId, TaskId, FeatureId) {
        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/maintenance-source",
                    "ssh://git@example.invalid/maintenance.git",
                    "/tmp/maintenance-managed.git",
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
                policy: SchedulePolicy::new(
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
                    MissedWindowBehavior::RescheduleForward,
                )
                .unwrap(),
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
                    goal: "Work that waited through a sleep".into(),
                    target,
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
                path: "/tmp/maintenance-workspace".into(),
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
                path: "/tmp/maintenance-package".into(),
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
        store
            .persist_schedule_slot(&NewScheduleSlot {
                release_unit_id: unit_id,
                package_id,
                package_revision: Revision::FIRST,
                policy_revision: Revision::FIRST,
                timezone: IanaTimeZone::new("UTC").unwrap(),
                eligible_at_unix_ms: now_unix_ms - 4 * DAY_MS,
                selected_at_unix_ms: now_unix_ms - 3 * DAY_MS,
                created_at_unix_ms: now_unix_ms - 4 * DAY_MS,
            })
            .unwrap();
        (store, unit_id, task_id, feature_id)
    }

    #[test]
    fn a_tick_after_a_long_sleep_catches_the_work_that_was_missed() {
        let now = 1_800_000_000_000;
        let (mut store, unit_id, _, _) = overdue_store(now);
        let start = Instant::now();
        let mut maintenance = Maintenance::new(
            ShutdownSignal::new(),
            Duration::from_secs(60),
            start,
            now - 3 * DAY_MS,
        );

        // The machine slept for three days: the monotonic clock barely moved. No timer fired
        // during that gap, which is exactly why the work has to be found rather than awaited.
        let report = maintenance
            .tick(&mut store, start + Duration::from_secs(2), now, 7)
            .unwrap();

        assert!(
            report.resumed_after.is_some(),
            "a three-day gap must be recognized as time the process did not observe"
        );
        assert_eq!(report.repositories_reconciled, 1);
        assert_eq!(report.repositories_failed, 0);

        // The overdue selection was replaced with a future one rather than left in the past.
        let slot = store.schedule_slot(unit_id).unwrap().unwrap();
        assert!(slot.selected_at_unix_ms > now);
    }

    #[test]
    fn an_idle_tick_changes_nothing_and_reports_nothing() {
        let now = 1_800_000_000_000;
        let (mut store, unit_id, _, _) = overdue_store(now);
        let start = Instant::now();
        let mut maintenance =
            Maintenance::new(ShutdownSignal::new(), Duration::from_secs(60), start, now);

        // First pass clears the backlog.
        maintenance
            .tick(&mut store, start + Duration::from_secs(60), now, 7)
            .unwrap();
        let after_first = store.schedule_slot(unit_id).unwrap().unwrap();

        // A second pass with nothing overdue must do no work at all. An idle daemon that keeps
        // rewriting state is the polling this loop exists to avoid.
        let report = maintenance
            .tick(
                &mut store,
                start + Duration::from_secs(120),
                now + 60_000,
                7,
            )
            .unwrap();

        assert_eq!(report, MaintenanceReport::default());
        assert_eq!(
            store.schedule_slot(unit_id).unwrap().unwrap(),
            after_first,
            "an idle pass must not disturb an existing selection"
        );
    }

    #[test]
    fn a_shutdown_request_is_visible_to_every_observer() {
        let shutdown = ShutdownSignal::new();
        let observer = shutdown.clone();
        let maintenance = Maintenance::new(
            shutdown.clone(),
            Duration::from_secs(60),
            Instant::now(),
            1_000,
        );

        assert!(!maintenance.should_stop());
        assert!(!observer.is_requested());

        shutdown.request();

        // One request stops everything, without any observer knowing about the others.
        assert!(maintenance.should_stop());
        assert!(observer.is_requested());
    }

    #[test]
    fn shutdown_does_not_stop_a_pass_from_recording_what_is_overdue() {
        let now = 1_800_000_000_000;
        let (mut store, unit_id, _, _) = overdue_store(now);
        let shutdown = ShutdownSignal::new();
        let start = Instant::now();
        let mut maintenance =
            Maintenance::new(shutdown.clone(), Duration::from_secs(60), start, now);
        shutdown.request();

        let report = maintenance
            .tick(&mut store, start + Duration::from_secs(60), now, 7)
            .unwrap();

        // Going down is not a reason to leave a stale deadline behind for the next process to
        // rediscover; the pass still writes what it found.
        assert_eq!(report.repositories_reconciled, 1);
        assert!(
            store
                .schedule_slot(unit_id)
                .unwrap()
                .unwrap()
                .selected_at_unix_ms
                > now
        );
    }
}
