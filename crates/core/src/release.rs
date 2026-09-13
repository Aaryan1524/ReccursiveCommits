use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ReleaseUnitId, TaskId};

/// An indivisible set of tasks that must be verified and released together.
///
/// A unit names *what* ships together. It deliberately does not name what must pass first: the
/// checks a release runs belong to the repository and are registered when it is enrolled, so that
/// what verifies a change is never something the change itself supplied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseUnit {
    pub id: ReleaseUnitId,
    pub task_ids: BTreeSet<TaskId>,
}

impl ReleaseUnit {
    /// Refuses a unit that would leave a selected task without one of its required task inputs.
    pub fn new(
        id: ReleaseUnitId,
        task_ids: BTreeSet<TaskId>,
        dependencies: &BTreeMap<TaskId, BTreeSet<TaskId>>,
    ) -> Result<Self, ReleaseUnitError> {
        if task_ids.is_empty() {
            return Err(ReleaseUnitError::Empty);
        }
        for task in &task_ids {
            let missing: Vec<_> = dependencies
                .get(task)
                .into_iter()
                .flatten()
                .filter(|dependency| !task_ids.contains(dependency))
                .copied()
                .collect();
            if !missing.is_empty() {
                return Err(ReleaseUnitError::MissingPrerequisite {
                    task: *task,
                    missing,
                });
            }
        }
        Ok(Self { id, task_ids })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ReleaseUnitError {
    #[error("release unit must contain at least one task")]
    Empty,
    #[error("release unit task {task} omits required prerequisite task(s) {missing:?}")]
    MissingPrerequisite { task: TaskId, missing: Vec<TaskId> },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caller_cannot_be_scheduled_without_its_api_prerequisite() {
        let api = TaskId::new();
        let caller = TaskId::new();
        let dependencies = BTreeMap::from([(caller, BTreeSet::from([api]))]);
        assert!(matches!(
            ReleaseUnit::new(
                ReleaseUnitId::new(),
                BTreeSet::from([caller]),
                &dependencies,
            ),
            Err(ReleaseUnitError::MissingPrerequisite { .. })
        ));
        assert!(
            ReleaseUnit::new(
                ReleaseUnitId::new(),
                BTreeSet::from([api, caller]),
                &dependencies,
            )
            .is_ok()
        );
    }
}
