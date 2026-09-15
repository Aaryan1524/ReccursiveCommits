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
    if let [only] = group.packages.as_slice() {
        return Ok(only);
    }
    let labels: Vec<String> = group.packages.iter().map(describe).collect();
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
    Ok(&group.packages[index])
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
