use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ReleaseUnitId, TaskId};

/// An indivisible set of tasks that must be verified and released together.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseUnit {
    pub id: ReleaseUnitId,
    pub task_ids: BTreeSet<TaskId>,
    pub required_checks: BTreeSet<String>,
}

impl ReleaseUnit {
    /// Refuses a unit that would leave a selected task without one of its required task inputs.
    pub fn new(
        id: ReleaseUnitId,
        task_ids: BTreeSet<TaskId>,
        dependencies: &BTreeMap<TaskId, BTreeSet<TaskId>>,
        required_checks: BTreeSet<String>,
    ) -> Result<Self, ReleaseUnitError> {
        if task_ids.is_empty() {
            return Err(ReleaseUnitError::Empty);
        }
        if required_checks.iter().any(|check| check.trim().is_empty()) {
            return Err(ReleaseUnitError::EmptyCheck);
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
        Ok(Self {
            id,
            task_ids,
            required_checks,
        })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ReleaseUnitError {
    #[error("release unit must contain at least one task")]
    Empty,
    #[error("release unit checks must be named")]
    EmptyCheck,
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
                BTreeSet::from(["integration".into()])
            ),
            Err(ReleaseUnitError::MissingPrerequisite { .. })
        ));
        assert!(
            ReleaseUnit::new(
                ReleaseUnitId::new(),
                BTreeSet::from([api, caller]),
                &dependencies,
                BTreeSet::from(["integration".into()])
            )
            .is_ok()
        );
    }
}
