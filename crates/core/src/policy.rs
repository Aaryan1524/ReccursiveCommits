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

/// How finished work reaches the target branch.
///
/// Orthogonal to `PublicationMode`, which decides *when* and *where first*. This decides what
/// happens at the moment the target is supposed to receive the work: a push, or a pull request
/// somebody reviews and merges themselves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetIntegration {
    /// Push straight to the target, using the Git credentials already in use.
    #[default]
    DirectPush,
    /// Push to a branch and open a pull request against the target.
    ///
    /// The pull request is opened automatically; it is never merged automatically. Merging is
    /// authority over what lands on a main branch, and this product does not take it.
    PullRequest,
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
    /// How the target is reached. Defaults to a direct push, which needs nothing configured.
    pub target_integration: TargetIntegration,
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
            target_integration: TargetIntegration::DirectPush,
        })
    }

    /// Chooses how the target is reached.
    ///
    /// A builder rather than a sixth argument to `new`, because direct push is what almost every
    /// repository wants and a caller that does not care should not have to say so.
    /// A pull request needs a branch to come from, and immediate mode is what produces one.
    /// Rather than inventing a second branch scheme that would need its own lifecycle, the
    /// pull-request strategy reuses the development branch the repository already publishes to.
    pub fn with_target_integration(
        mut self,
        target_integration: TargetIntegration,
    ) -> Result<Self, PolicyError> {
        if target_integration == TargetIntegration::PullRequest
            && self.publication_mode != PublicationMode::ImmediateAvailability
        {
            return Err(PolicyError::PullRequestNeedsDevelopmentBranch);
        }
        self.target_integration = target_integration;
        Ok(self)
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
    #[error(
        "the pull-request strategy needs a development branch to open the request from; \
         enrol with --mode immediate --development-target <branch>"
    )]
    PullRequestNeedsDevelopmentBranch,
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
    fn a_pull_request_strategy_requires_a_branch_to_open_the_request_from() {
        let target = TargetRef::new("refs/heads/main").unwrap();
        let scheduled = RepositoryPolicy::new(
            RepositoryId::new(),
            Revision::FIRST,
            PublicationMode::ScheduledCreation,
            target.clone(),
            None,
        )
        .unwrap();
        // A pull request has to come from somewhere. Scheduled mode publishes straight to the
        // target and never creates a branch, so there would be nothing to open a request from.
        assert_eq!(
            scheduled
                .clone()
                .with_target_integration(TargetIntegration::PullRequest)
                .unwrap_err(),
            PolicyError::PullRequestNeedsDevelopmentBranch
        );
        // Direct push stays available to it, which is the default anyway.
        assert!(
            scheduled
                .with_target_integration(TargetIntegration::DirectPush)
                .is_ok()
        );

        let immediate = RepositoryPolicy::new(
            RepositoryId::new(),
            Revision::FIRST,
            PublicationMode::ImmediateAvailability,
            target,
            Some(TargetRef::new("refs/heads/development").unwrap()),
        )
        .unwrap();
        assert_eq!(
            immediate
                .with_target_integration(TargetIntegration::PullRequest)
                .unwrap()
                .target_integration,
            TargetIntegration::PullRequest
        );
    }

    #[test]
    fn a_policy_publishes_by_direct_push_unless_it_says_otherwise() {
        // The default matters: it is what every repository enrolled before this existed means,
        // and it is the strategy that needs no token and no GitHub.
        let policy = RepositoryPolicy::new(
            RepositoryId::new(),
            Revision::FIRST,
            PublicationMode::ScheduledCreation,
            TargetRef::new("refs/heads/main").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(policy.target_integration, TargetIntegration::DirectPush);
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
