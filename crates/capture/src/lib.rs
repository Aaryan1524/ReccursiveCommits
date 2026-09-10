//! Isolated workspaces used to capture changes without mutating user checkouts.

mod snapshot;
mod validation;
mod workspace;

pub use snapshot::{SnapshotError, SnapshotManifest, SnapshotPackage, SnapshotRequest};
pub use validation::{ContentRule, ContentValidationPolicy, ContentViolation};
pub use workspace::{
    OwnedWorkspace, Prerequisite, PrerequisiteState, WorkspaceError, WorkspaceRequest,
};
