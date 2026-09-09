//! Versioned request and response contract shared by local clients and the daemon.

pub mod transport;

use std::fmt;

pub use reccursive_core::{
    AcceptanceCheck, AttemptId, EventId, FeatureId, FeaturePlan, PLAN_SCHEMA_VERSION, PlanPhase,
    PlanTask, PublicationMode, RepositoryId, RepositoryPolicy, RequestId, Revision, TargetRef,
    TaskId,
};
use serde::{Deserialize, Serialize};
pub use transport::{LocalClient, TransportError};

/// Initial local API protocol version.
pub const API_VERSION: u16 = 1;

/// Maximum encoded request or response size accepted by the local transport.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Domain version represented by this protocol crate.
#[must_use]
pub const fn domain_version() -> u16 {
    reccursive_core::DOMAIN_VERSION
}

/// Installation-scoped credential. Debug output is always redacted.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthToken(String);

impl AuthToken {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolValidationError> {
        let value = value.into();
        if value.len() < 32 || value.len() > 256 || value.chars().any(char::is_whitespace) {
            return Err(ProtocolValidationError::InvalidAuthToken);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for AuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthToken(<redacted>)")
    }
}

/// One versioned command sent to the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub api_version: u16,
    pub request_id: RequestId,
    pub auth_token: AuthToken,
    pub command: Command,
}

impl RequestEnvelope {
    #[must_use]
    pub fn new(auth_token: AuthToken, command: Command) -> Self {
        Self {
            api_version: API_VERSION,
            request_id: RequestId::new(),
            auth_token,
            command,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.api_version != API_VERSION {
            return Err(ProtocolValidationError::UnsupportedVersion {
                received: self.api_version,
                supported: API_VERSION,
            });
        }
        match &self.command {
            Command::EnrollRepository(request) => request.validate(),
            Command::CreateWorkspace(request) => request.validate(),
            Command::ImportPlan { plan } => plan
                .validate()
                .map_err(|error| ProtocolValidationError::InvalidPlan(error.to_string())),
            Command::ListEvents { limit } if !(1..=1_000).contains(limit) => {
                Err(ProtocolValidationError::InvalidEventLimit)
            }
            Command::Ping
            | Command::ListRepositories
            | Command::ListEvents { .. }
            | Command::GetPlan { .. }
            | Command::PlanHistory { .. }
            | Command::GetWorkspace { .. }
            | Command::Status => Ok(()),
        }
    }
}

/// Commands supported by the Phase 1 service boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Command {
    Ping,
    EnrollRepository(EnrollRepositoryRequest),
    ListRepositories,
    ListEvents {
        limit: usize,
    },
    ImportPlan {
        plan: FeaturePlan,
    },
    GetPlan {
        feature_id: FeatureId,
        revision: Option<Revision>,
    },
    PlanHistory {
        feature_id: FeatureId,
    },
    CreateWorkspace(CreateWorkspaceRequest),
    GetWorkspace {
        feature_id: FeatureId,
        revision: Revision,
    },
    Status,
}

/// Explicit request for a daemon-owned workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub feature_id: FeatureId,
    pub revision: Option<Revision>,
    #[serde(default)]
    pub prerequisites: Vec<String>,
}

impl CreateWorkspaceRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.prerequisites.len() > 1_000 {
            return Err(ProtocolValidationError::InvalidPrerequisite(
                "at most 1000 prerequisite paths may be selected".into(),
            ));
        }
        if self
            .prerequisites
            .iter()
            .any(|path| path.is_empty() || path.len() > 4_096)
        {
            return Err(ProtocolValidationError::InvalidPrerequisite(
                "prerequisite paths must contain 1-4096 bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Validated inputs needed to create a draft repository profile and first policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnrollRepositoryRequest {
    pub checkout_path: String,
    pub canonical_remote: String,
    pub publication_mode: PublicationMode,
    pub target: TargetRef,
    pub development_target: Option<TargetRef>,
}

impl EnrollRepositoryRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.checkout_path.trim().is_empty() {
            return Err(ProtocolValidationError::EmptyField("checkout_path"));
        }
        if self.canonical_remote.trim().is_empty() {
            return Err(ProtocolValidationError::EmptyField("canonical_remote"));
        }
        if remote_contains_credentials(&self.canonical_remote) {
            return Err(ProtocolValidationError::CredentialBearingRemote);
        }
        match self.publication_mode {
            PublicationMode::ScheduledCreation if self.development_target.is_some() => {
                Err(ProtocolValidationError::UnexpectedDevelopmentTarget)
            }
            PublicationMode::ImmediateAvailability if self.development_target.is_none() => {
                Err(ProtocolValidationError::MissingDevelopmentTarget)
            }
            _ if self.development_target.as_ref() == Some(&self.target) => {
                Err(ProtocolValidationError::TargetsMustDiffer)
            }
            _ => Ok(()),
        }
    }
}

fn remote_contains_credentials(remote: &str) -> bool {
    let Some((scheme, remainder)) = remote.split_once("://") else {
        return false;
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    let Some((userinfo, _host)) = authority.rsplit_once('@') else {
        return false;
    };
    matches!(scheme, "http" | "https") || userinfo.contains(':')
}

/// Repository representation safe for local API clients.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryView {
    pub id: RepositoryId,
    pub checkout_path: String,
    pub canonical_remote: String,
    pub managed_path: String,
    pub policy_revision: Revision,
    pub publication_mode: PublicationMode,
    pub target: TargetRef,
    pub development_target: Option<TargetRef>,
}

/// Severity attached to a diagnostic event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverityView {
    Debug,
    Info,
    Warning,
    Error,
}

/// Sanitized diagnostic event returned by the local service.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventView {
    pub sequence: i64,
    pub id: EventId,
    pub occurred_at_unix_ms: i64,
    pub request_id: Option<RequestId>,
    pub attempt_id: Option<AttemptId>,
    pub repository_id: Option<RepositoryId>,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    pub entity_revision: Option<Revision>,
    pub kind: String,
    pub severity: EventSeverityView,
    pub reason_code: Option<String>,
    pub message: String,
    pub details: serde_json::Value,
}

/// One immutable plan revision returned by the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanView {
    pub plan: FeaturePlan,
    pub created_at_unix_ms: i64,
}

/// Explicit uncommitted prerequisite included in an owned workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspacePrerequisiteView {
    pub path: String,
    pub state: String,
}

/// Daemon-owned workspace safe for local clients.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub feature_id: FeatureId,
    pub revision: Revision,
    pub repository_id: RepositoryId,
    pub path: String,
    pub base_commit: String,
    pub prerequisites: Vec<WorkspacePrerequisiteView>,
    pub created_at_unix_ms: i64,
}

/// Correlated daemon response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub api_version: u16,
    pub request_id: RequestId,
    pub result: Result<ResponseData, ApiError>,
}

impl ResponseEnvelope {
    #[must_use]
    pub const fn success(request_id: RequestId, data: ResponseData) -> Self {
        Self {
            api_version: API_VERSION,
            request_id,
            result: Ok(data),
        }
    }

    #[must_use]
    pub const fn failure(request_id: RequestId, error: ApiError) -> Self {
        Self {
            api_version: API_VERSION,
            request_id,
            result: Err(error),
        }
    }
}

/// Successful API payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ResponseData {
    Pong {
        service_version: String,
        schema_version: u32,
    },
    RepositoryEnrolled {
        repository: RepositoryView,
    },
    Repositories {
        repositories: Vec<RepositoryView>,
    },
    Events {
        events: Vec<EventView>,
    },
    PlanImported {
        plan: PlanView,
    },
    Plan {
        plan: PlanView,
    },
    PlanHistory {
        plans: Vec<PlanView>,
    },
    WorkspaceCreated {
        workspace: WorkspaceView,
    },
    Workspace {
        workspace: WorkspaceView,
    },
    Status {
        service_version: String,
        schema_version: u32,
        repository_count: usize,
    },
}

/// Stable machine-readable API failure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    #[must_use]
    pub fn new(code: ApiErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

/// Error codes remain stable when human wording improves.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidRequest,
    NotFound,
    UnsupportedVersion,
    Unauthorized,
    Conflict,
    TemporarilyUnavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProtocolValidationError {
    #[error("authentication token must be 32-256 non-whitespace characters")]
    InvalidAuthToken,
    #[error("API version {received} is unsupported; this service supports version {supported}")]
    UnsupportedVersion { received: u16, supported: u16 },
    #[error("{0} must not be empty")]
    EmptyField(&'static str),
    #[error("remote URL must not contain embedded credentials; use a credential helper or SSH")]
    CredentialBearingRemote,
    #[error("scheduled-creation mode must not define a development target")]
    UnexpectedDevelopmentTarget,
    #[error("immediate-availability mode requires a development target")]
    MissingDevelopmentTarget,
    #[error("development and target branches must differ")]
    TargetsMustDiffer,
    #[error("event limit must be between 1 and 1000")]
    InvalidEventLimit,
    #[error("invalid feature plan: {0}")]
    InvalidPlan(String),
    #[error("invalid prerequisite selection: {0}")]
    InvalidPrerequisite(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_links_the_current_domain_contract() {
        assert_eq!(domain_version(), reccursive_core::DOMAIN_VERSION);
    }

    #[test]
    fn authentication_token_is_redacted_from_debug_output() {
        let token = AuthToken::new("a".repeat(32)).unwrap();
        assert_eq!(format!("{token:?}"), "AuthToken(<redacted>)");
    }

    #[test]
    fn request_validation_reports_version_negotiation() {
        let mut request =
            RequestEnvelope::new(AuthToken::new("a".repeat(32)).unwrap(), Command::Ping);
        request.api_version += 1;
        assert_eq!(
            request.validate(),
            Err(ProtocolValidationError::UnsupportedVersion {
                received: API_VERSION + 1,
                supported: API_VERSION,
            })
        );
    }

    #[test]
    fn enrollment_rejects_credentials_and_invalid_mode_targets() {
        let target = TargetRef::new("refs/heads/main").unwrap();
        let credentials = EnrollRepositoryRequest {
            checkout_path: "/tmp/repo".into(),
            canonical_remote: "https://user:secret@example.invalid/repo.git".into(),
            publication_mode: PublicationMode::ScheduledCreation,
            target: target.clone(),
            development_target: None,
        };
        assert_eq!(
            credentials.validate(),
            Err(ProtocolValidationError::CredentialBearingRemote)
        );

        let missing_development = EnrollRepositoryRequest {
            checkout_path: "/tmp/repo".into(),
            canonical_remote: "git@example.invalid:repo.git".into(),
            publication_mode: PublicationMode::ImmediateAvailability,
            target,
            development_target: None,
        };
        assert_eq!(
            missing_development.validate(),
            Err(ProtocolValidationError::MissingDevelopmentTarget)
        );
    }

    #[test]
    fn event_query_limits_are_bounded() {
        let token = AuthToken::new("a".repeat(32)).unwrap();
        for limit in [0, 1_001] {
            let request = RequestEnvelope::new(token.clone(), Command::ListEvents { limit });
            assert_eq!(
                request.validate(),
                Err(ProtocolValidationError::InvalidEventLimit)
            );
        }
        assert!(
            RequestEnvelope::new(token, Command::ListEvents { limit: 100 })
                .validate()
                .is_ok()
        );
    }
}
