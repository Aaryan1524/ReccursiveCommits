use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    FeatureId, GraphError, RepositoryId, Revision, TargetRef, TaskDefinition, TaskGraph, TaskId,
};

/// Current portable feature-plan document version.
pub const PLAN_SCHEMA_VERSION: u16 = 1;

/// A versioned feature plan imported from an agent or another client.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeaturePlan {
    pub schema_version: u16,
    pub feature_id: FeatureId,
    pub revision: Revision,
    pub repository_id: RepositoryId,
    pub goal: String,
    pub target: TargetRef,
    pub sealed: bool,
    pub phases: Vec<PlanPhase>,
}

impl FeaturePlan {
    /// Validates the complete document before it crosses a persistence boundary.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(PlanError::UnsupportedSchema {
                received: self.schema_version,
                supported: PLAN_SCHEMA_VERSION,
            });
        }
        validate_text("goal", &self.goal, 5_000)?;
        if self.phases.is_empty() {
            return Err(PlanError::NoPhases);
        }

        let mut phase_ids = BTreeSet::new();
        let mut definitions = Vec::new();
        for phase in &self.phases {
            validate_key("phase", &phase.id)?;
            validate_text("phase name", &phase.name, 200)?;
            if !phase_ids.insert(phase.id.clone()) {
                return Err(PlanError::DuplicatePhase {
                    phase_id: phase.id.clone(),
                });
            }
            if phase.tasks.is_empty() {
                return Err(PlanError::EmptyPhase {
                    phase_id: phase.id.clone(),
                });
            }
            for task in &phase.tasks {
                validate_text("task name", &task.name, 300)?;
                if task.acceptance_checks.is_empty() {
                    return Err(PlanError::NoAcceptanceChecks { task_id: task.id });
                }
                let mut check_ids = BTreeSet::new();
                for check in &task.acceptance_checks {
                    validate_key("acceptance check", &check.id)?;
                    validate_text("acceptance check description", &check.description, 1_000)?;
                    if !check_ids.insert(check.id.clone()) {
                        return Err(PlanError::DuplicateAcceptanceCheck {
                            task_id: task.id,
                            check_id: check.id.clone(),
                        });
                    }
                }
                definitions.push(
                    TaskDefinition::new(task.id, task.name.clone(), task.dependencies.clone())
                        .map_err(|error| PlanError::InvalidTask(error.to_string()))?,
                );
            }
        }
        if definitions.is_empty() {
            return Err(PlanError::NoTasks);
        }
        TaskGraph::new(definitions).map_err(PlanError::InvalidGraph)?;
        Ok(())
    }

    /// Returns task IDs in deterministic dependency order after validation.
    pub fn task_order(&self) -> Result<Vec<TaskId>, PlanError> {
        self.validate()?;
        let definitions = self.phases.iter().flat_map(|phase| {
            phase.tasks.iter().map(|task| {
                TaskDefinition::new(task.id, task.name.clone(), task.dependencies.clone())
                    .expect("validated task must remain valid")
            })
        });
        Ok(TaskGraph::new(definitions)
            .expect("validated graph must remain valid")
            .topological_order()
            .to_vec())
    }
}

/// An ordered delivery phase in a feature plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanPhase {
    pub id: String,
    pub name: String,
    pub tasks: Vec<PlanTask>,
}

/// A planned task with dependency milestones and observable acceptance checks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    pub id: TaskId,
    pub name: String,
    #[serde(default)]
    pub dependencies: std::collections::BTreeMap<TaskId, crate::TargetMilestone>,
    pub acceptance_checks: Vec<AcceptanceCheck>,
}

/// A human-readable outcome assertion. Executable commands are configured separately.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCheck {
    pub id: String,
    pub description: String,
}

fn validate_key(kind: &'static str, value: &str) -> Result<(), PlanError> {
    if value.is_empty()
        || value.len() > 64
        || !value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || "_-".contains(character)
        })
        || !value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    {
        return Err(PlanError::InvalidKey {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_text(kind: &'static str, value: &str, maximum: usize) -> Result<(), PlanError> {
    if value.trim().is_empty() {
        return Err(PlanError::EmptyText { kind });
    }
    if value.len() > maximum {
        return Err(PlanError::TextTooLong { kind, maximum });
    }
    Ok(())
}

/// Invalid portable feature-plan document.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PlanError {
    #[error("plan schema {received} is unsupported; this build supports schema {supported}")]
    UnsupportedSchema { received: u16, supported: u16 },
    #[error("plan must contain at least one phase")]
    NoPhases,
    #[error("plan must contain at least one task")]
    NoTasks,
    #[error("phase {phase_id:?} contains no tasks")]
    EmptyPhase { phase_id: String },
    #[error("phase {phase_id:?} appears more than once")]
    DuplicatePhase { phase_id: String },
    #[error("task {task_id} must contain at least one acceptance check")]
    NoAcceptanceChecks { task_id: TaskId },
    #[error("task {task_id} repeats acceptance check {check_id:?}")]
    DuplicateAcceptanceCheck { task_id: TaskId, check_id: String },
    #[error("{kind} key {value:?} must be 1-64 lowercase letters, digits, underscores, or hyphens")]
    InvalidKey { kind: &'static str, value: String },
    #[error("{kind} must not be empty")]
    EmptyText { kind: &'static str },
    #[error("{kind} must not exceed {maximum} bytes")]
    TextTooLong { kind: &'static str, maximum: usize },
    #[error("invalid task: {0}")]
    InvalidTask(String),
    #[error(transparent)]
    InvalidGraph(GraphError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::TargetMilestone;

    fn plan() -> FeaturePlan {
        let schema = TaskId::new();
        let api = TaskId::new();
        FeaturePlan {
            schema_version: PLAN_SCHEMA_VERSION,
            feature_id: FeatureId::new(),
            revision: Revision::FIRST,
            repository_id: RepositoryId::new(),
            goal: "Ship a usable feature".into(),
            target: TargetRef::new("refs/heads/main").unwrap(),
            sealed: true,
            phases: vec![PlanPhase {
                id: "delivery".into(),
                name: "Delivery".into(),
                tasks: vec![
                    PlanTask {
                        id: schema,
                        name: "Add schema".into(),
                        dependencies: BTreeMap::new(),
                        acceptance_checks: vec![AcceptanceCheck {
                            id: "migration".into(),
                            description: "Migration completes".into(),
                        }],
                    },
                    PlanTask {
                        id: api,
                        name: "Add API".into(),
                        dependencies: [(schema, TargetMilestone::Captured)].into(),
                        acceptance_checks: vec![AcceptanceCheck {
                            id: "response".into(),
                            description: "API returns the new field".into(),
                        }],
                    },
                ],
            }],
        }
    }

    #[test]
    fn complete_plan_is_dependency_ordered() {
        let plan = plan();
        assert!(plan.validate().is_ok());
        let order = plan.task_order().unwrap();
        assert_eq!(
            order,
            vec![plan.phases[0].tasks[0].id, plan.phases[0].tasks[1].id]
        );
    }

    #[test]
    fn incomplete_and_cyclic_plans_are_rejected() {
        let mut incomplete = plan();
        incomplete.phases[0].tasks[0].acceptance_checks.clear();
        assert!(matches!(
            incomplete.validate(),
            Err(PlanError::NoAcceptanceChecks { .. })
        ));

        let mut cyclic = plan();
        let first = cyclic.phases[0].tasks[0].id;
        let second = cyclic.phases[0].tasks[1].id;
        cyclic.phases[0].tasks[0]
            .dependencies
            .insert(second, TargetMilestone::Captured);
        assert!(matches!(
            cyclic.validate(),
            Err(PlanError::InvalidGraph(GraphError::DependencyCycle { .. }))
        ));
        assert_ne!(first, second);
    }

    #[test]
    fn unknown_fields_are_rejected_during_import() {
        let mut value = serde_json::to_value(plan()).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<FeaturePlan>(value).is_err());
    }
}
