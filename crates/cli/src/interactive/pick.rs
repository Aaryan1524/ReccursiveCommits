//! Choosing what to ship.

use inquire::{Confirm, Select};
use reccursive_protocol::{ReadyPackageView, ReadyWorkGroup};

use crate::{CliFailure, EXIT_ACTION_REQUIRED, EXIT_SUCCESS};

/// Turns an interrupted prompt into an ordinary "nothing happened", not a failure.
///
/// Ctrl-C at a prompt is a person deciding not to; reporting it as an error would make an
/// abandoned wizard look like a broken one.
fn answered<T>(outcome: Result<T, inquire::InquireError>) -> Result<Option<T>, CliFailure> {
    match outcome {
        Ok(value) => Ok(Some(value)),
        Err(inquire::InquireError::OperationCanceled)
        | Err(inquire::InquireError::OperationInterrupted) => Ok(None),
        Err(error) => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "prompt_failed",
            error.to_string(),
        )),
    }
}

/// Cancelling is a decision, not a fault, so it leaves with a success code.
fn cancelled() -> CliFailure {
    CliFailure::new(EXIT_SUCCESS, "cancelled", "Nothing was scheduled.")
}

/// Chooses which feature revision to ship from.
///
/// A release unit belongs to exactly one feature revision, so this choice comes before the work
/// rather than being filtered afterwards.
pub fn group(groups: &[ReadyWorkGroup]) -> Result<&ReadyWorkGroup, CliFailure> {
    if let [only] = groups {
        return Ok(only);
    }
    let labels: Vec<String> = groups
        .iter()
        .map(|group| {
            format!(
                "{}  ({})",
                group.feature_goal,
                super::change_count(group.packages.iter().map(|p| p.task_ids.len()).sum())
            )
        })
        .collect();
    let chosen = answered(Select::new("What are you shipping?", labels.clone()).prompt())?
        .ok_or_else(cancelled)?;
    let index = labels
        .iter()
        .position(|label| *label == chosen)
        .expect("the chosen label came from this list");
    Ok(&groups[index])
}

/// Chooses which captured work to ship.
///
/// One package, not a selection of tasks. What ships together was decided when the work was
/// captured: a release unit's tasks must equal a package's exactly, so a pick-your-own-tasks
/// prompt would offer combinations that cannot be scheduled — and would only say so after the
/// person had reviewed and confirmed them.
pub fn package(group: &ReadyWorkGroup) -> Result<&ReadyPackageView, CliFailure> {
    // Blocked work is shown but not offered. Hiding it would leave somebody hunting for a change
    // they know they captured; offering it would let them pick something scheduling must refuse,
    // which is what a real trial did before this.
    let (selectable, blocked): (Vec<_>, Vec<_>) = group
        .packages
        .iter()
        .partition(|package| package.blocked_by.is_none());
    if !blocked.is_empty() {
        println!("\nWaiting on other work:");
        for package in &blocked {
            let blocking = package
                .blocked_by
                .as_ref()
                .expect("partitioned on this being present");
            println!(
                "  {}\n    waiting for: {} → {}",
                describe(package),
                blocking.name,
                milestone_label(blocking.milestone)
            );
        }
        println!();
    }
    match selectable.as_slice() {
        [] => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "all_blocked",
            "Every change here is waiting on work that has not been published yet.",
        )),
        [only] => Ok(only),
        many => {
            let labels: Vec<String> = many.iter().map(|package| describe(package)).collect();
            let chosen = answered(
                Select::new("Which change should ship?", labels.clone())
                    .with_help_message("each entry was captured together and ships together")
                    .prompt(),
            )?
            .ok_or_else(cancelled)?;
            let index = labels
                .iter()
                .position(|label| *label == chosen)
                .expect("the chosen label came from this list");
            Ok(many[index])
        }
    }
}

/// Says what a prerequisite still has to reach, without the enum's own vocabulary.
fn milestone_label(milestone: reccursive_protocol::TargetMilestone) -> &'static str {
    match milestone {
        reccursive_protocol::TargetMilestone::Captured => "captured",
        reccursive_protocol::TargetMilestone::DevelopmentAvailable => "available for review",
        reccursive_protocol::TargetMilestone::TargetPublished => "published",
    }
}

/// Names a package by the work in it, never by its identifier.
pub fn describe(package: &ReadyPackageView) -> String {
    match package.task_names.as_slice() {
        [] => "(no named work)".to_owned(),
        [only] => only.clone(),
        names => format!("{} (+{} more)", names[0], names.len() - 1),
    }
}

pub fn confirm(question: &str) -> Result<bool, CliFailure> {
    Ok(answered(Confirm::new(question).with_default(true).prompt())?.unwrap_or(false))
}

/// What to do after one batch of a multi-batch checkout session has been built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NextAction {
    AnotherBatch,
    LeaveRemaining,
}

/// Asks whether to keep splitting the checkout into more batches, or stop here.
///
/// Offered only while files remain unclaimed by any batch — a session with nothing left to
/// assign has nothing to ask about.
pub fn next_action(remaining: usize) -> Result<NextAction, CliFailure> {
    const AGAIN: &str = "Create another scheduled batch";
    const LEAVE: &str = "Leave remaining changes alone";
    let changes = if remaining == 1 {
        "1 change remains".to_owned()
    } else {
        format!("{remaining} changes remain")
    };
    let chosen =
        answered(Select::new(&format!("{changes}. What next?"), vec![AGAIN, LEAVE]).prompt())?
            .ok_or_else(cancelled)?;
    Ok(if chosen == AGAIN {
        NextAction::AnotherBatch
    } else {
        NextAction::LeaveRemaining
    })
}

/// Where the work to schedule should come from, when both exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Prepared,
    Checkout,
}

/// Asks which of the two, only when both are actually available.
pub fn source() -> Result<Source, CliFailure> {
    const PREPARED: &str = "Prepared work";
    const CHECKOUT: &str = "Current checkout changes";
    let chosen =
        answered(Select::new("What do you want to schedule?", vec![PREPARED, CHECKOUT]).prompt())?
            .ok_or_else(cancelled)?;
    Ok(if chosen == CHECKOUT {
        Source::Checkout
    } else {
        Source::Prepared
    })
}
