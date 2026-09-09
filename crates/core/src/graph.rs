use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{TargetMilestone, TaskId};

/// One task and the prerequisite milestones it needs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskDefinition {
    pub id: TaskId,
    pub name: String,
    pub dependencies: BTreeMap<TaskId, TargetMilestone>,
}

impl TaskDefinition {
    pub fn new(
        id: TaskId,
        name: impl Into<String>,
        dependencies: BTreeMap<TaskId, TargetMilestone>,
    ) -> Result<Self, TaskDefinitionError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(TaskDefinitionError::EmptyName { task_id: id });
        }
        Ok(Self {
            id,
            name,
            dependencies,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TaskDefinitionError {
    #[error("task {task_id} must have a non-empty name")]
    EmptyName { task_id: TaskId },
}

/// Validated dependency graph with deterministic traversal order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskGraph {
    tasks: BTreeMap<TaskId, TaskDefinition>,
    topological_order: Vec<TaskId>,
}

impl TaskGraph {
    pub fn new(tasks: impl IntoIterator<Item = TaskDefinition>) -> Result<Self, GraphError> {
        let mut by_id = BTreeMap::new();
        for task in tasks {
            let task_id = task.id;
            if by_id.insert(task_id, task).is_some() {
                return Err(GraphError::DuplicateTask { task_id });
            }
        }

        for task in by_id.values() {
            for dependency_id in task.dependencies.keys() {
                if dependency_id == &task.id {
                    return Err(GraphError::SelfDependency { task_id: task.id });
                }
                if !by_id.contains_key(dependency_id) {
                    return Err(GraphError::UnknownDependency {
                        task_id: task.id,
                        dependency_id: *dependency_id,
                    });
                }
            }
        }

        let topological_order = topological_order(&by_id)?;
        Ok(Self {
            tasks: by_id,
            topological_order,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&TaskDefinition> {
        self.tasks.get(&id)
    }

    #[must_use]
    pub fn topological_order(&self) -> &[TaskId] {
        &self.topological_order
    }
}

fn topological_order(tasks: &BTreeMap<TaskId, TaskDefinition>) -> Result<Vec<TaskId>, GraphError> {
    let mut remaining_dependencies: BTreeMap<TaskId, usize> = tasks
        .iter()
        .map(|(task_id, task)| (*task_id, task.dependencies.len()))
        .collect();
    let mut dependents: BTreeMap<TaskId, BTreeSet<TaskId>> = BTreeMap::new();
    for task in tasks.values() {
        for dependency_id in task.dependencies.keys() {
            dependents
                .entry(*dependency_id)
                .or_default()
                .insert(task.id);
        }
    }

    let mut ready: BTreeSet<TaskId> = remaining_dependencies
        .iter()
        .filter_map(|(task_id, count)| (*count == 0).then_some(*task_id))
        .collect();
    let mut ordered = Vec::with_capacity(tasks.len());
    while let Some(task_id) = ready.pop_first() {
        ordered.push(task_id);
        if let Some(task_dependents) = dependents.get(&task_id) {
            for dependent_id in task_dependents {
                let count = remaining_dependencies
                    .get_mut(dependent_id)
                    .expect("validated dependent must be present");
                *count -= 1;
                if *count == 0 {
                    ready.insert(*dependent_id);
                }
            }
        }
    }

    if ordered.len() != tasks.len() {
        let tasks = remaining_dependencies
            .into_iter()
            .filter_map(|(task_id, count)| (count > 0).then_some(task_id))
            .collect();
        return Err(GraphError::DependencyCycle { tasks });
    }
    Ok(ordered)
}

/// Invalid task graph.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum GraphError {
    #[error("task {task_id} appears more than once")]
    DuplicateTask { task_id: TaskId },
    #[error("task {task_id} cannot depend on itself")]
    SelfDependency { task_id: TaskId },
    #[error("task {task_id} depends on unknown task {dependency_id}")]
    UnknownDependency {
        task_id: TaskId,
        dependency_id: TaskId,
    },
    #[error("dependency cycle prevents scheduling tasks {tasks:?}")]
    DependencyCycle { tasks: Vec<TaskId> },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: TaskId, dependencies: &[TaskId]) -> TaskDefinition {
        TaskDefinition::new(
            id,
            format!("Task {id}"),
            dependencies
                .iter()
                .copied()
                .map(|dependency| (dependency, TargetMilestone::TargetPublished))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn graph_orders_dependencies_before_dependents() {
        let schema = TaskId::new();
        let api = TaskId::new();
        let client = TaskId::new();
        let graph = TaskGraph::new([
            task(client, &[api]),
            task(schema, &[]),
            task(api, &[schema]),
        ])
        .unwrap();
        assert_eq!(graph.topological_order(), &[schema, api, client]);
    }

    #[test]
    fn graph_rejects_missing_dependencies() {
        let task_id = TaskId::new();
        let missing = TaskId::new();
        assert_eq!(
            TaskGraph::new([task(task_id, &[missing])]),
            Err(GraphError::UnknownDependency {
                task_id,
                dependency_id: missing,
            })
        );
    }

    #[test]
    fn graph_rejects_cycles_with_affected_tasks() {
        let left = TaskId::new();
        let right = TaskId::new();
        let error = TaskGraph::new([task(left, &[right]), task(right, &[left])]).unwrap_err();
        let GraphError::DependencyCycle { tasks } = error else {
            panic!("expected a dependency cycle");
        };
        assert_eq!(
            tasks.into_iter().collect::<BTreeSet<_>>(),
            [left, right].into()
        );
    }

    #[test]
    fn graph_rejects_duplicate_and_self_references() {
        let duplicate = TaskId::new();
        assert!(matches!(
            TaskGraph::new([task(duplicate, &[]), task(duplicate, &[])]),
            Err(GraphError::DuplicateTask { .. })
        ));

        let self_referencing = TaskId::new();
        assert_eq!(
            TaskGraph::new([task(self_referencing, &[self_referencing])]),
            Err(GraphError::SelfDependency {
                task_id: self_referencing
            })
        );
    }
}
