use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use reccursive_capture::{
    ContentValidationPolicy, OwnedWorkspace, PrerequisiteState, SnapshotError, SnapshotPackage,
    SnapshotRequest, WorkspaceError, WorkspaceRequest as CaptureWorkspaceRequest,
};
use reccursive_protocol::{
    ApiError, ApiErrorCode, AuthToken, CancelTaskRequest, CapturePackageRequest, Command,
    CreateWorkspaceRequest, EnrollRepositoryRequest, EventSeverityView, EventView, PackageView,
    PlanView, ProtocolValidationError, QueueAuditView, QueueExportView, QueueRecoveryIssueView,
    ReasonCode, ReleaseAttemptView, ReleasePackageRequest, RepositoryId, RepositoryPolicy,
    RepositoryView, RequestEnvelope, RequestId, ResponseData, ResponseEnvelope, Revision,
    StateReason, TaskCancellationView, TaskStatus, TransportError, WorkspacePrerequisiteView,
    WorkspaceView,
    transport::{read_message, write_message},
};
use reccursive_store::{
    DEFAULT_EVENT_RETENTION, EventContext, EventSeverity, NewEvent, RepositoryRegistration,
    SnapshotRecord, SnapshotRecoveryIssue, Store, StoreError, StoredEvent, StoredPlan,
    StoredRepository, TrustedCheck, ValidationEvidence, WorkspaceRecord,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use wait_timeout::ChildExt;

/// Files owned by one daemon installation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePaths {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub auth_token: PathBuf,
    pub database: PathBuf,
    pub workspaces: PathBuf,
    pub packages: PathBuf,
}

impl ServicePaths {
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            socket: state_dir.join("service.sock"),
            lock: state_dir.join("service.lock"),
            auth_token: state_dir.join("auth.token"),
            database: state_dir.join("state.sqlite"),
            workspaces: state_dir.join("workspaces"),
            packages: state_dir.join("packages"),
            state_dir,
        }
    }
}

/// Bound local API and exclusive owner of mutable service state.
pub struct LocalService {
    owner: ServiceOwner,
    store: Arc<Mutex<Store>>,
    managed_root: Arc<PathBuf>,
    workspace_root: Arc<PathBuf>,
    release_root: Arc<PathBuf>,
    package_root: Arc<PathBuf>,
}

impl LocalService {
    /// Acquires exclusive ownership before opening the database or socket.
    pub fn bind(paths: ServicePaths) -> Result<Self, ServiceError> {
        let owner = ServiceOwner::acquire(paths)?;
        let mut store = Store::open(&owner.paths.database)?;
        let state_root = fs::canonicalize(&owner.paths.state_dir)?;
        let managed_root = state_root.join("repositories");
        let workspace_root = state_root.join("workspaces");
        let release_root = state_root.join("releases");
        let package_root = state_root.join("packages");
        reconcile_snapshot_storage(&mut store, &package_root)?;
        reconcile_interrupted_releases(&mut store, &managed_root, &state_root.join("releases"))?;
        Ok(Self {
            owner,
            store: Arc::new(Mutex::new(store)),
            managed_root: Arc::new(managed_root),
            workspace_root: Arc::new(workspace_root),
            release_root: Arc::new(release_root),
            package_root: Arc::new(package_root),
        })
    }

    /// Serves forever, creating one worker thread per accepted local connection.
    pub fn serve_forever(self) -> Result<(), ServiceError> {
        for connection in self.owner.listener.incoming() {
            let stream = connection?;
            let auth_token = self.owner.auth_token.clone();
            let store = Arc::clone(&self.store);
            let managed_root = Arc::clone(&self.managed_root);
            let workspace_root = Arc::clone(&self.workspace_root);
            let release_root = Arc::clone(&self.release_root);
            let package_root = Arc::clone(&self.package_root);
            thread::spawn(move || {
                let _ = handle_connection(
                    stream,
                    &auth_token,
                    &store,
                    &managed_root,
                    &workspace_root,
                    &release_root,
                    &package_root,
                );
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn serve_connections(self, count: usize) -> Result<(), ServiceError> {
        let mut workers = Vec::with_capacity(count);
        for _ in 0..count {
            let (stream, _) = self.owner.listener.accept()?;
            let auth_token = self.owner.auth_token.clone();
            let store = Arc::clone(&self.store);
            let managed_root = Arc::clone(&self.managed_root);
            let workspace_root = Arc::clone(&self.workspace_root);
            let release_root = Arc::clone(&self.release_root);
            let package_root = Arc::clone(&self.package_root);
            workers.push(thread::spawn(move || {
                handle_connection(
                    stream,
                    &auth_token,
                    &store,
                    &managed_root,
                    &workspace_root,
                    &release_root,
                    &package_root,
                )
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| ServiceError::WorkerPanicked)??;
        }
        Ok(())
    }
}

struct ServiceOwner {
    paths: ServicePaths,
    _lock_file: File,
    listener: UnixListener,
    auth_token: AuthToken,
}

impl ServiceOwner {
    fn acquire(paths: ServicePaths) -> Result<Self, ServiceError> {
        prepare_state_directory(&paths.state_dir)?;
        ensure_regular_file_or_missing(&paths.lock)?;
        ensure_regular_file_or_missing(&paths.auth_token)?;
        ensure_regular_file_or_missing(&paths.database)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock)?;
        fs::set_permissions(&paths.lock, fs::Permissions::from_mode(0o600))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ServiceError::AlreadyRunning { path: paths.lock });
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(ServiceError::Io(error)),
        }

        let auth_token = load_or_create_auth_token(&paths.auth_token)?;
        remove_stale_socket(&paths.socket)?;
        let listener = UnixListener::bind(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            paths,
            _lock_file: lock_file,
            listener,
            auth_token,
        })
    }
}

impl Drop for ServiceOwner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.socket);
    }
}

fn prepare_state_directory(path: &Path) -> Result<(), ServiceError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(ServiceError::InsecureStateDirectory {
                path: path.to_path_buf(),
            });
        }
        if !metadata.is_dir() {
            return Err(ServiceError::InsecureStateDirectory {
                path: path.to_path_buf(),
            });
        }
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn ensure_regular_file_or_missing(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(ServiceError::InsecureStateFile {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceError::Io(error)),
    }
}

fn load_or_create_auth_token(path: &Path) -> Result<AuthToken, ServiceError> {
    let value = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            let value = Uuid::new_v4().simple().to_string();
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            value
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => fs::read_to_string(path)?,
        Err(error) => return Err(ServiceError::Io(error)),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(AuthToken::new(value.trim().to_owned())?)
}

fn remove_stale_socket(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(ServiceError::SocketPathOccupied {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceError::Io(error)),
    }
}

fn handle_connection(
    mut stream: UnixStream,
    expected_auth_token: &AuthToken,
    store: &Mutex<Store>,
    managed_root: &Path,
    workspace_root: &Path,
    release_root: &Path,
    package_root: &Path,
) -> Result<(), ServiceError> {
    let request = match read_message::<RequestEnvelope>(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let response = ResponseEnvelope::failure(
                RequestId::new(),
                ApiError::new(ApiErrorCode::InvalidRequest, error.to_string(), false),
            );
            write_message(&mut stream, &response)?;
            return Ok(());
        }
    };

    let response = if let Err(error) = request.validate() {
        let code = if matches!(error, ProtocolValidationError::UnsupportedVersion { .. }) {
            ApiErrorCode::UnsupportedVersion
        } else {
            ApiErrorCode::InvalidRequest
        };
        ResponseEnvelope::failure(
            request.request_id,
            ApiError::new(code, error.to_string(), false),
        )
    } else if request.auth_token != *expected_auth_token {
        ResponseEnvelope::failure(
            request.request_id,
            ApiError::new(
                ApiErrorCode::Unauthorized,
                "local API authentication failed",
                false,
            ),
        )
    } else {
        let request_id = request.request_id;
        let command_name = command_name(&request.command);
        let result = dispatch(
            request.command,
            store,
            managed_root,
            workspace_root,
            release_root,
            package_root,
        );
        match record_api_event(store, request_id, command_name, &result) {
            Ok(()) => match result {
                Ok(data) => ResponseEnvelope::success(request_id, data),
                Err(error) => ResponseEnvelope::failure(request_id, error),
            },
            Err(error) => ResponseEnvelope::failure(request_id, error),
        }
    };
    write_message(&mut stream, &response)?;
    Ok(())
}

fn dispatch(
    command: Command,
    store: &Mutex<Store>,
    managed_root: &Path,
    workspace_root: &Path,
    release_root: &Path,
    package_root: &Path,
) -> Result<ResponseData, ApiError> {
    match command {
        Command::Ping => {
            let store = lock_store(store)?;
            Ok(ResponseData::Pong {
                service_version: env!("CARGO_PKG_VERSION").to_owned(),
                schema_version: store.schema_version().map_err(store_api_error)?,
            })
        }
        Command::Status => {
            let store = lock_store(store)?;
            Ok(ResponseData::Status {
                service_version: env!("CARGO_PKG_VERSION").to_owned(),
                schema_version: store.schema_version().map_err(store_api_error)?,
                repository_count: store.repositories().map_err(store_api_error)?.len(),
            })
        }
        Command::ListRepositories => {
            let store = lock_store(store)?;
            let repositories = store
                .repositories()
                .map_err(store_api_error)?
                .into_iter()
                .map(repository_view)
                .collect();
            Ok(ResponseData::Repositories { repositories })
        }
        Command::ListEvents { limit } => {
            let store = lock_store(store)?;
            let events = store
                .events(limit)
                .map_err(store_api_error)?
                .into_iter()
                .map(event_view)
                .collect();
            Ok(ResponseData::Events { events })
        }
        Command::ImportPlan { plan } => import_plan(plan, store),
        Command::GetPlan {
            feature_id,
            revision,
        } => {
            let store = lock_store(store)?;
            let plan = store
                .plan(feature_id, revision)
                .map_err(store_api_error)?
                .ok_or_else(|| {
                    ApiError::new(
                        ApiErrorCode::NotFound,
                        format!("feature plan {feature_id} was not found"),
                        false,
                    )
                })?;
            Ok(ResponseData::Plan {
                plan: plan_view(plan),
            })
        }
        Command::PlanHistory { feature_id } => {
            let store = lock_store(store)?;
            let plans = store
                .plan_history(feature_id)
                .map_err(store_api_error)?
                .into_iter()
                .map(plan_view)
                .collect();
            Ok(ResponseData::PlanHistory { plans })
        }
        Command::CreateWorkspace(request) => create_workspace(request, store, workspace_root),
        Command::GetWorkspace {
            feature_id,
            revision,
        } => get_workspace(feature_id, revision, store),
        Command::CapturePackage(request) => capture_package(request, store, package_root),
        Command::CancelTask(request) => cancel_task(request, store),
        Command::ReleasePackage(request) => {
            release_package(request, store, managed_root, release_root)
        }
        Command::GetReleaseAttempt { attempt_id } => get_release_attempt(attempt_id, store),
        Command::ListReleaseAttempts { package_id, limit } => {
            list_release_attempts(package_id, limit, store)
        }
        Command::GetPackage {
            package_id,
            revision,
        } => get_package(package_id, revision, store),
        Command::AuditQueue => audit_queue(store, package_root),
        Command::ExportQueue { destination } => export_queue(destination, store, package_root),
        Command::EnrollRepository(request) => enroll_repository(request, store, managed_root),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Ping => "ping",
        Command::EnrollRepository(_) => "repository.add",
        Command::ListRepositories => "repository.list",
        Command::ListEvents { .. } => "logs",
        Command::ImportPlan { .. } => "plan.import",
        Command::GetPlan { .. } => "plan.show",
        Command::PlanHistory { .. } => "plan.history",
        Command::CreateWorkspace(_) => "workspace.create",
        Command::GetWorkspace { .. } => "workspace.show",
        Command::CapturePackage(_) => "package.capture",
        Command::CancelTask(_) => "task.cancel",
        Command::ReleasePackage(_) => "release.publish",
        Command::GetReleaseAttempt { .. } => "release.attempt",
        Command::ListReleaseAttempts { .. } => "release.attempts",
        Command::GetPackage { .. } => "package.show",
        Command::AuditQueue => "queue.audit",
        Command::ExportQueue { .. } => "queue.export",
        Command::Status => "status",
    }
}

fn record_api_event(
    store: &Mutex<Store>,
    request_id: RequestId,
    command: &'static str,
    result: &Result<ResponseData, ApiError>,
) -> Result<(), ApiError> {
    let (repository_id, entity_type, entity_id, entity_revision) = match result {
        Ok(ResponseData::RepositoryEnrolled { repository }) => (
            Some(repository.id),
            Some("repository".into()),
            Some(repository.id.to_string()),
            None,
        ),
        Ok(ResponseData::PlanImported { plan }) | Ok(ResponseData::Plan { plan }) => (
            Some(plan.plan.repository_id),
            Some("feature_plan".into()),
            Some(plan.plan.feature_id.to_string()),
            Some(plan.plan.revision),
        ),
        Ok(ResponseData::WorkspaceCreated { workspace })
        | Ok(ResponseData::Workspace { workspace }) => (
            Some(workspace.repository_id),
            Some("workspace".into()),
            Some(workspace.feature_id.to_string()),
            Some(workspace.revision),
        ),
        Ok(ResponseData::PackageCaptured { package }) | Ok(ResponseData::Package { package }) => (
            None,
            Some("snapshot_package".into()),
            Some(package.package_id.to_string()),
            Some(package.revision),
        ),
        Ok(ResponseData::TaskCancelled { cancellation }) => (
            None,
            Some("plan_task".into()),
            Some(cancellation.task_id.to_string()),
            Some(cancellation.plan_revision),
        ),
        Ok(ResponseData::ReleaseAttempt { attempt }) => (
            Some(attempt.repository_id),
            Some("release_attempt".into()),
            Some(attempt.attempt_id.to_string()),
            Some(attempt.package_revision),
        ),
        Ok(ResponseData::QueueExported { .. }) | Ok(ResponseData::QueueAudit { .. }) => {
            (None, Some("queue_storage".into()), None, None)
        }
        _ => (None, None, None, None),
    };
    let (kind, severity, reason_code, outcome) = match result {
        Ok(_) => (
            "api.request_succeeded",
            EventSeverity::Info,
            None,
            "succeeded",
        ),
        Err(error) => (
            "api.request_failed",
            EventSeverity::Error,
            Some(api_error_code(error.code).to_owned()),
            "failed",
        ),
    };
    let event = NewEvent::new(
        current_unix_ms()?,
        EventContext {
            request_id: Some(request_id),
            repository_id,
            entity_type,
            entity_id,
            entity_revision,
            ..EventContext::default()
        },
        kind,
        severity,
        reason_code,
        format!("{command} request {outcome}"),
        json!({ "command": command, "outcome": outcome }),
    )
    .map_err(store_api_error)?;
    lock_store(store)?
        .record_event(&event, DEFAULT_EVENT_RETENTION)
        .map_err(store_api_error)
}

const fn api_error_code(code: ApiErrorCode) -> &'static str {
    match code {
        ApiErrorCode::InvalidRequest => "invalid_request",
        ApiErrorCode::NotFound => "not_found",
        ApiErrorCode::UnsupportedVersion => "unsupported_version",
        ApiErrorCode::Unauthorized => "unauthorized",
        ApiErrorCode::Conflict => "conflict",
        ApiErrorCode::TemporarilyUnavailable => "temporarily_unavailable",
        ApiErrorCode::Internal => "internal",
    }
}

fn import_plan(
    plan: reccursive_protocol::FeaturePlan,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let created_at_unix_ms = current_unix_ms()?;
    let mut store = lock_store(store)?;
    store
        .import_plan(&plan, created_at_unix_ms)
        .map_err(store_api_error)?;
    Ok(ResponseData::PlanImported {
        plan: PlanView {
            plan,
            created_at_unix_ms,
        },
    })
}

fn create_workspace(
    request: CreateWorkspaceRequest,
    store: &Mutex<Store>,
    workspace_root: &Path,
) -> Result<ResponseData, ApiError> {
    let (plan, repository) = {
        let store = lock_store(store)?;
        let plan = store
            .plan(request.feature_id, request.revision)
            .map_err(store_api_error)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::NotFound,
                    format!("feature plan {} was not found", request.feature_id),
                    false,
                )
            })?;
        let repository = store
            .repository(plan.plan.repository_id)
            .map_err(store_api_error)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::NotFound,
                    format!("repository {} was not found", plan.plan.repository_id),
                    false,
                )
            })?;
        (plan.plan, repository)
    };
    if !plan.sealed {
        return Err(ApiError::new(
            ApiErrorCode::InvalidRequest,
            "owned workspaces require a sealed plan revision",
            false,
        ));
    }

    let owned = OwnedWorkspace::create(CaptureWorkspaceRequest {
        feature_id: plan.feature_id,
        revision: plan.revision,
        source_checkout: Path::new(&repository.registration.checkout_path),
        workspace_root,
        base_ref: plan.target.as_str(),
        prerequisite_paths: &request.prerequisites,
    })
    .map_err(workspace_api_error)?;
    let created_at_unix_ms = current_unix_ms()?;
    let record = WorkspaceRecord {
        feature_id: plan.feature_id,
        revision: plan.revision,
        path: owned.path.clone(),
        base_commit: owned.base_commit.clone(),
        prerequisites: serde_json::to_value(&owned.prerequisites).map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "workspace prerequisite manifest could not be encoded",
                false,
            )
        })?,
        created_at_unix_ms,
    };
    lock_store(store)?
        .record_workspace(&record)
        .map_err(store_api_error)?;
    Ok(ResponseData::WorkspaceCreated {
        workspace: workspace_view(record, plan.repository_id)?,
    })
}

fn get_workspace(
    feature_id: reccursive_protocol::FeatureId,
    revision: Revision,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let store = lock_store(store)?;
    let plan = store
        .plan(feature_id, Some(revision))
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!(
                    "feature plan {feature_id} revision {} was not found",
                    revision.get()
                ),
                false,
            )
        })?;
    let record = store
        .workspace(feature_id, revision)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!(
                    "workspace for {feature_id} revision {} was not found",
                    revision.get()
                ),
                false,
            )
        })?;
    Ok(ResponseData::Workspace {
        workspace: workspace_view(record, plan.plan.repository_id)?,
    })
}

fn workspace_view(
    record: WorkspaceRecord,
    repository_id: RepositoryId,
) -> Result<WorkspaceView, ApiError> {
    let prerequisites: Vec<reccursive_capture::Prerequisite> =
        serde_json::from_value(record.prerequisites).map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "stored workspace prerequisite manifest is invalid",
                false,
            )
        })?;
    Ok(WorkspaceView {
        feature_id: record.feature_id,
        revision: record.revision,
        repository_id,
        path: record.path.to_string_lossy().into_owned(),
        base_commit: record.base_commit,
        prerequisites: prerequisites
            .into_iter()
            .map(|prerequisite| WorkspacePrerequisiteView {
                path: prerequisite.path,
                state: match prerequisite.state {
                    PrerequisiteState::Present => "present",
                    PrerequisiteState::Deleted => "deleted",
                }
                .into(),
            })
            .collect(),
        created_at_unix_ms: record.created_at_unix_ms,
    })
}

fn workspace_api_error(error: WorkspaceError) -> ApiError {
    let code = match &error {
        WorkspaceError::AlreadyExists { .. } => ApiErrorCode::Conflict,
        WorkspaceError::Git { .. } | WorkspaceError::Io(_) => ApiErrorCode::TemporarilyUnavailable,
        _ => ApiErrorCode::InvalidRequest,
    };
    ApiError::new(
        code,
        error.to_string(),
        code == ApiErrorCode::TemporarilyUnavailable,
    )
}

/// Reconciles the package directory with durable records after a crash or abrupt shutdown.
/// A complete authenticated package is recoverable even if the process stopped between its
/// atomic directory rename and SQLite insert. Incomplete or invalid artifacts are preserved and
/// recorded for the operator instead of being silently deleted.
fn reconcile_snapshot_storage(store: &mut Store, package_root: &Path) -> Result<(), StoreError> {
    ensure_private_directory(package_root)?;
    let package_root = fs::canonicalize(package_root)?;
    let now = recovery_unix_ms()?;
    let mut records: HashMap<_, _> = store
        .snapshots()?
        .into_iter()
        .map(|record| ((record.package_id, record.revision), record))
        .collect();
    let mut observed = HashSet::new();

    for parent in fs::read_dir(&package_root)? {
        let parent = parent?;
        let parent_path = parent.path();
        let parent_type = parent.file_type()?;
        if !parent_type.is_dir() || parent_type.is_symlink() {
            record_recovery_issue(
                store,
                parent_path,
                "unsafe_package_parent",
                "package root contains a non-directory or symbolic-link entry",
                now,
            )?;
            continue;
        }
        let package_id = match parent.file_name().to_string_lossy().parse() {
            Ok(value) => value,
            Err(_) => {
                record_recovery_issue(
                    store,
                    parent_path,
                    "unrecognized_package_parent",
                    "package parent name is not a valid package identifier",
                    now,
                )?;
                continue;
            }
        };

        for entry in fs::read_dir(&parent_path)? {
            let entry = entry?;
            let path = entry.path();
            let entry_type = entry.file_type()?;
            if !entry_type.is_dir() || entry_type.is_symlink() {
                record_recovery_issue(
                    store,
                    path,
                    "unsafe_package_entry",
                    "package entry is not a real directory",
                    now,
                )?;
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(revision) = parse_package_revision(&name, "revision-") {
                inspect_complete_package(
                    store,
                    &mut records,
                    &mut observed,
                    package_id,
                    revision,
                    path,
                    now,
                )?;
            } else if let Some(revision) =
                parse_package_revision(&name, ".revision-").filter(|_| name.ends_with(".partial"))
            {
                let final_name = format!("revision-{}", revision.get());
                let final_path = parent_path.join(final_name);
                match SnapshotPackage::open(path.clone()) {
                    Ok(package)
                        if package.manifest.package_id == package_id
                            && package.manifest.revision == revision
                            && fs::symlink_metadata(&final_path).is_err() =>
                    {
                        fs::rename(&path, &final_path)?;
                        sync_directory(&parent_path)?;
                        inspect_complete_package(
                            store,
                            &mut records,
                            &mut observed,
                            package_id,
                            revision,
                            final_path,
                            now,
                        )?;
                    }
                    Ok(_) => record_recovery_issue(
                        store,
                        path,
                        "incomplete_capture",
                        "partial package conflicts with its final destination or identity",
                        now,
                    )?,
                    Err(error) => record_recovery_issue(
                        store,
                        path,
                        "incomplete_capture",
                        &format!("partial package is not yet recoverable: {error}"),
                        now,
                    )?,
                }
            } else {
                record_recovery_issue(
                    store,
                    path,
                    "unrecognized_package_entry",
                    "package directory name is not a supported revision layout",
                    now,
                )?;
            }
        }
    }

    for record in records.into_values() {
        if !observed.contains(&record.path) {
            record_recovery_issue(
                store,
                record.path,
                "missing_package",
                "database record has no matching package directory in managed storage",
                now,
            )?;
        }
    }

    // Every package that was going to be re-registered now has been, so a parent link that is
    // still dangling is a real break in the unit chain rather than an ordering artefact.
    for (child, parent) in store.dangling_package_parents()? {
        let path = store
            .snapshot(child, Revision::FIRST)?
            .map_or_else(|| PathBuf::from(child.to_string()), |record| record.path);
        record_recovery_issue(
            store,
            path,
            "broken_unit_chain",
            &format!("package {child} was built on {parent}, which is not stored"),
            now,
        )?;
    }
    Ok(())
}

/// Resolves any push that may have reached the remote before the last shutdown.
///
/// Invariant 6: this runs before the service accepts work, so a successful-but-unrecorded push is
/// discovered rather than repeated as a second commit. It is deliberately best-effort — a daemon
/// starting without network access must still come up, and the attempt stays listed for the next
/// attempt at resolution rather than being lost.
fn reconcile_interrupted_releases(
    store: &mut Store,
    managed_root: &Path,
    release_root: &Path,
) -> Result<(), StoreError> {
    if store
        .release_attempts_awaiting_remote_resolution()?
        .is_empty()
    {
        return Ok(());
    }
    let worker = crate::release::ReleaseWorker::new(
        managed_root,
        release_root,
        crate::SERVICE_NAME.to_owned(),
    );
    let now = recovery_unix_ms()?;
    let report = crate::release::recover_interrupted_releases(store, &worker, now)?;
    for failure in report.failures {
        record_recovery_issue(
            store,
            PathBuf::from(failure.attempt_id.to_string()),
            "unresolved_publication",
            &failure.detail,
            now,
        )?;
    }
    Ok(())
}

fn inspect_complete_package(
    store: &mut Store,
    records: &mut HashMap<(reccursive_protocol::PackageId, Revision), SnapshotRecord>,
    observed: &mut HashSet<PathBuf>,
    expected_package_id: reccursive_protocol::PackageId,
    expected_revision: Revision,
    path: PathBuf,
    now: i64,
) -> Result<(), StoreError> {
    let package = match SnapshotPackage::open(path.clone()) {
        Ok(package) => package,
        Err(error) => {
            records.remove(&(expected_package_id, expected_revision));
            observed.insert(path.clone());
            return record_recovery_issue(
                store,
                path,
                "invalid_package",
                &format!("package verification failed: {error}"),
                now,
            );
        }
    };
    if package.manifest.package_id != expected_package_id
        || package.manifest.revision != expected_revision
    {
        records.remove(&(expected_package_id, expected_revision));
        observed.insert(path.clone());
        return record_recovery_issue(
            store,
            path,
            "package_identity_mismatch",
            "package manifest does not match its managed directory",
            now,
        );
    }
    observed.insert(path.clone());
    let record = records.remove(&(expected_package_id, expected_revision));
    let recovered = SnapshotRecord {
        package_id: package.manifest.package_id,
        revision: package.manifest.revision,
        feature_id: package.manifest.feature_id,
        plan_revision: package.manifest.plan_revision,
        path: path.clone(),
        base_tree: package.manifest.base_tree.clone(),
        result_tree: package.manifest.result_tree.clone(),
        content_hash: package.manifest.content_hash.clone(),
        parent_package_id: package.manifest.parent_package_id,
        manifest: serde_json::to_value(&package.manifest)?,
        created_at_unix_ms: now,
    };
    match record {
        Some(record) if snapshot_records_match(&record, &recovered) => {
            store.clear_snapshot_recovery_issue(&path)?;
        }
        Some(_) => record_recovery_issue(
            store,
            path,
            "record_mismatch",
            "database record disagrees with the authenticated package manifest",
            now,
        )?,
        None => {
            store.record_snapshot(&recovered)?;
            store.clear_snapshot_recovery_issue(&path)?;
        }
    }
    Ok(())
}

fn snapshot_records_match(left: &SnapshotRecord, right: &SnapshotRecord) -> bool {
    left.package_id == right.package_id
        && left.revision == right.revision
        && left.feature_id == right.feature_id
        && left.plan_revision == right.plan_revision
        && left.path == right.path
        && left.base_tree == right.base_tree
        && left.result_tree == right.result_tree
        && left.content_hash == right.content_hash
}

fn parse_package_revision(name: &str, prefix: &str) -> Option<Revision> {
    let value = name.strip_prefix(prefix)?;
    let value = value.strip_suffix(".partial").unwrap_or(value);
    value
        .parse::<u32>()
        .ok()
        .and_then(|value| Revision::new(value).ok())
}

fn ensure_private_directory(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(StoreError::InvalidData(format!(
            "managed package root is unsafe: {}",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(StoreError::Io(error)),
    }
}

fn record_recovery_issue(
    store: &mut Store,
    path: PathBuf,
    kind: &str,
    message: &str,
    now: i64,
) -> Result<(), StoreError> {
    store.record_snapshot_recovery_issue(&SnapshotRecoveryIssue {
        path,
        kind: kind.into(),
        message: message.into(),
        first_seen_at_unix_ms: now,
        last_seen_at_unix_ms: now,
    })
}

fn recovery_unix_ms() -> Result<i64, StoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::InvalidData("system clock is before the Unix epoch".into()))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| StoreError::InvalidData("system clock is out of range".into()))
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn audit_queue(store: &Mutex<Store>, package_root: &Path) -> Result<ResponseData, ApiError> {
    let mut store = lock_store(store)?;
    reconcile_snapshot_storage(&mut store, package_root).map_err(store_api_error)?;
    let records = store.snapshots().map_err(store_api_error)?;
    let verified_package_count = records
        .iter()
        .filter(|record| verified_snapshot_record(record, package_root).is_ok())
        .count();
    let issues = store
        .snapshot_recovery_issues()
        .map_err(store_api_error)?
        .into_iter()
        .map(|issue| QueueRecoveryIssueView {
            path: issue.path.to_string_lossy().into_owned(),
            kind: issue.kind,
            message: issue.message,
            first_seen_at_unix_ms: issue.first_seen_at_unix_ms,
            last_seen_at_unix_ms: issue.last_seen_at_unix_ms,
        })
        .collect();
    Ok(ResponseData::QueueAudit {
        audit: QueueAuditView {
            verified_package_count,
            issues,
        },
    })
}

fn export_queue(
    destination: String,
    store: &Mutex<Store>,
    package_root: &Path,
) -> Result<ResponseData, ApiError> {
    let destination = PathBuf::from(destination);
    if !destination.is_absolute() {
        return Err(ApiError::new(
            ApiErrorCode::InvalidRequest,
            "queue export destination must be an absolute directory",
            false,
        ));
    }
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(ApiError::new(
            ApiErrorCode::Conflict,
            "queue export destination already exists",
            false,
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        ApiError::new(
            ApiErrorCode::InvalidRequest,
            "queue export destination must have a parent directory",
            false,
        )
    })?;
    fs::create_dir_all(parent).map_err(|_| {
        ApiError::new(
            ApiErrorCode::TemporarilyUnavailable,
            "queue export parent directory cannot be created",
            true,
        )
    })?;
    let temporary = parent.join(format!(".reccursive-export-{}.partial", Uuid::new_v4()));
    fs::create_dir(&temporary).map_err(|_| {
        ApiError::new(
            ApiErrorCode::TemporarilyUnavailable,
            "queue export temporary directory cannot be created",
            true,
        )
    })?;

    let export = (|| {
        let (records, issues) = {
            let mut store = lock_store(store)?;
            reconcile_snapshot_storage(&mut store, package_root).map_err(store_api_error)?;
            store
                .create_backup(temporary.join("state.sqlite"))
                .map_err(store_api_error)?;
            (
                store.snapshots().map_err(store_api_error)?,
                store.snapshot_recovery_issues().map_err(store_api_error)?,
            )
        };
        let verified_records: Vec<_> = records
            .iter()
            .filter_map(|record| {
                verified_snapshot_record(record, package_root)
                    .ok()
                    .map(|package| (record, package))
            })
            .collect();
        let mut exported = Vec::new();
        for (record, package) in &verified_records {
            let package_parent = temporary
                .join("packages")
                .join(record.package_id.to_string());
            fs::create_dir_all(&package_parent).map_err(|_| {
                ApiError::new(
                    ApiErrorCode::TemporarilyUnavailable,
                    "queue export package directory cannot be created",
                    true,
                )
            })?;
            let target = package_parent.join(format!("revision-{}", record.revision.get()));
            package
                .copy_verified_to(&target)
                .map_err(snapshot_api_error)?;
            exported.push(json!({
                "package_id": record.package_id,
                "revision": record.revision,
                "content_hash": record.content_hash,
                "path": format!("packages/{}/revision-{}", record.package_id, record.revision.get()),
            }));
        }
        let database_sha256 =
            sha256_file(&temporary.join("state.sqlite")).map_err(snapshot_api_error)?;
        let manifest = json!({
            "schema_version": 1,
            "database": "state.sqlite",
            "database_sha256": database_sha256,
            "packages": exported,
            "unresolved_issue_count": issues.len(),
        });
        let mut manifest_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary.join("queue-export.json"))
            .map_err(|_| {
                ApiError::new(
                    ApiErrorCode::TemporarilyUnavailable,
                    "queue export manifest cannot be written",
                    true,
                )
            })?;
        serde_json::to_writer_pretty(&mut manifest_file, &manifest).map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "queue export manifest cannot be encoded",
                false,
            )
        })?;
        manifest_file.write_all(b"\n").map_err(|_| {
            ApiError::new(
                ApiErrorCode::TemporarilyUnavailable,
                "queue export manifest cannot be finalized",
                true,
            )
        })?;
        manifest_file.sync_all().map_err(|_| {
            ApiError::new(
                ApiErrorCode::TemporarilyUnavailable,
                "queue export manifest cannot be synchronized",
                true,
            )
        })?;
        sync_directory(&temporary).map_err(store_api_error)?;
        fs::rename(&temporary, &destination).map_err(|_| {
            ApiError::new(
                ApiErrorCode::TemporarilyUnavailable,
                "queue export cannot be finalized",
                true,
            )
        })?;
        sync_directory(parent).map_err(store_api_error)?;
        Ok(ResponseData::QueueExported {
            export: QueueExportView {
                destination: destination.to_string_lossy().into_owned(),
                verified_package_count: verified_records.len(),
                unresolved_issue_count: issues.len(),
                database_sha256,
            },
        })
    })();
    if export.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    export
}

fn verified_snapshot_record(
    record: &SnapshotRecord,
    package_root: &Path,
) -> Result<SnapshotPackage, SnapshotError> {
    let root = fs::canonicalize(package_root).map_err(SnapshotError::Io)?;
    let path = fs::canonicalize(&record.path).map_err(SnapshotError::Io)?;
    if !path.starts_with(&root) {
        return Err(SnapshotError::UnsafeRoot { path });
    }
    let package = SnapshotPackage::open(path)?;
    if !snapshot_records_match(
        record,
        &SnapshotRecord {
            package_id: package.manifest.package_id,
            revision: package.manifest.revision,
            feature_id: package.manifest.feature_id,
            plan_revision: package.manifest.plan_revision,
            path: record.path.clone(),
            base_tree: package.manifest.base_tree.clone(),
            result_tree: package.manifest.result_tree.clone(),
            content_hash: package.manifest.content_hash.clone(),
            parent_package_id: package.manifest.parent_package_id,
            manifest: serde_json::Value::Null,
            created_at_unix_ms: record.created_at_unix_ms,
        },
    ) {
        return Err(SnapshotError::HashMismatch {
            component: "database record",
        });
    }
    Ok(package)
}

fn sha256_file(path: &Path) -> Result<String, SnapshotError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn capture_package(
    request: CapturePackageRequest,
    store: &Mutex<Store>,
    package_root: &Path,
) -> Result<ResponseData, ApiError> {
    let (plan, workspace) = {
        let store = lock_store(store)?;
        let plan = store
            .plan(request.feature_id, Some(request.plan_revision))
            .map_err(store_api_error)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::NotFound,
                    format!(
                        "feature plan {} revision {} was not found",
                        request.feature_id,
                        request.plan_revision.get()
                    ),
                    false,
                )
            })?;
        let workspace = store
            .workspace(request.feature_id, request.plan_revision)
            .map_err(store_api_error)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::NotFound,
                    "create the owned workspace before capturing a package",
                    false,
                )
            })?;
        (plan.plan, workspace)
    };
    // Each package is the delta from the unit before it in this workspace, so the previous unit is
    // this one's parent and its result is this one's base.
    let parent_package_id = {
        let store = lock_store(store)?;
        store
            .latest_workspace_snapshot(request.feature_id, request.plan_revision)
            .map_err(store_api_error)?
            .map(|record| record.package_id)
    };
    let planned_tasks: std::collections::BTreeSet<_> = plan
        .phases
        .iter()
        .flat_map(|phase| phase.tasks.iter().map(|task| task.id))
        .collect();
    if !request.task_ids.is_subset(&planned_tasks) {
        return Err(ApiError::new(
            ApiErrorCode::InvalidRequest,
            "snapshot task selection contains an ID outside the plan revision",
            false,
        ));
    }

    let package = SnapshotPackage::capture(SnapshotRequest {
        package_id: reccursive_protocol::PackageId::new(),
        revision: Revision::FIRST,
        feature_id: request.feature_id,
        plan_revision: request.plan_revision,
        task_ids: request.task_ids,
        workspace: &workspace.path,
        package_root,
        expected_base_commit: &workspace.base_commit,
        parent_package_id,
        validation_policy: ContentValidationPolicy::default(),
    })
    .map_err(snapshot_api_error)?;
    let created_at_unix_ms = current_unix_ms()?;
    let record = SnapshotRecord {
        package_id: package.manifest.package_id,
        revision: package.manifest.revision,
        feature_id: package.manifest.feature_id,
        plan_revision: package.manifest.plan_revision,
        path: package.path.clone(),
        base_tree: package.manifest.base_tree.clone(),
        result_tree: package.manifest.result_tree.clone(),
        content_hash: package.manifest.content_hash.clone(),
        parent_package_id,
        manifest: serde_json::to_value(&package.manifest).map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "snapshot manifest could not be encoded",
                false,
            )
        })?,
        created_at_unix_ms,
    };
    lock_store(store)?
        .record_snapshot(&record)
        .map_err(store_api_error)?;
    // Capture has already advanced the workspace HEAD; the stored base has to follow it or the
    // next capture is refused as a base mismatch.
    lock_store(store)?
        .advance_workspace_base(
            request.feature_id,
            request.plan_revision,
            package.advanced_base_commit(),
        )
        .map_err(store_api_error)?;
    let captured_tasks: Vec<_> = package.manifest.task_ids.iter().copied().collect();
    lock_store(store)?
        .record_package_tasks(
            package.manifest.package_id,
            package.manifest.revision,
            captured_tasks.iter().copied(),
        )
        .map_err(store_api_error)?;
    // The package is the durable proof that these tasks were built and captured.
    for task_id in &captured_tasks {
        lock_store(store)?
            .advance_task_to(
                request.feature_id,
                request.plan_revision,
                *task_id,
                TaskStatus::Captured,
                created_at_unix_ms,
            )
            .map_err(store_api_error)?;
    }
    let checks = lock_store(store)?
        .trusted_checks(plan.repository_id)
        .map_err(store_api_error)?;
    let mut failed_check = None;
    for check in checks.into_iter().filter(|check| check.enabled) {
        let evidence = run_trusted_check(&check, &workspace, package.manifest.package_id)?;
        let passed = evidence.exit_code == Some(0) && !evidence.timed_out;
        lock_store(store)?
            .record_validation_evidence(&evidence)
            .map_err(store_api_error)?;
        if !passed {
            failed_check = Some(check.id);
            break;
        }
    }
    if let Some(check_id) = failed_check {
        // The evidence is already durable. Block the tasks rather than losing the package, so the
        // failure is visible and the same package can be resumed once the cause is fixed.
        let reason = StateReason::new(
            ReasonCode::ValidationFailed,
            format!("trusted check {check_id} failed"),
        )
        .map_err(|error| ApiError::new(ApiErrorCode::Internal, error.to_string(), false))?;
        for task_id in &captured_tasks {
            lock_store(store)?
                .advance_task(
                    request.feature_id,
                    request.plan_revision,
                    *task_id,
                    TaskStatus::Blocked,
                    Some(reason.clone()),
                    created_at_unix_ms,
                )
                .map_err(store_api_error)?;
        }
        return Err(ApiError::new(
            ApiErrorCode::InvalidRequest,
            format!(
                "trusted check {check_id} failed for captured package {}; inspect daemon logs",
                package.manifest.package_id
            ),
            false,
        ));
    }
    // Checks passed against this exact package, so the work is validated and eligible for release.
    for task_id in &captured_tasks {
        lock_store(store)?
            .advance_task_to(
                request.feature_id,
                request.plan_revision,
                *task_id,
                TaskStatus::Queued,
                created_at_unix_ms,
            )
            .map_err(store_api_error)?;
    }
    Ok(ResponseData::PackageCaptured {
        package: package_view(record, package.manifest)?,
    })
}

fn cancel_task(request: CancelTaskRequest, store: &Mutex<Store>) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let outcome = lock_store(store)?
        .cancel_task(
            request.feature_id,
            request.plan_revision,
            request.task_id,
            &request.message,
            now,
        )
        .map_err(store_api_error)?;
    Ok(ResponseData::TaskCancelled {
        cancellation: TaskCancellationView {
            feature_id: request.feature_id,
            plan_revision: request.plan_revision,
            task_id: request.task_id,
            blocked_dependents: outcome.blocked_dependents,
            invalidated_evidence: outcome.invalidated_evidence,
        },
    })
}

fn release_package(
    request: ReleasePackageRequest,
    store: &Mutex<Store>,
    managed_root: &Path,
    release_root: &Path,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let worker =
        crate::release::ReleaseWorker::new(managed_root, release_root, crate::SERVICE_NAME);
    let mut store = lock_store(store)?;
    let outcome = worker
        .release(
            &mut store,
            &crate::release::ReleaseRequest {
                package_id: request.package_id,
                package_revision: request.revision,
                message: request.message,
                author_name: request.author_name,
                author_email: request.author_email,
            },
            now,
        )
        .map_err(release_api_error)?;
    let attempt_id = match outcome {
        crate::release::ReleaseOutcome::Published { attempt_id, .. }
        | crate::release::ReleaseOutcome::Blocked { attempt_id, .. }
        | crate::release::ReleaseOutcome::Deferred { attempt_id, .. } => attempt_id,
    };
    let attempt = store
        .release_attempt(attempt_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                "release worker completed without a durable attempt",
                false,
            )
        })?;
    Ok(ResponseData::ReleaseAttempt {
        attempt: release_attempt_view(attempt),
    })
}

fn get_release_attempt(
    attempt_id: reccursive_protocol::AttemptId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let attempt = lock_store(store)?
        .release_attempt(attempt_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!("release attempt {attempt_id} was not found"),
                false,
            )
        })?;
    Ok(ResponseData::ReleaseAttempt {
        attempt: release_attempt_view(attempt),
    })
}

fn list_release_attempts(
    package_id: Option<reccursive_protocol::PackageId>,
    limit: usize,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let attempts = lock_store(store)?
        .release_attempts(package_id, limit)
        .map_err(store_api_error)?
        .into_iter()
        .map(release_attempt_view)
        .collect();
    Ok(ResponseData::ReleaseAttempts { attempts })
}

fn release_attempt_view(attempt: reccursive_store::ReleaseAttempt) -> ReleaseAttemptView {
    ReleaseAttemptView {
        attempt_id: attempt.attempt_id,
        repository_id: attempt.repository_id,
        package_id: attempt.package_id,
        package_revision: attempt.package_revision,
        target_remote: attempt.remote,
        target: attempt.target,
        base_commit: attempt.base_commit,
        candidate_sha: attempt.candidate_sha,
        candidate_parent_sha: attempt.candidate_parent_sha,
        push_intent_at_unix_ms: attempt.push_intent_at_unix_ms,
        observed_remote_sha: attempt.observed_remote_sha,
        failure_classification: attempt.failure_classification,
        failed_attempts: attempt.failed_attempts,
        retry_not_before_unix_ms: attempt.retry_not_before_unix_ms,
        status: attempt.state.status(),
        reason: attempt.state.reason().cloned(),
        blocked_from: attempt.state.blocked_from(),
        lease_expires_at_unix_ms: attempt.lease.expires_at_unix_ms,
        created_at_unix_ms: attempt.created_at_unix_ms,
        updated_at_unix_ms: attempt.updated_at_unix_ms,
    }
}

fn release_api_error(error: crate::release::ReleaseError) -> ApiError {
    match error {
        crate::release::ReleaseError::Store(error) => store_api_error(error),
        error => ApiError::new(
            ApiErrorCode::TemporarilyUnavailable,
            format!("release could not be started: {error}"),
            true,
        ),
    }
}

fn run_trusted_check(
    check: &TrustedCheck,
    workspace: &WorkspaceRecord,
    package_id: reccursive_protocol::PackageId,
) -> Result<ValidationEvidence, ApiError> {
    let command: Vec<_> = check
        .command
        .iter()
        .map(|argument| argument.replace("{base_commit}", &workspace.base_commit))
        .collect();
    let mut process = std::process::Command::new(&command[0]);
    process.args(&command[1..]).current_dir(&workspace.path);
    let mut child = process.spawn().map_err(|_| {
        ApiError::new(
            ApiErrorCode::TemporarilyUnavailable,
            "trusted check could not be started",
            true,
        )
    })?;
    let status = child
        .wait_timeout(std::time::Duration::from_secs(u64::from(
            check.timeout_seconds,
        )))
        .map_err(|_| {
            ApiError::new(
                ApiErrorCode::TemporarilyUnavailable,
                "trusted check could not be observed",
                true,
            )
        })?;
    let (exit_code, timed_out, output_summary) = match status {
        Some(status) => (status.code(), false, String::new()),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            (
                None,
                true,
                "trusted check exceeded its configured timeout".into(),
            )
        }
    };
    Ok(ValidationEvidence {
        package_id,
        revision: Revision::FIRST,
        check_id: check.id.clone(),
        command,
        exit_code,
        timed_out,
        output_summary: reccursive_store::redact_text(&output_summary),
        executed_at_unix_ms: current_unix_ms()?,
    })
}

fn get_package(
    package_id: reccursive_protocol::PackageId,
    revision: Revision,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let record = lock_store(store)?
        .snapshot(package_id, revision)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!(
                    "snapshot package {package_id} revision {} was not found",
                    revision.get()
                ),
                false,
            )
        })?;
    let package = SnapshotPackage::open(record.path.clone()).map_err(snapshot_api_error)?;
    if package.manifest.content_hash != record.content_hash {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            "snapshot record does not match its authenticated manifest",
            false,
        ));
    }
    Ok(ResponseData::Package {
        package: package_view(record, package.manifest)?,
    })
}

fn package_view(
    record: SnapshotRecord,
    manifest: reccursive_capture::SnapshotManifest,
) -> Result<PackageView, ApiError> {
    if record.package_id != manifest.package_id
        || record.revision != manifest.revision
        || record.feature_id != manifest.feature_id
        || record.plan_revision != manifest.plan_revision
        || record.base_tree != manifest.base_tree
        || record.result_tree != manifest.result_tree
    {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            "snapshot database record and manifest disagree",
            false,
        ));
    }
    Ok(PackageView {
        package_id: record.package_id,
        revision: record.revision,
        feature_id: record.feature_id,
        plan_revision: record.plan_revision,
        task_ids: manifest.task_ids,
        path: record.path.to_string_lossy().into_owned(),
        base_commit: manifest.base_commit,
        base_tree: record.base_tree,
        result_tree: record.result_tree,
        content_hash: record.content_hash,
        created_at_unix_ms: record.created_at_unix_ms,
    })
}

fn snapshot_api_error(error: SnapshotError) -> ApiError {
    let code = match &error {
        SnapshotError::AlreadyExists { .. } | SnapshotError::IncompleteExists { .. } => {
            ApiErrorCode::Conflict
        }
        SnapshotError::Io(_) | SnapshotError::Git { .. } => ApiErrorCode::TemporarilyUnavailable,
        SnapshotError::HashMismatch { .. }
        | SnapshotError::TreeMismatch { .. }
        | SnapshotError::UnsupportedSchema { .. } => ApiErrorCode::Internal,
        SnapshotError::ContentBlocked { .. } => ApiErrorCode::InvalidRequest,
        _ => ApiErrorCode::InvalidRequest,
    };
    ApiError::new(
        code,
        error.to_string(),
        code == ApiErrorCode::TemporarilyUnavailable,
    )
}

fn enroll_repository(
    request: EnrollRepositoryRequest,
    store: &Mutex<Store>,
    managed_root: &Path,
) -> Result<ResponseData, ApiError> {
    let checkout = Path::new(&request.checkout_path);
    if !checkout.is_absolute() || !checkout.is_dir() {
        return Err(ApiError::new(
            ApiErrorCode::InvalidRequest,
            "checkout_path must be an existing absolute directory",
            false,
        ));
    }

    let repository_id = RepositoryId::new();
    let managed_path = managed_root
        .join(format!("{repository_id}.git"))
        .to_string_lossy()
        .into_owned();
    let created_at_unix_ms = current_unix_ms()?;
    let registration = RepositoryRegistration::new(
        repository_id,
        request.checkout_path,
        request.canonical_remote,
        managed_path,
        created_at_unix_ms,
    )
    .map_err(store_api_error)?;
    let policy = RepositoryPolicy::new(
        repository_id,
        Revision::FIRST,
        request.publication_mode,
        request.target,
        request.development_target,
    )
    .map_err(|error| ApiError::new(ApiErrorCode::InvalidRequest, error.to_string(), false))?;
    let mut store = lock_store(store)?;
    store
        .enroll_repository(&registration, &policy)
        .map_err(store_api_error)?;
    store
        .add_trusted_check(&TrustedCheck {
            repository_id,
            id: "git_diff_check".into(),
            command: vec![
                "git".into(),
                "diff".into(),
                "--check".into(),
                "{base_commit}".into(),
            ],
            timeout_seconds: 30,
            enabled: true,
        })
        .map_err(store_api_error)?;
    Ok(ResponseData::RepositoryEnrolled {
        repository: repository_view(StoredRepository {
            registration,
            active_policy: policy,
        }),
    })
}

fn repository_view(stored: StoredRepository) -> RepositoryView {
    RepositoryView {
        id: stored.registration.id,
        checkout_path: stored.registration.checkout_path,
        canonical_remote: stored.registration.canonical_remote,
        managed_path: stored.registration.managed_path,
        policy_revision: stored.active_policy.revision,
        publication_mode: stored.active_policy.publication_mode,
        target: stored.active_policy.target,
        development_target: stored.active_policy.development_target,
    }
}

fn event_view(event: StoredEvent) -> EventView {
    EventView {
        sequence: event.sequence,
        id: event.id,
        occurred_at_unix_ms: event.occurred_at_unix_ms,
        request_id: event.context.request_id,
        attempt_id: event.context.attempt_id,
        repository_id: event.context.repository_id,
        entity_type: event.context.entity_type,
        entity_id: event.context.entity_id,
        entity_revision: event.context.entity_revision,
        kind: event.kind,
        severity: match event.severity {
            EventSeverity::Debug => EventSeverityView::Debug,
            EventSeverity::Info => EventSeverityView::Info,
            EventSeverity::Warning => EventSeverityView::Warning,
            EventSeverity::Error => EventSeverityView::Error,
        },
        reason_code: event.reason_code,
        message: event.message,
        details: event.details,
    }
}

fn plan_view(stored: StoredPlan) -> PlanView {
    PlanView {
        plan: stored.plan,
        created_at_unix_ms: stored.created_at_unix_ms,
    }
}

fn lock_store(store: &Mutex<Store>) -> Result<std::sync::MutexGuard<'_, Store>, ApiError> {
    store.lock().map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "service state lock is unavailable",
            true,
        )
    })
}

fn current_unix_ms() -> Result<i64, ApiError> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "system clock is before the Unix epoch",
            false,
        )
    })?;
    i64::try_from(duration.as_millis()).map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "system clock exceeds the supported timestamp range",
            false,
        )
    })
}

fn store_api_error(error: StoreError) -> ApiError {
    match error {
        StoreError::Conflict(_) => ApiError::new(
            ApiErrorCode::Conflict,
            "repository or policy conflicts with an existing profile",
            false,
        ),
        StoreError::InvalidData(message) => {
            ApiError::new(ApiErrorCode::InvalidRequest, message, false)
        }
        StoreError::PolicyRepositoryMismatch { .. } => ApiError::new(
            ApiErrorCode::InvalidRequest,
            "repository and policy identifiers do not match",
            false,
        ),
        StoreError::FutureSchema { .. } => ApiError::new(
            ApiErrorCode::Internal,
            "database was created by a newer application version",
            false,
        ),
        _ => ApiError::new(
            ApiErrorCode::TemporarilyUnavailable,
            "local state is temporarily unavailable",
            true,
        ),
    }
}

/// Service startup, ownership, transport, and storage failures.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("another service owner holds {}", path.display())]
    AlreadyRunning { path: PathBuf },
    #[error("state path is not a secure directory: {}", path.display())]
    InsecureStateDirectory { path: PathBuf },
    #[error("service state file is a symlink or non-file: {}", path.display())]
    InsecureStateFile { path: PathBuf },
    #[error("local API socket path is occupied by a non-socket file: {}", path.display())]
    SocketPathOccupied { path: PathBuf },
    #[error("service I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Protocol(#[from] ProtocolValidationError),
    #[error("local API worker panicked")]
    WorkerPanicked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use reccursive_protocol::{
        API_VERSION, AcceptanceCheck, AttemptId, FeaturePlan, LocalClient, PLAN_SCHEMA_VERSION,
        PlanPhase, PlanTask, PublicationMode, ResponseData, TargetRef,
    };
    use std::collections::BTreeMap;
    use std::net::Shutdown;
    use tempfile::tempdir;

    #[test]
    fn only_one_service_can_own_a_state_directory() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let first = LocalService::bind(paths.clone()).unwrap();
        assert!(matches!(
            LocalService::bind(paths.clone()),
            Err(ServiceError::AlreadyRunning { .. })
        ));
        drop(first);
        LocalService::bind(paths).unwrap();
    }

    #[test]
    fn state_directory_token_and_socket_are_owner_only() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path().join("state"));
        let _service = LocalService::bind(paths.clone()).unwrap();
        assert_eq!(
            fs::metadata(&paths.state_dir).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(&paths.auth_token)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(&paths.socket).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn authenticated_ping_is_correlated_and_reports_store_schema() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let response = LocalClient::from_token_file(&paths.auth_token)
            .unwrap()
            .send(&paths.socket, Command::Ping)
            .unwrap();
        assert_eq!(response.api_version, API_VERSION);
        assert!(matches!(
            response.result,
            Ok(ResponseData::Pong {
                schema_version: reccursive_store::STORAGE_SCHEMA_VERSION,
                ..
            })
        ));
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn attempt_inspection_is_available_over_the_authenticated_local_api() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let response = LocalClient::from_token_file(&paths.auth_token)
            .unwrap()
            .send(
                &paths.socket,
                Command::GetReleaseAttempt {
                    attempt_id: AttemptId::new(),
                },
            )
            .unwrap();
        assert_eq!(response.result.unwrap_err().code, ApiErrorCode::NotFound);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn authenticated_requests_are_available_as_correlated_events() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(2));
        let client = LocalClient::from_token_file(&paths.auth_token).unwrap();

        let ping = client.send(&paths.socket, Command::Ping).unwrap();
        let ping_request_id = ping.request_id;
        let logs = client
            .send(&paths.socket, Command::ListEvents { limit: 10 })
            .unwrap();

        let events = match logs.result.unwrap() {
            ResponseData::Events { events } => events,
            data => panic!("expected events, received {data:?}"),
        };
        let event = events
            .iter()
            .find(|event| event.request_id == Some(ping_request_id))
            .expect("ping event should be correlated by request ID");
        assert_eq!(event.kind, "api.request_succeeded");
        assert_eq!(event.details["command"], "ping");
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn plan_import_round_trips_through_the_service() {
        let directory = tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        let paths = ServicePaths::new(directory.path().join("state"));
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(3));
        let client = LocalClient::from_token_file(&paths.auth_token).unwrap();

        let enrollment = client
            .send(
                &paths.socket,
                Command::EnrollRepository(EnrollRepositoryRequest {
                    checkout_path: checkout.to_string_lossy().into_owned(),
                    canonical_remote: "ssh://git@example.invalid/project.git".into(),
                    publication_mode: PublicationMode::ScheduledCreation,
                    target: TargetRef::new("refs/heads/main").unwrap(),
                    development_target: None,
                }),
            )
            .unwrap()
            .result
            .unwrap();
        let repository_id = match enrollment {
            ResponseData::RepositoryEnrolled { repository } => repository.id,
            data => panic!("expected enrollment, received {data:?}"),
        };
        let plan = FeaturePlan {
            schema_version: PLAN_SCHEMA_VERSION,
            feature_id: reccursive_protocol::FeatureId::new(),
            revision: Revision::FIRST,
            repository_id,
            goal: "Round-trip an imported plan".into(),
            target: TargetRef::new("refs/heads/main").unwrap(),
            sealed: true,
            phases: vec![PlanPhase {
                id: "delivery".into(),
                name: "Delivery".into(),
                tasks: vec![PlanTask {
                    id: reccursive_protocol::TaskId::new(),
                    name: "Import through the daemon".into(),
                    dependencies: BTreeMap::new(),
                    acceptance_checks: vec![AcceptanceCheck {
                        id: "round_trip".into(),
                        description: "The exact plan returns through the API".into(),
                    }],
                }],
            }],
        };

        let imported = client
            .send(&paths.socket, Command::ImportPlan { plan: plan.clone() })
            .unwrap();
        assert!(matches!(
            imported.result,
            Ok(ResponseData::PlanImported { .. })
        ));
        let loaded = client
            .send(
                &paths.socket,
                Command::GetPlan {
                    feature_id: plan.feature_id,
                    revision: None,
                },
            )
            .unwrap();
        let returned = match loaded.result.unwrap() {
            ResponseData::Plan { plan } => plan.plan,
            data => panic!("expected plan, received {data:?}"),
        };
        assert_eq!(returned, plan);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn unauthorized_requests_receive_a_stable_error() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let response = LocalClient::new(AuthToken::new("b".repeat(32)).unwrap())
            .send(&paths.socket, Command::Ping)
            .unwrap();
        assert_eq!(
            response.result.unwrap_err().code,
            ApiErrorCode::Unauthorized
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn unsupported_protocol_version_is_reported_with_the_request_id() {
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let worker = thread::spawn(move || service.serve_connections(1));
        let token = AuthToken::new(fs::read_to_string(&paths.auth_token).unwrap()).unwrap();
        let mut request = RequestEnvelope::new(token, Command::Ping);
        request.api_version = API_VERSION + 1;
        let request_id = request.request_id;
        let mut stream = UnixStream::connect(&paths.socket).unwrap();
        write_message(&mut stream, &request).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let response: ResponseEnvelope = read_message(&mut stream).unwrap();
        assert_eq!(response.request_id, request_id);
        assert_eq!(
            response.result.unwrap_err().code,
            ApiErrorCode::UnsupportedVersion
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn concurrent_clients_share_one_service_owner() {
        const CLIENTS: usize = 8;
        let directory = tempdir().unwrap();
        let paths = ServicePaths::new(directory.path());
        let service = LocalService::bind(paths.clone()).unwrap();
        let server = thread::spawn(move || service.serve_connections(CLIENTS));
        let client = LocalClient::from_token_file(&paths.auth_token).unwrap();
        let workers: Vec<_> = (0..CLIENTS)
            .map(|_| {
                let client = client.clone();
                let socket = paths.socket.clone();
                thread::spawn(move || client.send(socket, Command::Ping))
            })
            .collect();
        for worker in workers {
            assert!(worker.join().unwrap().unwrap().result.is_ok());
        }
        server.join().unwrap().unwrap();
    }
}
