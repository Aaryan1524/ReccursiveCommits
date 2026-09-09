//! Isolated workspaces used to capture changes without mutating user checkouts.

mod workspace;

pub use workspace::{
    OwnedWorkspace, Prerequisite, PrerequisiteState, WorkspaceError, WorkspaceRequest,
};
