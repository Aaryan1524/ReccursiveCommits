use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{PackageId, RepositoryId, Revision, TaskId};

/// How completed work becomes available.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationMode {
    /// Keep a package local and create its public commit at the release opportunity.
    ScheduledCreation,
    /// Publish early to a development branch, then integrate into the target later.
    ImmediateAvailability,
}

/// A milestone a dependency must reach before its dependent becomes eligible.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetMilestone {
    Captured,
    DevelopmentAvailable,
    TargetPublished,
}

/// A fully-qualified local branch ref.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TargetRef(String);

impl TargetRef {
    /// Validates the minimum domain-level shape for a target branch.
    pub fn new(value: impl Into<String>) -> Result<Self, PolicyError> {
        let value = value.into();
        let branch =
            value
                .strip_prefix("refs/heads/")
                .ok_or_else(|| PolicyError::InvalidTargetRef {
                    value: value.clone(),
                    detail: "target must start with refs/heads/",
                })?;
        if branch.is_empty() || branch.starts_with('-') || branch.contains("..") {
            return Err(PolicyError::InvalidTargetRef {
                value,
                detail: "branch name is empty or contains a disallowed sequence",
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TargetRef {
    type Error = PolicyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<TargetRef> for String {
    fn from(value: TargetRef) -> Self {
        value.0
    }
}

/// Identifies the exact repository policy used by another record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyRef {
    pub repository_id: RepositoryId,
    pub revision: Revision,
}

/// Versioned standing authorization settings for one repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPolicy {
    pub repository_id: RepositoryId,
    pub revision: Revision,
    pub publication_mode: PublicationMode,
    pub target: TargetRef,
    pub development_target: Option<TargetRef>,
}

impl RepositoryPolicy {
    pub fn new(
        repository_id: RepositoryId,
        revision: Revision,
        publication_mode: PublicationMode,
        target: TargetRef,
        development_target: Option<TargetRef>,
    ) -> Result<Self, PolicyError> {
        match publication_mode {
            PublicationMode::ScheduledCreation if development_target.is_some() => {
                return Err(PolicyError::UnexpectedDevelopmentTarget);
            }
            PublicationMode::ImmediateAvailability if development_target.is_none() => {
                return Err(PolicyError::MissingDevelopmentTarget);
            }
            _ => {}
        }
        if development_target.as_ref() == Some(&target) {
            return Err(PolicyError::TargetsMustDiffer);
        }
        Ok(Self {
            repository_id,
            revision,
            publication_mode,
            target,
            development_target,
        })
    }

    #[must_use]
    pub const fn reference(&self) -> PolicyRef {
        PolicyRef {
            repository_id: self.repository_id,
            revision: self.revision,
        }
    }
}

/// Immutable package revision tied to a policy and one or more tasks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PackageRevision {
    pub package_id: PackageId,
    pub revision: Revision,
    pub policy: PolicyRef,
    pub task_ids: BTreeSet<TaskId>,
    pub required_milestone: TargetMilestone,
}

impl PackageRevision {
    pub fn new(
        package_id: PackageId,
        revision: Revision,
        policy: PolicyRef,
        task_ids: BTreeSet<TaskId>,
        required_milestone: TargetMilestone,
    ) -> Result<Self, PackageError> {
        if task_ids.is_empty() {
            return Err(PackageError::NoTasks);
        }
        Ok(Self {
            package_id,
            revision,
            policy,
            task_ids,
            required_milestone,
        })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PolicyError {
    #[error("invalid target ref {value:?}: {detail}")]
    InvalidTargetRef { value: String, detail: &'static str },
    #[error("scheduled-creation policy must not define a development target")]
    UnexpectedDevelopmentTarget,
    #[error("immediate-availability policy requires a development target")]
    MissingDevelopmentTarget,
    #[error("development and final targets must be different branches")]
    TargetsMustDiffer,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PackageError {
    #[error("package revision must contain at least one task")]
    NoTasks,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_target() -> TargetRef {
        TargetRef::new("refs/heads/main").unwrap()
    }

    #[test]
    fn immediate_mode_requires_a_distinct_development_target() {
        let repository_id = RepositoryId::new();
        let missing = RepositoryPolicy::new(
            repository_id,
            Revision::FIRST,
            PublicationMode::ImmediateAvailability,
            main_target(),
            None,
        );
        assert_eq!(missing, Err(PolicyError::MissingDevelopmentTarget));

        let same = RepositoryPolicy::new(
            repository_id,
            Revision::FIRST,
            PublicationMode::ImmediateAvailability,
            main_target(),
            Some(main_target()),
        );
        assert_eq!(same, Err(PolicyError::TargetsMustDiffer));
    }

    #[test]
    fn scheduled_mode_rejects_an_unused_development_target() {
        let result = RepositoryPolicy::new(
            RepositoryId::new(),
            Revision::FIRST,
            PublicationMode::ScheduledCreation,
            main_target(),
            Some(TargetRef::new("refs/heads/development").unwrap()),
        );
        assert_eq!(result, Err(PolicyError::UnexpectedDevelopmentTarget));
    }

    #[test]
    fn package_requires_at_least_one_task() {
        let policy = PolicyRef {
            repository_id: RepositoryId::new(),
            revision: Revision::FIRST,
        };
        let result = PackageRevision::new(
            PackageId::new(),
            Revision::FIRST,
            policy,
            BTreeSet::new(),
            TargetMilestone::TargetPublished,
        );
        assert_eq!(result, Err(PackageError::NoTasks));
    }

    #[test]
    fn target_ref_requires_a_full_branch_ref() {
        assert!(matches!(
            TargetRef::new("main"),
            Err(PolicyError::InvalidTargetRef { .. })
        ));
    }
}
