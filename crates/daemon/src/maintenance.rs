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

/// How long one GitHub request may take before it is abandoned.
///
/// Generous, because a slow answer is still an answer and re-asking costs a rate limit; bounded,
/// because a maintenance pass that blocks forever stops every other repository behind it.
const GITHUB_TIMEOUT: Duration = Duration::from_secs(30);

/// The branch name GitHub wants, from the ref this product stores.
fn branch_name(target: &reccursive_core::TargetRef) -> String {
    target
        .as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(target.as_str())
        .to_owned()
}

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

/// How one pass's releases turned out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReleaseTally {
    pub published: usize,
    pub blocked: usize,
    pub deferred: usize,
}

/// Identity and message for a release nobody is present to describe.
///
/// An autonomous release still has to say who made the commit and why. The identity comes from the
/// enrolled checkout's own Git configuration, so a scheduled commit is attributed exactly as a
/// manual one from that repository would be. The message is derived from the plan task names,
/// because the plan is the only description of the work that exists without asking someone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAttribution {
    pub author_name: String,
    pub author_email: String,
}

impl ReleaseAttribution {
    /// Reads the identity Git itself would use for a commit in this checkout.
    pub fn from_checkout(checkout_path: &std::path::Path) -> Option<Self> {
        let name = git_config(checkout_path, "user.name")?;
        let email = git_config(checkout_path, "user.email")?;
        Some(Self {
            author_name: name,
            author_email: email,
        })
    }
}

fn git_config(checkout_path: &std::path::Path, key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout_path)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Builds the commit message for a unit from the plan that described it.
///
/// Deliberately plain and derived only from what the plan already says. Inventing a summary would
/// mean describing work nobody reviewed the description of.
pub fn release_message(task_names: &[String]) -> String {
    match task_names {
        [] => "chore: publish scheduled release unit".to_owned(),
        [single] => single.clone(),
        many => format!(
            "{}\n\n{}",
            "Publish scheduled release unit",
            many.iter()
                .map(|name| format!("- {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
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
    /// Units whose selected time had arrived and which were published.
    pub units_published: usize,
    /// Units that were attempted and stopped with a durable, explained block.
    pub units_blocked: usize,
    /// Units whose transport failed and which a later pass will retry.
    pub units_deferred: usize,
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

        // A slot whose time arrived moments ago is not a missed window — it is a release about to
        // happen, and the pass that publishes runs immediately after this one. Without this grace
        // a `reschedule_forward` repository could never publish anything at all: every tick
        // reconciled the slot into the future before the release pass could claim it, forever.
        // The grace is one interval, because that is the longest a due slot can wait for its pass.
        let missed_before = wall_clock_ms
            .saturating_sub(i64::try_from(self.interval.as_millis()).unwrap_or(i64::MAX));

        // Shutdown stops new work, not this: reconciling is what records what is overdue, and a
        // service going down should leave that written rather than discovered later.
        for repository in store.repositories()? {
            let repository_id = repository.registration.id;
            if store
                .overdue_schedule_slots(repository_id, missed_before)?
                .is_empty()
            {
                continue;
            }
            match crate::scheduler::Scheduler::reconcile_missed_windows(
                store,
                repository_id,
                seed,
                missed_before,
            ) {
                Ok(_) => report.repositories_reconciled += 1,
                // A repository with no policy configured yet is not an error worth stopping for.
                Err(_) => report.repositories_failed += 1,
            }
        }
        Ok(report)
    }

    /// Selects a release time for work that is on a development branch but not on the target.
    ///
    /// Immediate mode promises two publications: an early one, and an integration later. The
    /// first withdraws its own spent selection, so without this the integration would wait for
    /// someone to schedule it by hand and "publish early, integrate on schedule" would be half a
    /// feature.
    ///
    /// Run as its own pass rather than at the moment of publication, so a daemon that stops
    /// between the two picks the integration up on the next tick instead of stranding it.
    /// `Scheduler::schedule` returns an existing slot unchanged, so repeating this is free.
    ///
    /// A paused repository still has integrations scheduled. Pausing governs what is *started* —
    /// `due_units` skips paused repositories — and selecting a time starts nothing. Refusing to
    /// select would instead mean a pause silently erased the plan for work already published.
    pub fn schedule_pending_integrations(
        &self,
        store: &mut Store,
        seed: u64,
        now_unix_ms: i64,
    ) -> Result<usize, StoreError> {
        let mut scheduled = 0;
        for repository in store.repositories()? {
            let Some(development_target) = repository.active_policy.development_target.clone()
            else {
                continue;
            };
            // A pull-request repository never pushes to its target, so there is no second
            // publication to plan. Its integration is a request opened as soon as the development
            // publication succeeds, and then a person deciding when to merge.
            if repository.active_policy.target_integration
                == reccursive_core::TargetIntegration::PullRequest
            {
                continue;
            }
            let target = repository.active_policy.target.clone();
            for unit in store.release_units_for_repository(repository.registration.id)? {
                if self.shutdown.is_requested() {
                    return Ok(scheduled);
                }
                if store.schedule_slot(unit.unit_id)?.is_some() {
                    continue;
                }
                // A unit's tasks are carried by one package, so any of them names it.
                let Some(task_id) = unit.task_ids.iter().next().copied() else {
                    continue;
                };
                let Some(package) = store.snapshot_package_for_task(
                    unit.feature_id,
                    unit.plan_revision,
                    task_id,
                )?
                else {
                    continue;
                };
                // On the development branch and not yet on the target is exactly the work this
                // pass exists for. Both are read from the attempts, which recorded them.
                if !store.package_published_to(
                    package.package_id,
                    package.revision,
                    &development_target,
                )? || store.package_published_to(
                    package.package_id,
                    package.revision,
                    &target,
                )? {
                    continue;
                }
                // A unit whose tasks are not back in the queue is not waiting for a time; it is
                // blocked, cancelled, or already owned by an attempt. Refusals here are ordinary.
                if crate::scheduler::Scheduler::schedule(
                    store,
                    unit.unit_id,
                    package.package_id,
                    package.revision,
                    seed,
                    now_unix_ms,
                )
                .is_ok()
                {
                    scheduled += 1;
                }
            }
        }
        Ok(scheduled)
    }

    /// Opens a pull request for work that is due to reach the target and cannot be pushed there.
    ///
    /// The pull-request strategy publishes early to a development branch exactly as immediate mode
    /// always does; what changes is the integration. Instead of pushing to the target, the service
    /// asks GitHub to open a request against it, and then stops. Nothing here merges, and there is
    /// no code path that could: merging is authority over what lands on a main branch.
    ///
    /// Opening is claim-before-execute like the rest of the queue. A process that opened a pull
    /// request and died before recording it finds the existing one instead of opening a second,
    /// which is why `AlreadyExists` is recovered from rather than reported.
    pub fn open_pending_pull_requests(
        &self,
        store: &mut Store,
        state_dir: &std::path::Path,
        endpoint: &reccursive_github::Endpoint,
        now_unix_ms: i64,
    ) -> Result<usize, StoreError> {
        let mut opened = 0;
        for repository in store.repositories()? {
            if self.shutdown.is_requested() {
                return Ok(opened);
            }
            if repository.active_policy.target_integration
                != reccursive_core::TargetIntegration::PullRequest
            {
                continue;
            }
            let repository_id = repository.registration.id;
            // Pausing governs what is started, and opening a pull request starts something.
            if store.repository_pause(repository_id)?.is_some() {
                continue;
            }
            let Some(development_target) = repository.active_policy.development_target.clone()
            else {
                continue;
            };
            let Some(slug) = reccursive_github::RepositorySlug::from_remote(
                &repository.registration.canonical_remote,
            ) else {
                self.report_pull_request_problem(
                    store,
                    repository_id,
                    "github_remote_unrecognised",
                    "this repository's remote is not a GitHub URL, so no pull request can be \
                     opened for it; publish by direct push instead",
                    now_unix_ms,
                )?;
                continue;
            };
            let token =
                match reccursive_github::storage::load(state_dir, &repository_id.to_string()) {
                    Ok(token) => token,
                    Err(error) => {
                        self.report_pull_request_problem(
                            store,
                            repository_id,
                            "github_token_unavailable",
                            &format!("{error}; run: reccursive github set-token {repository_id}"),
                            now_unix_ms,
                        )?;
                        continue;
                    }
                };
            let client =
                reccursive_github::GitHubClient::new(endpoint.clone(), token, GITHUB_TIMEOUT);

            // Every unit, not only those holding an overdue slot. The pull request is opened as
            // soon as the development publication it describes has succeeded — the scheduled time
            // the person chose was for *that* publication, and making them wait for a second,
            // unrelated future slot before the request appeared meant the one time they picked did
            // not mean what it said.
            for unit in store.release_units_for_repository(repository_id)? {
                if self.shutdown.is_requested() {
                    return Ok(opened);
                }
                if store.pull_request(unit.unit_id)?.is_some() {
                    continue;
                }
                // A unit's tasks are carried by one package, so any of them names it.
                let Some(task_id) = unit.task_ids.iter().next().copied() else {
                    continue;
                };
                let Some(package) = store.snapshot_package_for_task(
                    unit.feature_id,
                    unit.plan_revision,
                    task_id,
                )?
                else {
                    continue;
                };
                // Only the integration half is a pull request. Work that has not reached the
                // development branch yet is an ordinary publication and belongs to the worker, and
                // work already on the target has nothing left to request.
                if !store.package_published_to(
                    package.package_id,
                    package.revision,
                    &development_target,
                )? || store.package_published_to(
                    package.package_id,
                    package.revision,
                    &repository.active_policy.target,
                )? {
                    continue;
                }
                let head = branch_name(&development_target);
                let base = branch_name(&repository.active_policy.target);
                let title = self.pull_request_title(store, unit.unit_id)?;
                let body = "Opened by reccursive at its scheduled release time.\n\n\
                            This pull request is not merged automatically, now or ever. \
                            Review and merge it yourself when you are ready.";

                let outcome = match client.create_pull_request(&slug, &head, &base, &title, body) {
                    Ok(pull_request) => Some(pull_request),
                    // Already open: a previous run created it and did not get to record it. The
                    // existing one is adopted rather than a second one opened.
                    Err(reccursive_github::GitHubError::AlreadyExists) => client
                        .find_open_pull_request(&slug, &slug.owner, &head)
                        .ok()
                        .flatten(),
                    Err(error) => {
                        self.report_pull_request_problem(
                            store,
                            repository_id,
                            if error.is_worth_retrying() {
                                "github_unavailable"
                            } else {
                                "github_refused"
                            },
                            &error.to_string(),
                            now_unix_ms,
                        )?;
                        None
                    }
                };
                let Some(pull_request) = outcome else {
                    continue;
                };

                store.record_pull_request(&reccursive_store::PullRequestRecord {
                    release_unit_id: unit.unit_id,
                    repository_id,
                    package_id: package.package_id,
                    package_revision: package.revision,
                    number: pull_request.number,
                    url: pull_request.url.clone(),
                    head: development_target.clone(),
                    base: repository.active_policy.target.clone(),
                    state: pull_request.state.clone(),
                    merged_at_unix_ms: None,
                    observed_at_unix_ms: now_unix_ms,
                    created_at_unix_ms: now_unix_ms,
                })?;
                // A live selection would now be meaningless: the unit waits on a person, not on
                // a time. There usually is not one — the development publication withdrew its own
                // — so an absent slot is the ordinary case rather than a fault.
                store.invalidate_schedule_slot(
                    unit.unit_id,
                    "a pull request was opened for this work",
                    now_unix_ms,
                )?;
                opened += 1;
            }
        }
        Ok(opened)
    }

    /// Asks GitHub what became of the pull requests this service opened.
    ///
    /// A merged pull request is the only thing that makes a task published under this strategy,
    /// and it is always somebody else's doing. Closed-without-merge is left as it is: the work is
    /// still captured, and deciding what to do with a rejected change is not automation's call.
    pub fn observe_pull_requests(
        &self,
        store: &mut Store,
        state_dir: &std::path::Path,
        endpoint: &reccursive_github::Endpoint,
        now_unix_ms: i64,
    ) -> Result<usize, StoreError> {
        let mut merged = 0;
        for record in store.open_pull_requests()? {
            if self.shutdown.is_requested() {
                return Ok(merged);
            }
            let Some(repository) = store.repository(record.repository_id)? else {
                continue;
            };
            let Some(slug) = reccursive_github::RepositorySlug::from_remote(
                &repository.registration.canonical_remote,
            ) else {
                continue;
            };
            let Ok(token) =
                reccursive_github::storage::load(state_dir, &record.repository_id.to_string())
            else {
                continue;
            };
            let client =
                reccursive_github::GitHubClient::new(endpoint.clone(), token, GITHUB_TIMEOUT);
            let Ok(observed) = client.pull_request(&slug, record.number) else {
                continue;
            };
            store.observe_pull_request(
                record.release_unit_id,
                &observed.state,
                observed.merged,
                now_unix_ms,
            )?;
            if !observed.merged {
                continue;
            }
            // Merged means the target now carries this work, which is exactly what `Published`
            // asserts. Nothing else in this strategy may set it.
            if let Some(unit) = store.release_unit(record.release_unit_id)? {
                for task_id in &unit.task_ids {
                    store.advance_task_to(
                        unit.feature_id,
                        unit.plan_revision,
                        *task_id,
                        reccursive_core::TaskStatus::Published,
                        now_unix_ms,
                    )?;
                }
            }
            merged += 1;
        }
        Ok(merged)
    }

    /// Names a pull request after the work it delivers, rather than after an identifier.
    fn pull_request_title(
        &self,
        store: &Store,
        release_unit_id: reccursive_core::ReleaseUnitId,
    ) -> Result<String, StoreError> {
        let Some(unit) = store.release_unit(release_unit_id)? else {
            return Ok("Scheduled change".to_owned());
        };
        let names: Vec<String> = unit
            .task_ids
            .iter()
            .filter_map(|task_id| {
                store
                    .task(unit.feature_id, unit.plan_revision, *task_id)
                    .ok()
                    .flatten()
                    .map(|task| task.name)
            })
            .collect();
        Ok(if names.is_empty() {
            "Scheduled change".to_owned()
        } else {
            names.join(", ")
        })
    }

    /// Records that a repository cannot be published from, and why.
    ///
    /// Written once per pass rather than deduplicated, because the event store already prunes and
    /// a repeated entry is how a reader sees that this is still happening rather than a single
    /// thing that happened once.
    fn report_unattributable_checkout(
        &self,
        store: &mut Store,
        repository_id: reccursive_core::RepositoryId,
        checkout_path: &str,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        self.report_pull_request_problem(
            store,
            repository_id,
            "checkout_unattributable",
            &format!(
                "{checkout_path} cannot provide an author for a commit; it may have been moved, \
                 deleted, or left without user.name and user.email. Nothing will publish from \
                 this repository until that is fixed: reccursive diagnose {repository_id}"
            ),
            now_unix_ms,
        )
    }

    /// Records why a pull request could not be opened, where a person will actually find it.
    fn report_pull_request_problem(
        &self,
        store: &mut Store,
        repository_id: reccursive_core::RepositoryId,
        kind: &str,
        detail: &str,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let event = reccursive_store::NewEvent::new(
            now_unix_ms,
            reccursive_store::EventContext {
                repository_id: Some(repository_id),
                ..reccursive_store::EventContext::default()
            },
            kind,
            reccursive_store::EventSeverity::Warning,
            None,
            detail,
            serde_json::Value::Null,
        )?;
        store.record_event(&event, reccursive_store::DEFAULT_EVENT_RETENTION)?;
        Ok(())
    }

    /// Publishes every unit whose selected time has arrived.
    ///
    /// This is what makes a schedule mean anything without someone at the keyboard: the pass that
    /// notices a slot is due is the same one that acts on it.
    ///
    /// Shutdown is honoured *between* units rather than during one. A release that has begun owns
    /// a durable attempt and a lease, and abandoning it partway would leave the remote's state to
    /// be rediscovered later; letting it finish is both safer and quicker than recovering from it.
    pub fn release_due_units(
        &self,
        store: &mut Store,
        worker: &crate::release::ReleaseWorker,
        now_unix_ms: i64,
        concurrency_limit: usize,
    ) -> Result<ReleaseTally, StoreError> {
        let due = crate::scheduler::Scheduler::due_units(store, now_unix_ms, concurrency_limit)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let mut tally = ReleaseTally::default();

        for unit in due {
            if self.shutdown.is_requested() {
                break;
            }
            let Some(repository) = store.repository(unit.repository_id)? else {
                continue;
            };
            // Under the pull-request strategy the integration is a request somebody merges, not a
            // push. Handing this unit to the worker would push it to the target and bypass the
            // pull request entirely — the exact thing the strategy was chosen to prevent. The
            // early publication to the development branch is still an ordinary push, so only work
            // that has already reached that branch is withheld.
            if repository.active_policy.target_integration
                == reccursive_core::TargetIntegration::PullRequest
                && let Some(development_target) = &repository.active_policy.development_target
                && store.package_published_to(
                    unit.package_id,
                    unit.package_revision,
                    development_target,
                )?
            {
                continue;
            }
            // No identity configured means no commit can be attributed. Refusing is the only
            // honest option: inventing an author would put a name on work it did not do.
            //
            // Refusing silently is not. A checkout that has been moved, deleted, or left without
            // an identity makes every pass skip this repository forever, and without a record the
            // queue simply looks idle. The reason is written where `logs` and the diagnostic
            // export will show it.
            let Some(attribution) = ReleaseAttribution::from_checkout(std::path::Path::new(
                &repository.registration.checkout_path,
            )) else {
                self.report_unattributable_checkout(
                    store,
                    unit.repository_id,
                    &repository.registration.checkout_path,
                    now_unix_ms,
                )?;
                tally.blocked += 1;
                continue;
            };
            let task_names = store
                .release_unit(unit.release_unit_id)?
                .map(|release_unit| {
                    release_unit
                        .task_ids
                        .iter()
                        .filter_map(|task_id| {
                            store
                                .task(
                                    release_unit.feature_id,
                                    release_unit.plan_revision,
                                    *task_id,
                                )
                                .ok()
                                .flatten()
                                .map(|task| task.name)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let request = crate::release::ReleaseRequest {
                package_id: unit.package_id,
                package_revision: unit.package_revision,
                message: release_message(&task_names),
                author_name: attribution.author_name,
                author_email: attribution.author_email,
            };
            match worker.release(store, &request, now_unix_ms) {
                Ok(crate::release::ReleaseOutcome::Published { .. }) => tally.published += 1,
                Ok(crate::release::ReleaseOutcome::Blocked { .. }) => tally.blocked += 1,
                // A transport failure already carries its own retry time; it is waiting, not
                // broken, and must not be reported as needing attention.
                Ok(crate::release::ReleaseOutcome::Deferred { .. }) => tally.deferred += 1,
                // The attempt records its own reason; a failure here must not stop the units
                // behind it from being tried.
                Err(_) => tally.blocked += 1,
            }
        }
        Ok(tally)
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
                released_on_request: false,
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
