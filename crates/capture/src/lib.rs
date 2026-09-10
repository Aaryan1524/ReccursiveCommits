//! Isolated workspaces used to capture changes without mutating user checkouts.

mod snapshot;
mod workspace;

pub use snapshot::{SnapshotError, SnapshotManifest, SnapshotPackage, SnapshotRequest};
pub use workspace::{
    OwnedWorkspace, Prerequisite, PrerequisiteState, WorkspaceError, WorkspaceRequest,
};
