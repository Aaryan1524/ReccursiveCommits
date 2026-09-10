use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Revision, TaskId};

/// Append-only task-revision decisions that invalidate stale package evidence.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionLifecycle {
    cancelled: BTreeSet<TaskId>,
    superseded_by: BTreeMap<TaskId, Revision>,
    published: BTreeSet<TaskId>,
    invalidated_tasks: BTreeSet<TaskId>,
}

impl RevisionLifecycle {
    pub fn mark_published(&mut self, task: TaskId) {
        self.published.insert(task);
    }
    pub fn cancel(&mut self, task: TaskId) -> Result<(), LifecycleError> {
        if self.published.contains(&task) {
            return Err(LifecycleError::PublishedImmutable { task });
        }
        self.cancelled.insert(task);
        self.invalidated_tasks.insert(task);
        Ok(())
    }
    pub fn supersede(&mut self, task: TaskId, replacement: Revision) -> Result<(), LifecycleError> {
        if self.published.contains(&task) {
            return Err(LifecycleError::PublishedImmutable { task });
        }
        self.superseded_by.insert(task, replacement);
        self.invalidated_tasks.insert(task);
        Ok(())
    }
    pub fn may_unlock(&self, task: TaskId) -> bool {
        !self.cancelled.contains(&task) && !self.superseded_by.contains_key(&task)
    }
    pub fn invalidated(&self, task: TaskId) -> bool {
        self.invalidated_tasks.contains(&task)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LifecycleError {
    #[error("published task {task} is immutable")]
    PublishedImmutable { task: TaskId },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_blocks_dependents_and_supersession_invalidates_evidence() {
        let cancelled = TaskId::new();
        let replaced = TaskId::new();
        let published = TaskId::new();
        let mut state = RevisionLifecycle::default();
        state.cancel(cancelled).unwrap();
        state
            .supersede(replaced, Revision::new(2).unwrap())
            .unwrap();
        state.mark_published(published);
        assert!(!state.may_unlock(cancelled));
        assert!(state.invalidated(replaced));
        assert!(matches!(
            state.cancel(published),
            Err(LifecycleError::PublishedImmutable { .. })
        ));
    }
}
