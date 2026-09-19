//! The human scheduling wizard behind `reccursive schedule`.
//!
//! One job: let somebody who is about to be offline say "ship this on Thursday at 10:30" and be
//! confident it will happen. Everything here is presentation and prompting — the work is done by
//! the same protocol commands the non-interactive commands send, so a release scheduled through
//! this wizard is indistinguishable from one scheduled by hand, and inherits every guarantee the
//! release engine already makes.
//!
//! Nothing internal is shown: no package or unit identifiers, no revisions, no seeds, no
//! milliseconds, no lifecycle states. Those remain the language of `--json` and the advanced
//! commands, which are unchanged.

mod checkout;
mod pick;
mod when;

pub use when::to_instant;

/// How many times to re-ask before giving up, so a contended queue cannot loop forever.
const MAX_ATTEMPTS: usize = 5;

use std::io::Write;

use reccursive_protocol::{
    Command, ReadyWorkGroup, ResponseData, ScheduleCapturedWorkRequest,
    ScheduleCheckoutBatchesRequest, TargetIntegration,
};

use crate::{CliFailure, EXIT_ACTION_REQUIRED, Session, human_time_in, send};

/// Runs the wizard end to end, or explains why it cannot.
///
/// Two sources of work, and the person is only asked which when both exist. Prepared work is what
/// an agent captured through the plan flow; checkout changes are what they edited themselves.
/// They are never mixed into one release: a release unit's tasks must match a package's exactly,
/// so a combined one could not be scheduled at all.
pub fn schedule(session: &Session, stdout: &mut impl Write) -> Result<(), CliFailure> {
    let working_directory = std::env::current_dir().map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "no_working_directory",
            error.to_string(),
        )
    })?;
    let here = checkout::enrolled_here(session, &working_directory)?;

    let prepared = ready_work(session)?;
    let checkout_changes = match &here {
        Some((_, root)) => checkout::changes(root)?,
        None => Vec::new(),
    };

    if prepared.is_empty() && checkout_changes.is_empty() {
        // Standing in an unenrolled repository is the one case worth naming, because the fix is a
        // single command rather than anything about capturing work.
        if here.is_none() && git_repository_here(&working_directory) {
            writeln!(
                stdout,
                "This repository isn't set up for Reccursive yet.\n\nRun:\n  reccursive setup"
            )
            .map_err(crate::output_error)?;
            return Ok(());
        }
        writeln!(
            stdout,
            "Nothing to schedule.\n\nNo prepared Reccursive work or checkout changes were found."
        )
        .map_err(crate::output_error)?;
        return Ok(());
    }

    let use_checkout = match (prepared.is_empty(), checkout_changes.is_empty()) {
        (true, false) => true,
        (false, true) => false,
        _ => pick::source()? == pick::Source::Checkout,
    };

    if use_checkout {
        let (repository, root) = here.expect("checkout changes require an enrolled repository");
        return schedule_checkout(session, stdout, &repository, &root, checkout_changes);
    }

    let group = pick::group(&prepared)?;
    let package = pick::package(group)?;

    for _ in 0..MAX_ATTEMPTS {
        let when = when::choose(session, group)?;
        review(stdout, group, package, when.instant_unix_ms)?;
        if !pick::confirm("Schedule it?")? {
            writeln!(stdout, "Not scheduled. No changes were made.")
                .map_err(crate::output_error)?;
            return Ok(());
        }
        match schedule_captured(session, group, package, when.instant_unix_ms) {
            Ok(slot) => return confirmation(stdout, group, package, slot),
            Err(failure) if failure.code == "invalid_request" => {
                writeln!(
                    stdout,
                    "\nThat time is no longer available.\n{}\n",
                    failure.message
                )
                .map_err(crate::output_error)?;
            }
            Err(failure) => return Err(failure),
        }
    }
    Err(CliFailure::new(
        EXIT_ACTION_REQUIRED,
        "no_valid_time",
        "No release time could be secured. Nothing was scheduled.",
    ))
}

fn git_repository_here(path: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Schedules what is sitting in the user's own working tree, split into as many independent,
/// independently timed batches as the person wants.
///
/// Each batch's snapshot is taken the moment it is built, so editing the checkout afterwards
/// cannot change what that batch will publish. Nothing becomes a real release, though, until the
/// whole plan — every batch built this session — is reviewed and confirmed together: building a
/// batch only ever captures it, and capturing is already the operation the single-batch flow used
/// to leave behind when somebody declined its confirmation, so a session that stops early leaves
/// nothing worse than that same, already-supported, retryable state. Their checkout is only ever
/// read.
fn schedule_checkout(
    session: &Session,
    stdout: &mut impl Write,
    repository: &reccursive_protocol::RepositoryView,
    root: &std::path::Path,
    changes: Vec<checkout::Change>,
) -> Result<(), CliFailure> {
    let _ = root;
    writeln!(stdout, "\n{}\n", checkout::summary_line(&changes)).map_err(crate::output_error)?;

    let delivery = delivery_for(repository);
    let timezone = repository_timezone(session, repository.id)?;

    let mut remaining = changes;
    let mut batches: Vec<checkout::Batch> = Vec::new();

    while !remaining.is_empty() {
        writeln!(stdout, "{}\n", checkout::summary_line(&remaining))
            .map_err(crate::output_error)?;
        let selected = checkout::select_files(&remaining)?;
        if selected.is_empty() {
            writeln!(
                stdout,
                "No files selected. Leaving the rest of your checkout alone.\n"
            )
            .map_err(crate::output_error)?;
            break;
        }
        let (chosen, rest) = checkout::partition(remaining, &selected);
        let title = checkout::ask_name()?;

        let pending_instants: Vec<i64> =
            batches.iter().map(|batch| batch.instant_unix_ms).collect();
        let when = match when::choose_for(session, repository.id, &timezone, &pending_instants) {
            Ok(when) => when,
            Err(failure) if failure.code == "cancelled" => {
                // A person can back out of naming a time without losing the batches they already
                // built; only the file selection they were mid-way through is abandoned.
                remaining = rest;
                break;
            }
            Err(failure) => return Err(failure),
        };

        let (feature_id, plan_revision, package_id) =
            checkout::capture(session, repository, &title, &chosen)?;
        batches.push(checkout::Batch {
            title,
            changes: chosen,
            instant_unix_ms: when.instant_unix_ms,
            feature_id,
            plan_revision,
            package_id,
        });
        remaining = rest;

        if remaining.is_empty() {
            break;
        }
        match pick::next_action(remaining.len()) {
            Ok(pick::NextAction::AnotherBatch) => {}
            Ok(pick::NextAction::LeaveRemaining) => break,
            Err(failure) if failure.code == "cancelled" => break,
            Err(failure) => return Err(failure),
        }
    }

    if batches.is_empty() {
        writeln!(stdout, "Not scheduled. No changes were made.").map_err(crate::output_error)?;
        return Ok(());
    }

    checkout::review_plan(stdout, &batches, &remaining, &delivery, &timezone)?;
    let question = if batches.len() == 1 {
        "Schedule this release?".to_owned()
    } else {
        format!("Schedule these {} releases?", batches.len())
    };
    if !pick::confirm(&question)? {
        writeln!(
            stdout,
            "Not scheduled. Every batch stays captured and ready — run `reccursive schedule` \
             again to pick up where you left off."
        )
        .map_err(crate::output_error)?;
        return Ok(());
    }

    let request = ScheduleCheckoutBatchesRequest {
        batches: batches
            .iter()
            .map(|batch| ScheduleCapturedWorkRequest {
                feature_id: batch.feature_id,
                plan_revision: batch.plan_revision,
                package_id: batch.package_id,
                package_revision: reccursive_protocol::Revision::FIRST,
                requested_at_unix_ms: Some(batch.instant_unix_ms),
                seed: 0,
            })
            .collect(),
    };
    match send(session, Command::ScheduleCheckoutBatches(request)) {
        Ok(ResponseData::CheckoutBatchesScheduled { slots }) => {
            checkout::confirmation_multi(stdout, &batches, &slots, &delivery)
        }
        Ok(_) => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return release times",
        )),
        Err(failure) => {
            writeln!(
                stdout,
                "\nNone of these releases were scheduled.\n{}\n\nEvery batch stays captured and \
                 ready — run `reccursive schedule` again.",
                failure.message
            )
            .map_err(crate::output_error)?;
            Err(failure)
        }
    }
}

/// The repository's schedule zone, which is the one a requested time is read in.
fn repository_timezone(
    session: &Session,
    repository_id: reccursive_protocol::RepositoryId,
) -> Result<String, CliFailure> {
    match send(session, Command::GetSchedulePolicy { repository_id })? {
        ResponseData::SchedulePolicy { policy } => Ok(policy.policy.timezone.as_str().to_owned()),
        _ => Ok("UTC".to_owned()),
    }
}

fn delivery_for(repository: &reccursive_protocol::RepositoryView) -> String {
    match repository.target_integration {
        TargetIntegration::DirectPush => format!("Direct → {}", branch(&repository.target)),
        TargetIntegration::PullRequest => match &repository.development_target {
            Some(development) => format!(
                "Pull request · {} → {}",
                branch(development),
                branch(&repository.target)
            ),
            None => format!("Pull request → {}", branch(&repository.target)),
        },
    }
}

/// A group's delivery strategy, in the words the person enrolling it chose.
fn delivery(group: &ReadyWorkGroup) -> String {
    match group.target_integration {
        TargetIntegration::DirectPush => {
            format!("Direct → {}", branch(&group.target))
        }
        TargetIntegration::PullRequest => match &group.development_target {
            Some(development) => format!(
                "Pull request · {} → {}",
                branch(development),
                branch(&group.target)
            ),
            None => format!("Pull request → {}", branch(&group.target)),
        },
    }
}

fn branch(target: &reccursive_protocol::TargetRef) -> &str {
    target
        .as_str()
        .strip_prefix("refs/heads/")
        .unwrap_or(target.as_str())
}

fn ready_work(session: &Session) -> Result<Vec<ReadyWorkGroup>, CliFailure> {
    match send(session, Command::ListReadyWork)? {
        ResponseData::ReadyWork { groups } => Ok(groups),
        _ => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return ready work",
        )),
    }
}

fn review(
    stdout: &mut impl Write,
    group: &ReadyWorkGroup,
    package: &reccursive_protocol::ReadyPackageView,
    instant_unix_ms: i64,
) -> Result<(), CliFailure> {
    writeln!(stdout, "\nReview\n").map_err(crate::output_error)?;
    writeln!(stdout, "{}", group.feature_goal).map_err(crate::output_error)?;
    // Every task in the package, named. A package carries what was captured together and ships
    // together, so there is nothing here the person did not already choose by choosing it.
    for name in &package.task_names {
        writeln!(stdout, "  {name}").map_err(crate::output_error)?;
    }
    writeln!(
        stdout,
        "\n{}\n{}\n",
        change_count(package.task_names.len()),
        delivery(group)
    )
    .map_err(crate::output_error)?;
    // The group's own schedule-policy zone: the same one the time was entered and validated in,
    // and the only source of truth for what the instant chosen a moment ago actually means.
    writeln!(
        stdout,
        "{}\n",
        human_time_in(instant_unix_ms, &group.timezone)
    )
    .map_err(crate::output_error)
}

fn change_count(count: usize) -> String {
    if count == 1 {
        "1 change".to_owned()
    } else {
        format!("{count} changes")
    }
}

/// Groups the captured work and selects its time in one request.
///
/// One call because they are one intention. Split in two, a grouping that succeeds followed by a
/// scheduling that fails leaves a release unit nobody asked for, and the work inside it stops
/// being offered while still sitting in the queue.
fn schedule_captured(
    session: &Session,
    group: &ReadyWorkGroup,
    package: &reccursive_protocol::ReadyPackageView,
    instant_unix_ms: i64,
) -> Result<reccursive_protocol::ScheduleSlotView, CliFailure> {
    let request = reccursive_protocol::ScheduleCapturedWorkRequest {
        feature_id: group.feature_id,
        plan_revision: group.plan_revision,
        package_id: package.package_id,
        package_revision: package.package_revision,
        requested_at_unix_ms: Some(instant_unix_ms),
        seed: 0,
    };
    match send(session, Command::ScheduleCapturedWork(request))? {
        ResponseData::ScheduleSlot { slot } => Ok(slot),
        _ => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return a release time",
        )),
    }
}

fn confirmation(
    stdout: &mut impl Write,
    group: &ReadyWorkGroup,
    package: &reccursive_protocol::ReadyPackageView,
    slot: reccursive_protocol::ScheduleSlotView,
) -> Result<(), CliFailure> {
    // The slot's own timezone, not the group's current policy zone: the zone actually in effect
    // when this instant was persisted, so a later policy change cannot change what is shown here.
    writeln!(
        stdout,
        "\n{}\n\n{}\n{}\n\n{} · {}\n\nReccursive will handle it automatically.",
        crate::paint(crate::ansi::GREEN, "✓ Scheduled"),
        group.feature_goal,
        human_time_in(slot.selected_at_unix_ms, &slot.timezone),
        change_count(package.task_names.len()),
        delivery(group),
    )
    .map_err(crate::output_error)
}
