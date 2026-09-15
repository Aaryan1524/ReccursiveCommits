//! Selecting what to ship.

use inquire::{Confirm, MultiSelect, Select};
use reccursive_protocol::{ReadyWorkGroup, TaskId};

use crate::{CliFailure, EXIT_ACTION_REQUIRED};

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

fn cancelled() -> CliFailure {
    CliFailure::new(EXIT_SUCCESS_CODE, "cancelled", "Nothing was scheduled.")
}

// Cancelling is not a failure, but the flow still has to stop. Zero keeps scripts honest.
const EXIT_SUCCESS_CODE: u8 = crate::EXIT_SUCCESS;

/// Chooses which feature revision to ship from.
///
/// A release unit belongs to exactly one feature revision, so this choice is made before tasks
/// rather than filtered afterwards. Offering one flat list would invite a selection that cannot
/// become a single atomic release.
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
                super::change_count(group.tasks.len())
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

/// Multi-selects the work to ship together.
pub fn tasks(group: &ReadyWorkGroup) -> Result<Vec<TaskId>, CliFailure> {
    let labels: Vec<String> = group.tasks.iter().map(|task| task.name.clone()).collect();
    let chosen = answered(
        MultiSelect::new("Which changes should ship together?", labels.clone())
            .with_help_message("space to select, enter to continue")
            .prompt(),
    )?
    .ok_or_else(cancelled)?;
    Ok(chosen
        .iter()
        .filter_map(|label| {
            labels
                .iter()
                .position(|candidate| candidate == label)
                .map(|index| group.tasks[index].task_id)
        })
        .collect())
}

/// Adds the tasks the plan couples to the chosen ones.
///
/// `create_release_unit` applies this expansion regardless, so resolving it here is what makes the
/// review screen true rather than optimistic. Order follows the group so the review reads in plan
/// order rather than selection order.
pub fn with_coupled(group: &ReadyWorkGroup, chosen: &[TaskId]) -> Vec<TaskId> {
    let mut included: Vec<TaskId> = Vec::new();
    let mut frontier: Vec<TaskId> = chosen.to_vec();
    while let Some(task_id) = frontier.pop() {
        if included.contains(&task_id) {
            continue;
        }
        included.push(task_id);
        if let Some(task) = group.tasks.iter().find(|task| task.task_id == task_id) {
            // Coupling can chain: a task required by a selection may itself require another.
            frontier.extend(task.couples_with.iter().copied());
        }
    }
    group
        .tasks
        .iter()
        .map(|task| task.task_id)
        .filter(|task_id| included.contains(task_id))
        .collect()
}

pub fn confirm(question: &str) -> Result<bool, CliFailure> {
    Ok(answered(Confirm::new(question).with_default(true).prompt())?.unwrap_or(false))
}
