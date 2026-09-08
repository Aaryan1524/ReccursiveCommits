//! Domain model and state rules.

mod graph;
mod ids;
mod policy;
mod revision;
mod task;

pub use graph::{GraphError, TaskDefinition, TaskDefinitionError, TaskGraph};
pub use ids::{FeatureId, IdParseError, PackageId, ReleaseUnitId, RepositoryId, TaskId};
pub use policy::{
    PackageError, PackageRevision, PolicyError, PolicyRef, PublicationMode, RepositoryPolicy,
    TargetMilestone, TargetRef,
};
pub use revision::{Revision, RevisionError};
pub use task::{ReasonCode, StateReason, TaskState, TaskStatus, TransitionError};

/// Version of the in-process domain contract.
pub const DOMAIN_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_contract_starts_at_version_one() {
        assert_eq!(DOMAIN_VERSION, 1);
    }
}
