//! Versioned request and response contract shared by local clients and the daemon.

pub mod transport;

use std::{collections::BTreeSet, fmt};

pub use reccursive_core::{
    AcceptanceCheck, AttemptId, ConnectivityFault, EventId, FeatureId, FeaturePlan, Integration,
    PLAN_SCHEMA_VERSION, PackageId, PlanPhase, PlanTask, PublicationMode, ReasonCode,
    ReleaseUnitId, RepositoryId, RepositoryPolicy, RequestId, Revision, SchedulePolicy,
    SchedulePolicyOverride, StateReason, TargetMilestone, TargetRef, TaskId, TaskStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub use transport::{LocalClient, TransportError};

/// Local API protocol version. Version 16 adds package change inspection and check results.
pub const API_VERSION: u16 = 16;

/// Stable service identifier shared by the daemon and by service installation.
pub const SERVICE_NAME: &str = "reccursive-daemon";

/// Maximum encoded request or response size accepted by the local transport.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Domain version represented by this protocol crate.
#[must_use]
pub const fn domain_version() -> u16 {
    reccursive_core::DOMAIN_VERSION
}

/// A client-chosen name for one intended action, reused across retries of that action.
///
/// Deliberately opaque to the daemon: it is compared, never interpreted. Constrained only enough
/// that it can be a primary key and cannot be used to smuggle unbounded data into storage.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolValidationError> {
        let value = value.into();
        let usable = (1..=200).contains(&value.len())
            && value.trim() == value
            && !value.is_empty()
            && value
                .chars()
                .all(|character| character.is_ascii_graphic() || character == ' ');
        if !usable {
            return Err(ProtocolValidationError::InvalidIdempotencyKey);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
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
    /// Client-chosen key making a repeated request return its first result instead of acting
    /// twice.
    ///
    /// Distinct from `request_id`, which is generated per send and identifies *this*
    /// transmission for correlation. An idempotency key identifies the *intent*, and a client
    /// retrying after a dropped connection deliberately reuses it. Optional, because an
    /// interactive user issuing a command by hand has no retry to protect against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
    pub command: Command,
}

impl RequestEnvelope {
    #[must_use]
    pub fn new(auth_token: AuthToken, command: Command) -> Self {
        Self {
            api_version: API_VERSION,
            request_id: RequestId::new(),
            auth_token,
            idempotency_key: None,
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
            Command::InitializeRepository(request) => request.validate(),
            Command::CreateWorkspace(request) => request.validate(),
            Command::CapturePackage(request) => request.validate(),
            Command::CancelTask(request) => request.validate(),
            Command::SubmitTask(request) => request.validate(),
            Command::ReleasePackage(request) => request.validate(),
            Command::CreateReleaseUnit(request) => request.validate(),
            Command::PauseRepository { reason, .. }
                if reason.trim().is_empty() || reason.len() > 256 =>
            {
                Err(ProtocolValidationError::InvalidLifecycleMessage)
            }
            Command::PauseRepository { .. } => Ok(()),
            Command::ListReleaseAttempts { limit, .. } if !(1..=1_000).contains(limit) => {
                Err(ProtocolValidationError::InvalidAttemptLimit)
            }
            Command::ListDueUnits { concurrency_limit }
                if !(1..=1_000).contains(concurrency_limit) =>
            {
                Err(ProtocolValidationError::InvalidConcurrencyLimit)
            }
            Command::ListDueUnits { .. } => Ok(()),
            Command::ExportQueue { destination }
                if destination.is_empty() || destination.len() > 4_096 =>
            {
                Err(ProtocolValidationError::InvalidExportDestination)
            }
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
            | Command::SealPlan { .. }
            | Command::GetFeatureStatus { .. }
            | Command::SummarizeQueue { .. }
            | Command::ScheduleHistory { .. }
            | Command::GetWorkspace { .. }
            | Command::GetPackage { .. }
            | Command::InspectPackageChanges { .. }
            | Command::ListCheckResults { .. }
            | Command::GetReleaseUnit { .. }
            | Command::SetSchedulePolicy(_)
            | Command::GetSchedulePolicy { .. }
            | Command::ScheduleUnit(_)
            | Command::GetScheduleSlot { .. }
            | Command::WithdrawScheduleSlot { .. }
            | Command::RecalculateSchedule { .. }
            | Command::ReconcileMissedWindows { .. }
            | Command::SetScheduleOverride { .. }
            | Command::ResumeRepository { .. }
            | Command::ReleaseUnitNow { .. }
            | Command::PreviewSchedule { .. }
            | Command::ListIntegrationHealth
            | Command::DiagnoseRepository { .. }
            | Command::GetReleaseAttempt { .. }
            | Command::ListReleaseAttempts { .. }
            | Command::AuditQueue
            | Command::ExportQueue { .. }
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
    /// Register a repository and activate its first scheduling policy as one durable operation.
    InitializeRepository(InitializeRepositoryRequest),
    ListRepositories,
    ListEvents {
        limit: usize,
    },
    ImportPlan {
        plan: FeaturePlan,
    },
    /// Append a sealed copy of a feature's latest revision, fixing its scope.
    SealPlan {
        feature_id: FeatureId,
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
    CapturePackage(CapturePackageRequest),
    /// Cancel one not-yet-published task and block its dependent tasks.
    CancelTask(CancelTaskRequest),
    /// Capture, group, and schedule one task's completed work in a single step.
    SubmitTask(SubmitTaskRequest),
    /// Summarize queued release work, across every repository or one of them.
    SummarizeQueue {
        repository_id: Option<RepositoryId>,
    },
    /// List every release time ever selected for one unit, and why each stopped being valid.
    ScheduleHistory {
        release_unit_id: ReleaseUnitId,
    },
    /// Report every task of one plan revision with the durable work attached to it.
    GetFeatureStatus {
        feature_id: FeatureId,
        revision: Option<Revision>,
    },
    /// Reconcile, validate, and publish one immutable package through the daemon-owned worker.
    ReleasePackage(ReleasePackageRequest),
    /// Group tasks that cannot independently leave the target usable into one release unit.
    CreateReleaseUnit(CreateReleaseUnitRequest),
    /// Inspect one durable release unit.
    GetReleaseUnit {
        release_unit_id: ReleaseUnitId,
    },
    /// Activate a validated scheduling-policy revision for a repository.
    SetSchedulePolicy(SetSchedulePolicyRequest),
    /// Inspect a repository's active scheduling policy.
    GetSchedulePolicy {
        repository_id: RepositoryId,
    },
    /// Select a durable future release time for one captured release unit.
    ScheduleUnit(ScheduleUnitRequest),
    /// Inspect the durable slot previously selected for a release unit.
    GetScheduleSlot {
        release_unit_id: ReleaseUnitId,
    },
    /// Withdraw one unit's selected release time so it returns to the queue.
    WithdrawScheduleSlot {
        release_unit_id: ReleaseUnitId,
        reason: String,
    },
    /// Withdraw every live selection for a repository, for use after its policy changes.
    RecalculateSchedule {
        repository_id: RepositoryId,
        reason: String,
    },
    /// Apply the repository's missed-window policy to every release time that has already passed.
    ReconcileMissedWindows {
        repository_id: RepositoryId,
        seed: u64,
    },
    /// Refine one repository's scheduling without changing the policy it shares.
    SetScheduleOverride {
        repository_id: RepositoryId,
        #[serde(rename = "override")]
        override_policy: SchedulePolicyOverride,
    },
    /// List units due for release now, fairly across repositories and within a global limit.
    ListDueUnits {
        concurrency_limit: usize,
    },
    /// Stop a repository from handing out new release work until it is resumed.
    PauseRepository {
        repository_id: RepositoryId,
        reason: String,
    },
    /// Resume a paused repository.
    ResumeRepository {
        repository_id: RepositoryId,
    },
    /// Move one unit's release time to now, subject to the same eligibility rules.
    ReleaseUnitNow {
        release_unit_id: ReleaseUnitId,
    },
    /// Show a repository's upcoming release times without changing any of them.
    PreviewSchedule {
        repository_id: RepositoryId,
    },
    /// List integrations currently failing, and when each may be retried.
    ListIntegrationHealth,
    /// Check whether credentials and signing would let this repository publish right now.
    DiagnoseRepository {
        repository_id: RepositoryId,
    },
    /// Inspect one durable publication attempt.
    GetReleaseAttempt {
        attempt_id: AttemptId,
    },
    /// List recent publication attempts, optionally restricted to one package.
    ListReleaseAttempts {
        package_id: Option<PackageId>,
        limit: usize,
    },
    GetPackage {
        package_id: PackageId,
        revision: Revision,
    },
    /// Report what a captured package changes, relative to the base it was captured against.
    InspectPackageChanges {
        package_id: PackageId,
        revision: Revision,
        /// Include the patch text, not only which files changed.
        include_patch: bool,
    },
    /// Report the checks that ran for one package: at capture, and against each release candidate.
    ListCheckResults {
        package_id: PackageId,
        revision: Revision,
    },
    /// Inspect package storage and any crash-recovery evidence.
    AuditQueue,
    /// Write a portable, verified backup of queue state and immutable packages.
    ExportQueue {
        destination: String,
    },
    Status,
}

impl Command {
    /// Whether repeating this command could act twice.
    ///
    /// Only these commands are recorded against an idempotency key. A key on a query is accepted
    /// and ignored: repeating a question is already safe, and recording an answer would freeze it
    /// for every later use of that key. The match is exhaustive on purpose — a command added later
    /// must be classified deliberately rather than fall into a default.
    #[must_use]
    pub const fn changes_state(&self) -> bool {
        match self {
            Self::EnrollRepository(_)
            | Self::InitializeRepository(_)
            | Self::ImportPlan { .. }
            | Self::SealPlan { .. }
            | Self::SubmitTask(_)
            | Self::CreateWorkspace(_)
            | Self::CapturePackage(_)
            | Self::CancelTask(_)
            | Self::ReleasePackage(_)
            | Self::CreateReleaseUnit(_)
            | Self::SetSchedulePolicy(_)
            | Self::ScheduleUnit(_)
            | Self::WithdrawScheduleSlot { .. }
            | Self::RecalculateSchedule { .. }
            | Self::ReconcileMissedWindows { .. }
            | Self::SetScheduleOverride { .. }
            | Self::PauseRepository { .. }
            | Self::ResumeRepository { .. }
            | Self::ReleaseUnitNow { .. }
            | Self::ExportQueue { .. } => true,
            Self::Ping
            | Self::ListRepositories
            | Self::ListEvents { .. }
            | Self::GetPlan { .. }
            | Self::PlanHistory { .. }
            | Self::GetWorkspace { .. }
            | Self::GetFeatureStatus { .. }
            | Self::SummarizeQueue { .. }
            | Self::ScheduleHistory { .. }
            | Self::GetReleaseUnit { .. }
            | Self::GetSchedulePolicy { .. }
            | Self::GetScheduleSlot { .. }
            | Self::ListDueUnits { .. }
            | Self::PreviewSchedule { .. }
            | Self::ListIntegrationHealth
            | Self::DiagnoseRepository { .. }
            | Self::GetReleaseAttempt { .. }
            | Self::ListReleaseAttempts { .. }
            | Self::GetPackage { .. }
            | Self::InspectPackageChanges { .. }
            | Self::ListCheckResults { .. }
            | Self::AuditQueue
            | Self::Status => false,
        }
    }

    /// Whether carrying this command out can reach a Git remote.
    ///
    /// This decides what happens to an idempotency claim when the command fails. A failure that
    /// provably never left the machine can release its claim, so a genuine retry runs. A failure
    /// somewhere in a publication sequence cannot: the push may already have landed, and the
    /// service has no way to know from the error alone. Those keep the claim and replay the
    /// recorded failure, leaving the durable attempt record as the place to look.
    #[must_use]
    pub const fn may_reach_remote(&self) -> bool {
        match self {
            Self::ReleasePackage(_) => true,
            Self::Ping
            | Self::EnrollRepository(_)
            | Self::InitializeRepository(_)
            | Self::ListRepositories
            | Self::ListEvents { .. }
            | Self::ImportPlan { .. }
            | Self::SealPlan { .. }
            | Self::GetPlan { .. }
            | Self::PlanHistory { .. }
            | Self::GetFeatureStatus { .. }
            | Self::SummarizeQueue { .. }
            | Self::ScheduleHistory { .. }
            | Self::SubmitTask(_)
            | Self::CreateWorkspace(_)
            | Self::GetWorkspace { .. }
            | Self::CapturePackage(_)
            | Self::CancelTask(_)
            | Self::CreateReleaseUnit(_)
            | Self::GetReleaseUnit { .. }
            | Self::SetSchedulePolicy(_)
            | Self::GetSchedulePolicy { .. }
            | Self::ScheduleUnit(_)
            | Self::GetScheduleSlot { .. }
            | Self::WithdrawScheduleSlot { .. }
            | Self::RecalculateSchedule { .. }
            | Self::ReconcileMissedWindows { .. }
            | Self::SetScheduleOverride { .. }
            | Self::ListDueUnits { .. }
            | Self::PauseRepository { .. }
            | Self::ResumeRepository { .. }
            | Self::ReleaseUnitNow { .. }
            | Self::PreviewSchedule { .. }
            | Self::ListIntegrationHealth
            | Self::DiagnoseRepository { .. }
            | Self::GetReleaseAttempt { .. }
            | Self::ListReleaseAttempts { .. }
            | Self::GetPackage { .. }
            | Self::InspectPackageChanges { .. }
            | Self::ListCheckResults { .. }
            | Self::AuditQueue
            | Self::ExportQueue { .. }
            | Self::Status => false,
        }
    }

    /// A stable digest of exactly what this command asks for.
    ///
    /// Used to catch an idempotency key reused for a different request, which is a client bug
    /// worth reporting rather than silently serving the wrong cached answer. Every collection in a
    /// command payload is a `BTreeSet` or `BTreeMap`, so the serialization is ordered and the
    /// digest is reproducible across processes.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        let mut digest = Sha256::new();
        digest.update(&encoded);
        format!("{:x}", digest.finalize())
    }
}

/// Task selection for one immutable snapshot package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturePackageRequest {
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
}

/// A user-requested cancellation for one task in an immutable plan revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CancelTaskRequest {
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_id: TaskId,
    pub message: String,
}

/// Explicit commit metadata for a daemon-owned publication attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleasePackageRequest {
    pub package_id: PackageId,
    pub revision: Revision,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

/// Groups tasks that cannot independently leave the target usable into one release unit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreateReleaseUnitRequest {
    pub release_unit_id: ReleaseUnitId,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
}

impl CreateReleaseUnitRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.task_ids.is_empty() || self.task_ids.len() > 1_000 {
            return Err(ProtocolValidationError::InvalidReleaseUnitTasks);
        }
        Ok(())
    }
}

/// One task's completed work, submitted as a single intent.
///
/// Capture, grouping, and time selection are three durable steps, and an agent that has to issue
/// them separately can stop between any two of them — leaving work captured but never scheduled,
/// which looks identical to work still in progress. Submitting them together removes that state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubmitTaskRequest {
    pub feature_id: FeatureId,
    /// The plan revision to submit against. `None` means the newest stored revision, which is what
    /// an agent working from the plan it just imported wants.
    #[serde(default)]
    pub plan_revision: Option<Revision>,
    pub task_ids: BTreeSet<TaskId>,
    /// Seed for the deterministic slot selection, so a submission is reproducible.
    pub seed: u64,
}

impl SubmitTaskRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.task_ids.is_empty() || self.task_ids.len() > 1_000 {
            return Err(ProtocolValidationError::InvalidPackageTasks);
        }
        Ok(())
    }
}

/// Activates one validated scheduling-policy revision for a repository.
///
/// The policy itself is validated at deserialization by its own domain type; nothing here
/// re-checks the fields serde has already accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SetSchedulePolicyRequest {
    pub repository_id: RepositoryId,
    pub policy: SchedulePolicy,
}

/// Explicitly selects a durable future release time for one captured release unit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleUnitRequest {
    pub release_unit_id: ReleaseUnitId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub seed: u64,
}

impl ReleasePackageRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        for (field, value, maximum) in [
            ("release message", &self.message, 8_192),
            ("author name", &self.author_name, 512),
            ("author email", &self.author_email, 512),
        ] {
            if value.trim().is_empty() || value.len() > maximum || value.contains('\0') {
                return Err(ProtocolValidationError::InvalidReleaseField(field));
            }
        }
        Ok(())
    }
}

impl CancelTaskRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.message.trim().is_empty() || self.message.len() > 1_024 {
            return Err(ProtocolValidationError::InvalidLifecycleMessage);
        }
        Ok(())
    }
}

impl CapturePackageRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.task_ids.is_empty() || self.task_ids.len() > 1_000 {
            return Err(ProtocolValidationError::InvalidPackageTasks);
        }
        Ok(())
    }
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

/// Validated inputs needed to make a repository ready to accept scheduled work.
///
/// Enrollment and initial schedule activation are one request because a repository without an
/// active schedule policy is not a usable scheduled publisher. Keeping them together lets the
/// daemon persist both or neither across a crash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InitializeRepositoryRequest {
    pub enrollment: EnrollRepositoryRequest,
    pub schedule_policy: SchedulePolicy,
}

impl InitializeRepositoryRequest {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        self.enrollment.validate()
    }
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

/// Authenticated immutable package metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PackageView {
    pub package_id: PackageId,
    pub revision: Revision,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
    pub path: String,
    pub base_commit: String,
    pub base_tree: String,
    pub result_tree: String,
    pub content_hash: String,
    pub created_at_unix_ms: i64,
}

/// What a person should do about a blocked publication attempt.
///
/// Lives in the protocol crate rather than in one client because every client faces the same
/// question, and because the mapping is a property of the failure classifications the daemon
/// produces — not of any particular interface.
///
/// A classification with no known next action returns `None`. That is deliberate: inventing
/// plausible-sounding advice for a failure nobody has characterised is worse than admitting there
/// is none, because a command that does not help still costs the reader their attention.
#[must_use]
pub fn next_action_for_attempt(attempt: &ReleaseAttemptView) -> Option<String> {
    let package = attempt.package_id;
    let revision = attempt.package_revision.get();
    let repository = attempt.repository_id;
    let attempt_id = attempt.attempt_id;
    let classification = attempt.failure_classification.as_deref()?;
    Some(match classification {
        // The captured change no longer applies to the target. Seeing what it changes is the only
        // way to judge whether to re-capture it against the moved target.
        "conflict" => format!(
            "reccursive package changes {package} --revision {revision} --patch
             then re-capture the work against the moved target with a new plan revision"
        ),
        // A check ran and said no. Which one, and what it printed, is already recorded.
        "validation_failed" => format!(
            "reccursive package checks {package} --revision {revision}
             fix what the failing check reports, then submit the task again"
        ),
        // The check could not run at all, which is a problem with this machine rather than with
        // the change.
        "validation_unavailable" => "reccursive doctor
the attempt retries on its own once checks can run again"
            .to_owned(),
        // Credentials. Never retried automatically, because presenting a rejected credential
        // repeatedly is how an account gets locked.
        "authentication" => format!(
            "reccursive diagnose {repository}
             fix the credential it reports, then run: reccursive schedule release-now <unit>"
        ),
        // Transport faults back off and retry by themselves; the useful thing is to see when.
        "transport" | "remote" => "reccursive integrations
the attempt retries on its own when the remote is reachable"
            .to_owned(),
        // The push may or may not have landed. This is the one case that genuinely needs a person
        // to look at the remote, and saying so is more honest than suggesting a command.
        "ambiguous_remote" => format!(
            "reccursive release attempt {attempt_id}
             check the remote yourself before acting: the push may or may not have landed"
        ),
        // The target moved under a valid attempt. Reconciliation handles it on the next pass.
        "target_changed" => "reccursive queue status
the attempt reconciles against the new target on its own"
            .to_owned(),
        "internal_error" => format!(
            "reccursive logs --limit 50
then: reccursive release attempt {attempt_id}"
        ),
        _ => return None,
    })
}

/// One file a captured package changes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangedFileView {
    /// Git's own status letter: A added, M modified, D deleted, R renamed.
    pub change: String,
    pub path: String,
}

/// What a captured package changes, relative to the base it was captured against.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PackageChangesView {
    pub package_id: PackageId,
    pub revision: Revision,
    pub base_commit: String,
    pub base_tree: String,
    pub result_tree: String,
    pub files: Vec<ChangedFileView>,
    /// The patch itself, only when asked for. Absent by default because a release unit's diff can
    /// be large, and the question "what does this change" is usually answered by the file list.
    pub patch: Option<String>,
    /// True when the patch was cut short at the size limit. Reported rather than silently trimmed,
    /// so nobody reviews a partial diff believing it is whole.
    pub patch_truncated: bool,
}

/// One check that ran, and what it found.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckResultView {
    pub check_id: String,
    pub command: Vec<String>,
    /// `None` for a check that never returned, which `timed_out` then explains.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Output as stored: already scrubbed for credential-shaped text on the way in.
    pub output_summary: String,
    pub executed_at_unix_ms: i64,
    /// Set for a check that ran against a reconciled release candidate rather than at capture.
    pub attempt_id: Option<AttemptId>,
    /// The commit the candidate was built on, for a candidate check.
    pub base_commit: Option<String>,
    /// Why this result stopped standing — a changed base invalidates evidence rather than
    /// overwriting it, so a superseded result stays visible with its reason.
    pub invalidated_reason: Option<String>,
}

/// Every check recorded for one package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckResultsView {
    pub package_id: PackageId,
    pub revision: Revision,
    /// Checks run in the package's own workspace when it was captured.
    pub at_capture: Vec<CheckResultView>,
    /// Checks run against each reconciled release candidate, newest attempt first.
    pub at_release: Vec<CheckResultView>,
}

/// Where one release unit stands right now.
///
/// Derived when asked rather than stored, because every part of it already has an owner: the tasks
/// own their status, the slot owns its selected time, and the attempt owns publication. A second
/// stored copy would be a second thing to keep true.
///
/// Deliberately has no awaiting-merge variant. That state belongs to pull-request lifecycles, which
/// arrive in Phase 10; inventing it now would mean a value nothing can ever produce.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedUnitState {
    /// Captured and grouped, with no release time selected yet.
    Ready,
    /// A release time is selected and in the future.
    Scheduled,
    /// The release time has arrived and the daemon may act on it at any moment.
    Due,
    /// A publication attempt owns this unit now.
    Publishing,
    /// Something stopped it, and it will not move without a person.
    Blocked,
    /// Withdrawn from the target; it will not be published.
    Cancelled,
    /// On the target.
    Published,
}

/// One release unit as it appears in the queue.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueuedUnitView {
    pub release_unit_id: ReleaseUnitId,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub state: QueuedUnitState,
    /// Plan task names, so a queue reads as work rather than as identifiers.
    pub task_names: Vec<String>,
    pub selected_at_unix_ms: Option<i64>,
    /// Why the unit stopped, when it is blocked.
    pub reason: Option<String>,
    /// Why its most recent release time stopped being valid, if one was withdrawn.
    pub last_schedule_change: Option<String>,
}

/// Queued work across one repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryQueueView {
    pub repository_id: RepositoryId,
    pub target: TargetRef,
    /// Present when the repository is paused, carrying the reason it was.
    pub paused: Option<String>,
    pub units: Vec<QueuedUnitView>,
}

/// The whole queue, as of the moment it was asked for.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueueSummaryView {
    pub generated_at_unix_ms: i64,
    pub repositories: Vec<RepositoryQueueView>,
}

/// One release time that was selected for a unit, and what became of it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleChangeView {
    pub selected_at_unix_ms: i64,
    pub withdrawn_at_unix_ms: Option<i64>,
    /// Why it stopped being valid. Retained rather than deleted, so what moved and why stays
    /// answerable long after the fact.
    pub reason: Option<String>,
}

/// Everything durable that one plan revision has produced so far.
///
/// This is the view an agent polls. It reports each task's status verbatim rather than a derived
/// "ready" flag, because only the caller knows what it is waiting for — and because `blocked` and
/// `cancelled` have to be distinguishable from "not yet", which a boolean cannot do.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FeatureStatusView {
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub repository_id: RepositoryId,
    pub goal: String,
    pub sealed: bool,
    pub tasks: Vec<TaskProgressView>,
}

/// One task's durable state and the work attached to it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskProgressView {
    pub task_id: TaskId,
    pub name: String,
    pub status: TaskStatus,
    /// The status the task was blocked out of, when it is blocked. What it was doing matters:
    /// blocked before a push and blocked after one are different situations.
    pub blocked_from: Option<TaskStatus>,
    pub reason: Option<StateReason>,
    pub updated_at_unix_ms: i64,
    pub package_id: Option<PackageId>,
    pub release_unit_id: Option<ReleaseUnitId>,
    /// When this task's unit is due for release, if a time has been selected.
    pub selected_at_unix_ms: Option<i64>,
}

/// The three durable results one submission produced.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubmissionView {
    pub package: PackageView,
    pub unit: ReleaseUnitView,
    pub slot: ScheduleSlotView,
    /// False when the submission found the work already captured, grouped and scheduled, and
    /// returned what was there. A repeat of a submission is not a second submission.
    pub created: bool,
}

/// Durable result of cancelling one task and propagating its dependency consequences.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskCancellationView {
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_id: TaskId,
    pub blocked_dependents: Vec<TaskId>,
    pub invalidated_evidence: usize,
}

/// A durable release unit and the tasks it publishes together.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseUnitView {
    pub unit_id: ReleaseUnitId,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub task_ids: BTreeSet<TaskId>,
    pub created_at_unix_ms: i64,
}

/// One immutable revision of a repository's active scheduling policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SchedulePolicyView {
    pub repository_id: RepositoryId,
    pub revision: Revision,
    pub policy: SchedulePolicy,
    pub created_at_unix_ms: i64,
}

/// One integration that is currently failing, and what happens next.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntegrationHealthView {
    pub integration: Integration,
    pub scope: String,
    pub consecutive_failures: u32,
    pub fault: ConnectivityFault,
    pub detail: String,
    /// `None` means waiting will not help and a person has to act.
    pub next_attempt_at_unix_ms: Option<i64>,
}

/// One unit whose selected release time has arrived.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DueUnitView {
    pub release_unit_id: ReleaseUnitId,
    pub repository_id: RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub selected_at_unix_ms: i64,
}

/// What applying a missed-window policy did to release times that had already passed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MissedWindowView {
    /// Units left due for immediate release under an explicit catch-up allowance.
    pub released_now: Vec<ReleaseUnitId>,
    /// Units whose overdue time was replaced with a future one.
    pub rescheduled: Vec<ReleaseUnitId>,
    /// Units left alone because an attempt already owns them.
    pub retained: Vec<ReleaseUnitId>,
}

/// What an adaptive recalculation moved, and what it deliberately left alone.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRecalculationView {
    /// Units returned to the queue for a fresh selection.
    pub withdrawn: Vec<ReleaseUnitId>,
    /// Units left untouched because an attempt already owns them.
    pub retained: Vec<ReleaseUnitId>,
}

/// A selected, durable UTC release time for one release unit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScheduleSlotView {
    pub release_unit_id: ReleaseUnitId,
    pub repository_id: RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub policy_revision: Revision,
    pub timezone: String,
    pub eligible_at_unix_ms: i64,
    pub selected_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
}

/// Durable publication state returned to local clients without exposing credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttemptView {
    pub attempt_id: AttemptId,
    pub repository_id: RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub target_remote: String,
    pub target: TargetRef,
    pub base_commit: String,
    pub candidate_sha: Option<String>,
    pub candidate_parent_sha: Option<String>,
    pub push_intent_at_unix_ms: Option<i64>,
    pub observed_remote_sha: Option<String>,
    pub failure_classification: Option<String>,
    pub failed_attempts: u8,
    pub retry_not_before_unix_ms: Option<i64>,
    pub status: TaskStatus,
    pub reason: Option<StateReason>,
    pub blocked_from: Option<TaskStatus>,
    pub lease_expires_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

/// An unresolved package-storage issue retained for recovery and operator action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueueRecoveryIssueView {
    pub path: String,
    pub kind: String,
    pub message: String,
    pub first_seen_at_unix_ms: i64,
    pub last_seen_at_unix_ms: i64,
}

/// Current package-storage verification result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueueAuditView {
    pub verified_package_count: usize,
    pub issues: Vec<QueueRecoveryIssueView>,
}

/// Metadata for a portable queue backup written by the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueueExportView {
    pub destination: String,
    pub verified_package_count: usize,
    pub unresolved_issue_count: usize,
    pub database_sha256: String,
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
    RepositoryInitialized {
        repository: RepositoryView,
        schedule_policy: SchedulePolicyView,
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
    PlanSealed {
        plan: PlanView,
    },
    FeatureStatus {
        status: FeatureStatusView,
    },
    QueueSummary {
        summary: QueueSummaryView,
    },
    PackageChanges {
        changes: PackageChangesView,
    },
    CheckResults {
        results: CheckResultsView,
    },
    ScheduleHistory {
        changes: Vec<ScheduleChangeView>,
    },
    TaskSubmitted {
        submission: SubmissionView,
    },
    WorkspaceCreated {
        workspace: WorkspaceView,
    },
    Workspace {
        workspace: WorkspaceView,
    },
    PackageCaptured {
        package: PackageView,
    },
    Package {
        package: PackageView,
    },
    TaskCancelled {
        cancellation: TaskCancellationView,
    },
    ReleaseAttempt {
        attempt: ReleaseAttemptView,
    },
    ReleaseAttempts {
        attempts: Vec<ReleaseAttemptView>,
    },
    ReleaseUnitCreated {
        unit: ReleaseUnitView,
    },
    ReleaseUnit {
        unit: ReleaseUnitView,
    },
    SchedulePolicyActivated {
        policy: SchedulePolicyView,
    },
    SchedulePolicy {
        policy: SchedulePolicyView,
    },
    ScheduleSlot {
        slot: ScheduleSlotView,
    },
    ScheduleRecalculated {
        recalculation: ScheduleRecalculationView,
    },
    MissedWindowsReconciled {
        outcome: MissedWindowView,
    },
    ScheduleOverrideActivated {
        repository_id: RepositoryId,
        revision: Revision,
    },
    DueUnits {
        units: Vec<DueUnitView>,
    },
    RepositoryPaused {
        repository_id: RepositoryId,
        reason: String,
    },
    RepositoryResumed {
        repository_id: RepositoryId,
        was_paused: bool,
    },
    SchedulePreview {
        slots: Vec<ScheduleSlotView>,
        paused: Option<String>,
    },
    IntegrationHealth {
        integrations: Vec<IntegrationHealthView>,
    },
    RepositoryDiagnostics {
        repository_id: RepositoryId,
        credentials: String,
        credential_detail: Option<String>,
        signing: String,
        signing_detail: Option<String>,
        can_publish: bool,
    },
    QueueAudit {
        audit: QueueAuditView,
    },
    QueueExported {
        export: QueueExportView,
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
    #[error("snapshot package must select between 1 and 1000 plan tasks")]
    InvalidPackageTasks,
    #[error("cancellation message must contain 1-1024 non-whitespace bytes")]
    InvalidLifecycleMessage,
    #[error("{0} must contain non-whitespace text within its length limit and no NUL bytes")]
    InvalidReleaseField(&'static str),
    #[error("release attempt limit must be between 1 and 1000")]
    InvalidAttemptLimit,
    #[error("release concurrency limit must be between 1 and 1000")]
    InvalidConcurrencyLimit,
    #[error("idempotency key must be 1-200 printable characters without surrounding whitespace")]
    InvalidIdempotencyKey,
    #[error("release unit must select between 1 and 1000 plan tasks")]
    InvalidReleaseUnitTasks,
    #[error(
        "a required check name must be 1-256 non-whitespace bytes, and at most 64 may be named"
    )]
    InvalidRequiredCheck,
    #[error("queue export destination must contain 1-4096 bytes")]
    InvalidExportDestination,
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

    #[test]
    fn release_requests_and_attempt_limits_are_validated() {
        let token = AuthToken::new("a".repeat(32)).unwrap();
        let invalid = RequestEnvelope::new(
            token.clone(),
            Command::ReleasePackage(ReleasePackageRequest {
                package_id: PackageId::new(),
                revision: Revision::FIRST,
                message: " ".into(),
                author_name: "Aaryan".into(),
                author_email: "aaryan@example.invalid".into(),
            }),
        );
        assert_eq!(
            invalid.validate(),
            Err(ProtocolValidationError::InvalidReleaseField(
                "release message"
            ))
        );
        for limit in [0, 1_001] {
            let request = RequestEnvelope::new(
                token.clone(),
                Command::ListReleaseAttempts {
                    package_id: None,
                    limit,
                },
            );
            assert_eq!(
                request.validate(),
                Err(ProtocolValidationError::InvalidAttemptLimit)
            );
        }
    }

    #[test]
    fn an_idempotency_key_must_be_usable_as_an_identifier() {
        assert!(IdempotencyKey::new("agent-7/capture/task-2").is_ok());
        for rejected in [
            "",
            " ",
            " leading",
            "trailing ",
            "line\nbreak",
            &"x".repeat(201),
        ] {
            assert!(
                IdempotencyKey::new(rejected).is_err(),
                "{rejected:?} should not be accepted as a key"
            );
        }
    }

    #[test]
    fn the_same_request_fingerprints_the_same_way_and_a_different_one_does_not() {
        let plan_tasks = BTreeSet::from([TaskId::new(), TaskId::new()]);
        let feature_id = FeatureId::new();
        let request = CapturePackageRequest {
            feature_id,
            plan_revision: Revision::new(1).unwrap(),
            task_ids: plan_tasks.clone(),
        };
        let command = Command::CapturePackage(request.clone());
        assert_eq!(
            command.fingerprint(),
            Command::CapturePackage(request).fingerprint()
        );

        let different = Command::CapturePackage(CapturePackageRequest {
            feature_id,
            plan_revision: Revision::new(1).unwrap(),
            task_ids: BTreeSet::from([TaskId::new()]),
        });
        assert_ne!(command.fingerprint(), different.fingerprint());
    }

    #[test]
    fn queries_and_writes_are_classified_distinctly() {
        assert!(!Command::Ping.changes_state());
        assert!(!Command::Status.changes_state());
        assert!(!Command::AuditQueue.changes_state());
        assert!(
            Command::ReleaseUnitNow {
                release_unit_id: ReleaseUnitId::new()
            }
            .changes_state()
        );
        assert!(
            Command::ResumeRepository {
                repository_id: RepositoryId::new()
            }
            .changes_state()
        );
    }

    #[test]
    fn only_publication_is_treated_as_able_to_reach_a_remote() {
        assert!(!Command::Status.may_reach_remote());
        assert!(
            !Command::ReleaseUnitNow {
                release_unit_id: ReleaseUnitId::new()
            }
            .may_reach_remote()
        );
    }

    #[test]
    fn every_failure_the_release_worker_can_record_has_a_next_action() {
        // These are the classifications `ReleaseWorker` actually writes. A blocked attempt a user
        // cannot act on is the failure this task exists to remove, so the list is asserted rather
        // than assumed — a new classification added without guidance fails here.
        let attempt = |classification: &str| ReleaseAttemptView {
            attempt_id: AttemptId::new(),
            repository_id: RepositoryId::new(),
            package_id: PackageId::new(),
            package_revision: Revision::new(1).unwrap(),
            target_remote: "ssh://git@example.invalid/p.git".into(),
            target: TargetRef::new("refs/heads/main").unwrap(),
            base_commit: "a".repeat(40),
            candidate_sha: None,
            candidate_parent_sha: None,
            push_intent_at_unix_ms: None,
            observed_remote_sha: None,
            failure_classification: Some(classification.to_owned()),
            failed_attempts: 1,
            retry_not_before_unix_ms: None,
            status: TaskStatus::Blocked,
            reason: None,
            blocked_from: None,
            lease_expires_at_unix_ms: 0,
            created_at_unix_ms: 0,
            updated_at_unix_ms: 0,
        };
        for classification in [
            "conflict",
            "validation_failed",
            "validation_unavailable",
            "authentication",
            "transport",
            "remote",
            "ambiguous_remote",
            "target_changed",
            "internal_error",
        ] {
            let action = next_action_for_attempt(&attempt(classification));
            assert!(
                action.is_some(),
                "{classification} leaves a user with nothing to do"
            );
            assert!(
                action.as_deref().unwrap().contains("reccursive")
                    || action.as_deref().unwrap().contains("check the remote"),
                "{classification} suggests nothing actionable"
            );
        }
    }

    #[test]
    fn an_attempt_that_has_not_failed_is_offered_no_advice() {
        let mut attempt = ReleaseAttemptView {
            attempt_id: AttemptId::new(),
            repository_id: RepositoryId::new(),
            package_id: PackageId::new(),
            package_revision: Revision::new(1).unwrap(),
            target_remote: "ssh://git@example.invalid/p.git".into(),
            target: TargetRef::new("refs/heads/main").unwrap(),
            base_commit: "a".repeat(40),
            candidate_sha: None,
            candidate_parent_sha: None,
            push_intent_at_unix_ms: None,
            observed_remote_sha: None,
            failure_classification: None,
            failed_attempts: 0,
            retry_not_before_unix_ms: None,
            status: TaskStatus::Published,
            reason: None,
            blocked_from: None,
            lease_expires_at_unix_ms: 0,
            created_at_unix_ms: 0,
            updated_at_unix_ms: 0,
        };
        assert_eq!(next_action_for_attempt(&attempt), None);

        // An unfamiliar classification gets silence rather than invented advice.
        attempt.failure_classification = Some("something_nobody_has_characterised".into());
        assert_eq!(next_action_for_attempt(&attempt), None);
    }
}
