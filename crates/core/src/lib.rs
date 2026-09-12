//! Domain model and state rules.

mod graph;
mod ids;
mod plan;
mod policy;
mod release;
mod revision;
mod schedule;
mod task;

pub use graph::{GraphError, TaskDefinition, TaskDefinitionError, TaskGraph};
pub use ids::{
    AttemptId, EventId, FeatureId, IdParseError, PackageId, ReleaseUnitId, RepositoryId, RequestId,
    TaskId,
};
pub use plan::{AcceptanceCheck, FeaturePlan, PLAN_SCHEMA_VERSION, PlanError, PlanPhase, PlanTask};
pub use policy::{
    PackageError, PackageRevision, PolicyError, PolicyRef, PublicationMode, RepositoryPolicy,
    TargetMilestone, TargetRef,
};
pub use release::{ReleaseUnit, ReleaseUnitError};
pub use revision::{Revision, RevisionError};
pub use schedule::{
    DailyReleaseRange, DailyTime, DailyWindow, IanaTimeZone, MissedWindowBehavior, SchedulePolicy,
    SchedulePolicyError, SchedulePolicyOverride, Weekday,
};
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
