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

mod pick;
mod when;

pub use when::to_instant;

/// How many times to re-ask before giving up, so a contended queue cannot loop forever.
const MAX_ATTEMPTS: usize = 5;

use std::io::Write;

use reccursive_protocol::{
    Command, CreateReleaseUnitRequest, ReadyWorkGroup, ReleaseUnitId, ResponseData,
    ScheduleUnitRequest, TargetIntegration, TaskId,
};

use crate::{CliFailure, EXIT_ACTION_REQUIRED, Session, human_time, send};

/// Runs the wizard end to end, or explains why it cannot.
pub fn schedule(session: &Session, stdout: &mut impl Write) -> Result<(), CliFailure> {
    let groups = ready_work(session)?;
    if groups.is_empty() {
        writeln!(
            stdout,
            "Nothing is ready to schedule.\n\n\
             Work becomes ready once it has been captured and its checks have passed:\n  \
             reccursive workspace create <feature> --revision <n>\n  \
             reccursive package capture <feature> --revision <n> --task <task>"
        )
        .map_err(crate::output_error)?;
        return Ok(());
    }

    let group = pick::group(&groups)?;
    let selected = pick::tasks(group)?;
    if selected.is_empty() {
        writeln!(stdout, "Nothing selected. No changes were made.").map_err(crate::output_error)?;
        return Ok(());
    }

    // Coupling is applied by the queue whether or not anyone asked for it, so it is resolved and
    // shown here rather than discovered on the review screen as an unexplained extra line.
    let included = pick::with_coupled(group, &selected);

    // The early check is advisory: it holds nothing and reserves nothing, so between answering
    // the prompts and confirming, another release can be scheduled nearby and take the time away.
    // The authoritative refusal comes from scheduling itself, and it is not a contradiction — it
    // is a true answer about a queue that changed. When that happens the person is told what
    // changed and asked again rather than dropped out of the wizard.
    for _ in 0..MAX_ATTEMPTS {
        let when = when::choose(session, group)?;

        review(stdout, group, &included, &selected, when.instant_unix_ms)?;
        if !pick::confirm("Schedule it?")? {
            writeln!(stdout, "Not scheduled. No changes were made.")
                .map_err(crate::output_error)?;
            return Ok(());
        }

        // Grouping is idempotent for an identical task set, so repeating this after a refused
        // time returns the same unit rather than creating a second one.
        let unit = create_unit(session, group, &included)?;
        match schedule_unit(session, group, &unit, when.instant_unix_ms) {
            Ok(slot) => return confirmation(stdout, group, &included, slot),
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
    included: &[TaskId],
    chosen: &[TaskId],
    instant_unix_ms: i64,
) -> Result<(), CliFailure> {
    writeln!(stdout, "\nReview\n").map_err(crate::output_error)?;
    writeln!(stdout, "{}", group.feature_goal).map_err(crate::output_error)?;
    for task_id in included {
        let name = group
            .tasks
            .iter()
            .find(|task| task.task_id == *task_id)
            .map_or("(unnamed)", |task| task.name.as_str());
        // A task nobody ticked is one the plan couples to one they did. Saying so is the whole
        // point: work appearing in a release that nobody chose is the surprise to avoid.
        if chosen.contains(task_id) {
            writeln!(stdout, "  {name}").map_err(crate::output_error)?;
        } else {
            writeln!(stdout, "  {name}  (required by your selection)")
                .map_err(crate::output_error)?;
        }
    }
    writeln!(
        stdout,
        "\n{}\n{}\n",
        change_count(included.len()),
        delivery(group)
    )
    .map_err(crate::output_error)?;
    writeln!(stdout, "{}\n", human_time(instant_unix_ms)).map_err(crate::output_error)
}

fn change_count(count: usize) -> String {
    if count == 1 {
        "1 change".to_owned()
    } else {
        format!("{count} changes")
    }
}

fn create_unit(
    session: &Session,
    group: &ReadyWorkGroup,
    included: &[TaskId],
) -> Result<reccursive_protocol::ReleaseUnitView, CliFailure> {
    let request = CreateReleaseUnitRequest {
        release_unit_id: ReleaseUnitId::new(),
        feature_id: group.feature_id,
        plan_revision: group.plan_revision,
        task_ids: included.iter().copied().collect(),
    };
    match send(session, Command::CreateReleaseUnit(request))? {
        ResponseData::ReleaseUnitCreated { unit } => Ok(unit),
        _ => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return the grouped work",
        )),
    }
}

fn schedule_unit(
    session: &Session,
    group: &ReadyWorkGroup,
    unit: &reccursive_protocol::ReleaseUnitView,
    instant_unix_ms: i64,
) -> Result<reccursive_protocol::ScheduleSlotView, CliFailure> {
    let package = package_for(session, group, unit)?;
    let request = ScheduleUnitRequest {
        release_unit_id: unit.unit_id,
        package_id: package.0,
        package_revision: package.1,
        // Unused when a time is requested, but the field is not optional and a fixed value keeps
        // a repeated wizard run reproducible rather than accidentally drawing a different slot.
        seed: 0,
        requested_at_unix_ms: Some(instant_unix_ms),
    };
    match send(session, Command::ScheduleUnit(request))? {
        ResponseData::ScheduleSlot { slot } => Ok(slot),
        _ => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return a release time",
        )),
    }
}

/// Finds the package carrying this unit's work, so the person never has to know it exists.
fn package_for(
    session: &Session,
    group: &ReadyWorkGroup,
    unit: &reccursive_protocol::ReleaseUnitView,
) -> Result<
    (
        reccursive_protocol::PackageId,
        reccursive_protocol::Revision,
    ),
    CliFailure,
> {
    let ResponseData::FeatureStatus { status } = send(
        session,
        Command::GetFeatureStatus {
            feature_id: group.feature_id,
            revision: Some(group.plan_revision),
        },
    )?
    else {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return the feature's status",
        ));
    };
    status
        .tasks
        .iter()
        .find(|task| unit.task_ids.contains(&task.task_id) && task.package_id.is_some())
        .and_then(|task| task.package_id)
        .map(|package_id| (package_id, reccursive_protocol::Revision::FIRST))
        .ok_or_else(|| {
            CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "package_missing",
                "this work has no captured package; capture it before scheduling",
            )
        })
}

fn confirmation(
    stdout: &mut impl Write,
    group: &ReadyWorkGroup,
    included: &[TaskId],
    slot: reccursive_protocol::ScheduleSlotView,
) -> Result<(), CliFailure> {
    writeln!(
        stdout,
        "\n✓ Scheduled\n\n{}\n{}\n\n{} · {}\n\nYou can go offline.",
        group.feature_goal,
        human_time(slot.selected_at_unix_ms),
        change_count(included.len()),
        delivery(group),
    )
    .map_err(crate::output_error)
}
