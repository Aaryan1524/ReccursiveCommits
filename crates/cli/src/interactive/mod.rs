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
    ScheduleUnitRequest, TargetIntegration,
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
    let package = pick::package(group)?;

    // The early check is advisory: it holds nothing and reserves nothing, so between answering
    // the prompts and confirming, another release can be scheduled nearby and take the time away.
    // The authoritative refusal comes from scheduling itself, and it is not a contradiction — it
    // is a true answer about a queue that changed. When that happens the person is told what
    // changed and asked again rather than dropped out of the wizard.
    for _ in 0..MAX_ATTEMPTS {
        let when = when::choose(session, group)?;

        review(stdout, group, package, when.instant_unix_ms)?;
        if !pick::confirm("Schedule it?")? {
            writeln!(stdout, "Not scheduled. No changes were made.")
                .map_err(crate::output_error)?;
            return Ok(());
        }

        // Grouping is idempotent for an identical task set, so repeating this after a refused
        // time returns the same unit rather than creating a second one.
        let unit = create_unit(session, group, package)?;
        match schedule_unit(session, package, &unit, when.instant_unix_ms) {
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
    package: &reccursive_protocol::ReadyPackageView,
) -> Result<reccursive_protocol::ReleaseUnitView, CliFailure> {
    let request = CreateReleaseUnitRequest {
        release_unit_id: ReleaseUnitId::new(),
        feature_id: group.feature_id,
        plan_revision: group.plan_revision,
        // Exactly the package's tasks. A unit whose set differs from its package's is refused
        // when it is scheduled, so the package decides this rather than the person.
        task_ids: package.task_ids.iter().copied().collect(),
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
    package: &reccursive_protocol::ReadyPackageView,
    unit: &reccursive_protocol::ReleaseUnitView,
    instant_unix_ms: i64,
) -> Result<reccursive_protocol::ScheduleSlotView, CliFailure> {
    let request = ScheduleUnitRequest {
        release_unit_id: unit.unit_id,
        package_id: package.package_id,
        package_revision: package.package_revision,
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

fn confirmation(
    stdout: &mut impl Write,
    group: &ReadyWorkGroup,
    package: &reccursive_protocol::ReadyPackageView,
    slot: reccursive_protocol::ScheduleSlotView,
) -> Result<(), CliFailure> {
    writeln!(
        stdout,
        "\n✓ Scheduled\n\n{}\n{}\n\n{} · {}\n\nReccursive will handle it automatically.",
        group.feature_goal,
        human_time(slot.selected_at_unix_ms),
        change_count(package.task_names.len()),
        delivery(group),
    )
    .map_err(crate::output_error)
}
