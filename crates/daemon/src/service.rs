use std::{
    collections::{BTreeSet, HashMap, HashSet},
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

use crate::scheduler::{Scheduler, SchedulerError};
use reccursive_capture::{
    ContentValidationPolicy, OwnedWorkspace, PrerequisiteState, SnapshotError, SnapshotPackage,
    SnapshotRequest, WorkspaceError, WorkspaceRequest as CaptureWorkspaceRequest,
};
use reccursive_protocol::{
    ApiError, ApiErrorCode, AuthToken, CancelTaskRequest, CapturePackageRequest, Command,
    CreateReleaseUnitRequest, CreateWorkspaceRequest, DueUnitView, EnrollRepositoryRequest,
    EventSeverityView, EventView, FeatureId, FeatureStatusView, IdempotencyKey,
    IntegrationHealthView, MissedWindowView, PackageView, PlanView, ProtocolValidationError,
    QueueAuditView, QueueExportView, QueueRecoveryIssueView, ReasonCode, ReleaseAttemptView,
    ReleasePackageRequest, ReleaseUnitView, RepositoryId, RepositoryPolicy, RepositoryView,
    RequestEnvelope, RequestId, ResponseData, ResponseEnvelope, Revision, SchedulePolicyView,
    ScheduleRecalculationView, ScheduleSlotView, ScheduleUnitRequest, SetSchedulePolicyRequest,
    StateReason, SubmissionView, TaskCancellationView, TaskProgressView, TaskStatus,
    TransportError, WorkspacePrerequisiteView, WorkspaceView,
    transport::{read_message, write_message},
};
use reccursive_store::{
    DEFAULT_EVENT_RETENTION, EventContext, EventSeverity, IdempotencyClaim, NewEvent,
    RepositoryRegistration, SnapshotRecord, SnapshotRecoveryIssue, Store, StoreError, StoredEvent,
    StoredPlan, StoredRepository, StoredSchedulePolicy, TrustedCheck, ValidationEvidence,
    WorkspaceRecord,
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
/// How often the daemon performs its periodic pass.
///
/// Coarse on purpose: scheduling resolution is minutes, and an idle daemon that wakes constantly to
/// ask a question whose answer is almost always "nothing" costs battery for no benefit.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How many units one pass may publish.
///
/// A bound rather than a throughput target: a backlog that accumulated over a long offline period
/// should drain steadily across passes instead of becoming a burst of simultaneous pushes.
const RELEASE_CONCURRENCY_LIMIT: usize = 4;

pub struct LocalService {
    owner: ServiceOwner,
    store: Arc<Mutex<Store>>,
    /// Serializes whole submissions.
    ///
    /// A submission reads durable state, captures, groups and schedules — four steps that each
    /// take the store lock and release it. Two submissions of the same task interleaving in that
    /// gap both find nothing captured and both capture, which is the one outcome this command
    /// exists to prevent. Held across the sequence rather than inside it, and taken only by
    /// submission, so nothing else waits on it and there is no second lock order to get wrong.
    submission: Arc<Mutex<()>>,
    managed_root: Arc<PathBuf>,
    workspace_root: Arc<PathBuf>,
    release_root: Arc<PathBuf>,
    package_root: Arc<PathBuf>,
    shutdown: crate::maintenance::ShutdownSignal,
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
        reconcile_overdue_schedules(&mut store)?;
        Ok(Self {
            owner,
            store: Arc::new(Mutex::new(store)),
            submission: Arc::new(Mutex::new(())),
            managed_root: Arc::new(managed_root),
            workspace_root: Arc::new(workspace_root),
            release_root: Arc::new(release_root),
            package_root: Arc::new(package_root),
            shutdown: crate::maintenance::ShutdownSignal::new(),
        })
    }

    /// Starts the periodic maintenance pass.
    ///
    /// Kept coarse on purpose. A slot whose time arrives between two passes is simply overdue at
    /// the next one, which is the same state a slot reaches after a sleep and is handled by the
    /// same path — so nothing depends on a pass landing at any particular moment, and the daemon
    /// can stay cheap while idle instead of polling.
    fn spawn_maintenance(&self) -> thread::JoinHandle<()> {
        let store = Arc::clone(&self.store);
        let shutdown = self.shutdown.clone();
        let managed_root = Arc::clone(&self.managed_root);
        let release_root = Arc::clone(&self.release_root);
        let interval = MAINTENANCE_INTERVAL;
        let mut maintenance = crate::maintenance::Maintenance::new(
            shutdown.clone(),
            interval,
            std::time::Instant::now(),
            current_unix_ms().unwrap_or(0),
        );
        thread::spawn(move || {
            while !shutdown.is_requested() {
                thread::sleep(interval);
                if shutdown.is_requested() {
                    break;
                }
                let Ok(now) = current_unix_ms() else { continue };
                let seed = u64::from(Uuid::new_v4().as_fields().0);
                if let Ok(mut store) = store.lock() {
                    // A maintenance failure is never fatal: the next pass tries again, and the
                    // state it would have written is still discoverable from what is durable.
                    let _ = maintenance.tick(&mut store, std::time::Instant::now(), now, seed);
                    // The pass that notices a slot is due is the same one that acts on it. Without
                    // this a schedule is only ever a record of intent, and publishing still waits
                    // for someone to type a command.
                    let worker = crate::release::ReleaseWorker::new(
                        managed_root.as_path(),
                        release_root.as_path(),
                        crate::SERVICE_NAME,
                    );
                    let _ = maintenance.release_due_units(
                        &mut store,
                        &worker,
                        now,
                        RELEASE_CONCURRENCY_LIMIT,
                    );
                }
            }
        })
    }

    /// Serves forever, creating one worker thread per accepted local connection.
    pub fn serve_forever(self) -> Result<(), ServiceError> {
        let _maintenance = self.spawn_maintenance();
        for connection in self.owner.listener.incoming() {
            let stream = connection?;
            let auth_token = self.owner.auth_token.clone();
            let store = Arc::clone(&self.store);
            let submission = Arc::clone(&self.submission);
            let managed_root = Arc::clone(&self.managed_root);
            let workspace_root = Arc::clone(&self.workspace_root);
            let release_root = Arc::clone(&self.release_root);
            let package_root = Arc::clone(&self.package_root);
            thread::spawn(move || {
                let _ = handle_connection(
                    stream,
                    &auth_token,
                    &store,
                    &submission,
                    ServiceRoots {
                        managed: &managed_root,
                        workspace: &workspace_root,
                        release: &release_root,
                        package: &package_root,
                    },
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
            let submission = Arc::clone(&self.submission);
            let managed_root = Arc::clone(&self.managed_root);
            let workspace_root = Arc::clone(&self.workspace_root);
            let release_root = Arc::clone(&self.release_root);
            let package_root = Arc::clone(&self.package_root);
            workers.push(thread::spawn(move || {
                handle_connection(
                    stream,
                    &auth_token,
                    &store,
                    &submission,
                    ServiceRoots {
                        managed: &managed_root,
                        workspace: &workspace_root,
                        release: &release_root,
                        package: &package_root,
                    },
                )
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| ServiceError::WorkerPanicked)??;
        }
        Ok(())
    }
}

/// The four directories the daemon owns, passed together because they always travel together.
#[derive(Clone, Copy)]
struct ServiceRoots<'a> {
    managed: &'a Path,
    workspace: &'a Path,
    release: &'a Path,
    package: &'a Path,
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
    submission: &Mutex<()>,
    roots: ServiceRoots<'_>,
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
        let result = execute_command(
            request.command,
            KeyedRequest {
                idempotency_key: request.idempotency_key.as_ref(),
                command_name,
            },
            store,
            submission,
            roots,
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

/// What the caller said about repeating this request, alongside the name used to record it.
struct KeyedRequest<'a> {
    idempotency_key: Option<&'a IdempotencyKey>,
    command_name: &'static str,
}

/// Carries out one command, replaying an earlier result when the caller named a repeated intent.
///
/// Without a key this is `dispatch` and nothing more. With one, the key is claimed *before* the
/// command runs: a duplicate arriving during the first attempt is told to wait rather than allowed
/// to act alongside it, and a duplicate arriving afterwards is handed the original outcome instead
/// of a second one.
fn execute_command(
    command: Command,
    request: KeyedRequest<'_>,
    store: &Mutex<Store>,
    submission: &Mutex<()>,
    roots: ServiceRoots<'_>,
) -> Result<ResponseData, ApiError> {
    let KeyedRequest {
        idempotency_key,
        command_name,
    } = request;
    // A key on a query is accepted and ignored: repeating a question is already safe, and
    // recording its answer would freeze that answer for every later use of the key.
    let Some(key) = idempotency_key.filter(|_| command.changes_state()) else {
        return dispatch(command, store, submission, roots);
    };

    let fingerprint = command.fingerprint();
    let may_reach_remote = command.may_reach_remote();
    let claimed_at = current_unix_ms()?;
    let claim = lock_store(store)?
        .claim_idempotency_key(key.as_str(), command_name, &fingerprint, claimed_at)
        .map_err(store_api_error)?;

    match claim {
        IdempotencyClaim::Settled { outcome } => return replay_outcome(key, &outcome),
        // Reachable: each connection is handled on its own thread, and the store lock is released
        // for the duration of the command. A duplicate is refused rather than queued behind the
        // original, because holding a connection open for the length of a publication is a worse
        // answer than telling the caller to ask again.
        IdempotencyClaim::InProgress => {
            return Err(ApiError::new(
                ApiErrorCode::TemporarilyUnavailable,
                format!(
                    "idempotency key {key} names a request that is still running;                      retry to collect its result"
                ),
                true,
            ));
        }
        IdempotencyClaim::Mismatched { command: earlier } => {
            return Err(ApiError::new(
                ApiErrorCode::Conflict,
                format!("idempotency key {key} was already used for a different {earlier} request"),
                false,
            ));
        }
        IdempotencyClaim::Claimed => {}
    }

    let result = dispatch(command, store, submission, roots);

    // A failure that provably never left this machine gives the key back, so a real retry runs. A
    // failure anywhere a push could have reached the remote keeps it: the error alone does not say
    // whether the remote moved, and the durable attempt record is where that is settled.
    let release = match &result {
        Ok(_) => false,
        Err(error) => error.retryable && !may_reach_remote,
    };
    let mut store = lock_store(store)?;
    if release {
        store
            .release_idempotency_key(key.as_str())
            .map_err(store_api_error)?;
        return result;
    }
    let outcome = serde_json::to_string(&result).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("the result of {command_name} could not be recorded for replay: {error}"),
            false,
        )
    })?;
    let settled_at = current_unix_ms()?;
    store
        .settle_idempotency_key(key.as_str(), &outcome, settled_at)
        .map_err(store_api_error)?;
    result
}

/// Returns what the first request carrying this key returned.
fn replay_outcome(key: &IdempotencyKey, outcome: &str) -> Result<ResponseData, ApiError> {
    serde_json::from_str::<Result<ResponseData, ApiError>>(outcome).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("the recorded result for idempotency key {key} is unreadable: {error}"),
            false,
        )
    })?
}

fn dispatch(
    command: Command,
    store: &Mutex<Store>,
    submission: &Mutex<()>,
    roots: ServiceRoots<'_>,
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
        Command::CreateWorkspace(request) => create_workspace(request, store, roots.workspace),
        Command::GetWorkspace {
            feature_id,
            revision,
        } => get_workspace(feature_id, revision, store),
        Command::CapturePackage(request) => capture_package(request, store, roots.package),
        Command::CancelTask(request) => cancel_task(request, store),
        Command::SubmitTask(request) => submit_task(request, store, submission, roots.package),
        Command::SealPlan { feature_id } => seal_plan(feature_id, store),
        Command::GetFeatureStatus {
            feature_id,
            revision,
        } => get_feature_status(feature_id, revision, store),
        Command::ReleasePackage(request) => {
            release_package(request, store, roots.managed, roots.release)
        }
        Command::CreateReleaseUnit(request) => create_release_unit(request, store),
        Command::GetReleaseUnit { release_unit_id } => get_release_unit(release_unit_id, store),
        Command::SetSchedulePolicy(request) => set_schedule_policy(request, store),
        Command::GetSchedulePolicy { repository_id } => get_schedule_policy(repository_id, store),
        Command::ScheduleUnit(request) => schedule_unit(request, store),
        Command::GetScheduleSlot { release_unit_id } => get_schedule_slot(release_unit_id, store),
        Command::WithdrawScheduleSlot {
            release_unit_id,
            reason,
        } => withdraw_schedule_slot(release_unit_id, &reason, store),
        Command::RecalculateSchedule {
            repository_id,
            reason,
        } => recalculate_schedule(repository_id, &reason, store),
        Command::ReconcileMissedWindows {
            repository_id,
            seed,
        } => reconcile_missed_windows(repository_id, seed, store),
        Command::SetScheduleOverride {
            repository_id,
            override_policy,
        } => set_schedule_override(repository_id, &override_policy, store),
        Command::ListDueUnits { concurrency_limit } => list_due_units(concurrency_limit, store),
        Command::PauseRepository {
            repository_id,
            reason,
        } => pause_repository(repository_id, &reason, store),
        Command::ResumeRepository { repository_id } => resume_repository(repository_id, store),
        Command::ReleaseUnitNow { release_unit_id } => release_unit_now(release_unit_id, store),
        Command::PreviewSchedule { repository_id } => preview_schedule(repository_id, store),
        Command::ListIntegrationHealth => list_integration_health(store),
        Command::DiagnoseRepository { repository_id } => diagnose_repository(repository_id, store),
        Command::GetReleaseAttempt { attempt_id } => get_release_attempt(attempt_id, store),
        Command::ListReleaseAttempts { package_id, limit } => {
            list_release_attempts(package_id, limit, store)
        }
        Command::GetPackage {
            package_id,
            revision,
        } => get_package(package_id, revision, store),
        Command::AuditQueue => audit_queue(store, roots.package),
        Command::ExportQueue { destination } => export_queue(destination, store, roots.package),
        Command::EnrollRepository(request) => enroll_repository(request, store, roots.managed),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Ping => "ping",
        Command::EnrollRepository(_) => "repository.add",
        Command::ListRepositories => "repository.list",
        Command::ListEvents { .. } => "logs",
        Command::ImportPlan { .. } => "plan.import",
        Command::SealPlan { .. } => "plan.seal",
        Command::SubmitTask(_) => "task.submit",
        Command::GetFeatureStatus { .. } => "feature.status",
        Command::GetPlan { .. } => "plan.show",
        Command::PlanHistory { .. } => "plan.history",
        Command::CreateWorkspace(_) => "workspace.create",
        Command::GetWorkspace { .. } => "workspace.show",
        Command::CapturePackage(_) => "package.capture",
        Command::CancelTask(_) => "task.cancel",
        Command::ReleasePackage(_) => "release.publish",
        Command::CreateReleaseUnit(_) => "release.unit.create",
        Command::GetReleaseUnit { .. } => "release.unit.show",
        Command::SetSchedulePolicy(_) => "schedule.policy.set",
        Command::GetSchedulePolicy { .. } => "schedule.policy.show",
        Command::ScheduleUnit(_) => "schedule.unit",
        Command::GetScheduleSlot { .. } => "schedule.slot.show",
        Command::WithdrawScheduleSlot { .. } => "schedule.withdraw",
        Command::RecalculateSchedule { .. } => "schedule.recalculate",
        Command::ReconcileMissedWindows { .. } => "schedule.missed_windows",
        Command::SetScheduleOverride { .. } => "schedule.override.set",
        Command::ListDueUnits { .. } => "schedule.due",
        Command::PauseRepository { .. } => "schedule.pause",
        Command::ResumeRepository { .. } => "schedule.resume",
        Command::ReleaseUnitNow { .. } => "schedule.release_now",
        Command::PreviewSchedule { .. } => "schedule.preview",
        Command::ListIntegrationHealth => "integrations.health",
        Command::DiagnoseRepository { .. } => "repository.diagnose",
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
/// Applies each repository's missed-window policy to anything overdue at startup.
///
/// This is what makes a scheduled release a durable deadline rather than a timer. A machine that
/// slept through a window, or was simply off, starts up with work whose time has already passed;
/// nothing fired while it was away, so the reconciliation has to happen on the way up rather than
/// in response to an event nobody was present to receive.
///
/// Best-effort by design: a repository with no policy configured yet, or one whose selection fails,
/// must not stop the service from starting.
fn reconcile_overdue_schedules(store: &mut Store) -> Result<(), StoreError> {
    let now = recovery_unix_ms()?;
    let seed = u64::from(Uuid::new_v4().as_fields().0);
    for repository in store.repositories()? {
        let repository_id = repository.registration.id;
        if store.overdue_schedule_slots(repository_id, now)?.is_empty() {
            continue;
        }
        if let Err(error) =
            crate::scheduler::Scheduler::reconcile_missed_windows(store, repository_id, seed, now)
        {
            record_recovery_issue(
                store,
                PathBuf::from(repository_id.to_string()),
                "missed_window_unreconciled",
                &format!("overdue release times could not be reconciled at startup: {error}"),
                now,
            )?;
        }
    }
    Ok(())
}

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

    // A task that already reached `captured` has a package holding its work. Capturing again
    // would produce a second package delivering the same task, which is the duplication an agent
    // retrying a step must never cause. A blocked task is deliberately still capturable: that is
    // the fix-and-resubmit path after a failed check, and its content has changed.
    for task_id in &request.task_ids {
        let existing = lock_store(store)?
            .task(request.feature_id, request.plan_revision, *task_id)
            .map_err(store_api_error)?;
        if let Some(task) = existing
            && task
                .state
                .status()
                .is_at_or_past(reccursive_protocol::TaskStatus::Captured)
                == Some(true)
        {
            return Err(ApiError::new(
                ApiErrorCode::Conflict,
                format!(
                    "task {task_id} is already {status:?} and its work is captured; capture a new plan revision, or cancel it first, rather than capturing it twice",
                    status = task.state.status()
                ),
                false,
            ));
        }
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

/// Fixes a feature's scope by appending a sealed copy of its newest revision.
///
/// Sealing appends rather than editing. A stored plan revision means one thing forever — packages,
/// units and attempts all name a revision, and P6-T04's scope enforcement is about to depend on
/// that — so the revision an agent sealed is a new one, and the response says which.
fn seal_plan(feature_id: FeatureId, store: &Mutex<Store>) -> Result<ResponseData, ApiError> {
    let created_at_unix_ms = current_unix_ms()?;
    let mut store = lock_store(store)?;
    let latest = store
        .plan(feature_id, None)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!("feature plan {feature_id} was not found"),
                false,
            )
        })?;
    if latest.plan.sealed {
        return Err(ApiError::new(
            ApiErrorCode::Conflict,
            format!(
                "feature plan {feature_id} revision {} is already sealed",
                latest.plan.revision.get()
            ),
            false,
        ));
    }
    let next = Revision::new(latest.plan.revision.get() + 1)
        .map_err(|error| ApiError::new(ApiErrorCode::InvalidRequest, error.to_string(), false))?;
    let sealed = reccursive_protocol::FeaturePlan {
        revision: next,
        sealed: true,
        ..latest.plan
    };
    store
        .import_plan(&sealed, created_at_unix_ms)
        .map_err(store_api_error)?;
    Ok(ResponseData::PlanSealed {
        plan: PlanView {
            plan: sealed,
            created_at_unix_ms,
        },
    })
}

/// Reports every task of one plan revision with whatever durable work is attached to it.
fn get_feature_status(
    feature_id: FeatureId,
    revision: Option<Revision>,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
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
    let plan_revision = plan.plan.revision;

    let mut tasks = Vec::new();
    for record in store
        .plan_tasks(feature_id, plan_revision)
        .map_err(store_api_error)?
    {
        let package = store
            .snapshot_package_for_task(feature_id, plan_revision, record.task_id)
            .map_err(store_api_error)?;
        let unit = store
            .release_unit_for_task(feature_id, plan_revision, record.task_id)
            .map_err(store_api_error)?;
        let selected_at_unix_ms = match &unit {
            Some(unit) => store
                .schedule_slot(unit.unit_id)
                .map_err(store_api_error)?
                .map(|slot| slot.selected_at_unix_ms),
            None => None,
        };
        tasks.push(TaskProgressView {
            task_id: record.task_id,
            name: record.name,
            status: record.state.status(),
            blocked_from: record.state.blocked_from(),
            reason: record.state.reason().cloned(),
            updated_at_unix_ms: record.updated_at_unix_ms,
            package_id: package.map(|package| package.package_id),
            release_unit_id: unit.map(|unit| unit.unit_id),
            selected_at_unix_ms,
        });
    }

    Ok(ResponseData::FeatureStatus {
        status: FeatureStatusView {
            feature_id,
            plan_revision,
            repository_id: plan.plan.repository_id,
            goal: plan.plan.goal,
            sealed: plan.plan.sealed,
            tasks,
        },
    })
}

/// Captures, groups and schedules one task's work as a single intent.
///
/// Every step is a step the caller could take separately; what this adds is that *repeating* it is
/// harmless without needing an idempotency key. Each of the three steps already answers a repeat
/// with what it produced the first time, so the composition does too — an agent that cannot tell
/// whether its submission landed can simply submit again.
fn submit_task(
    request: reccursive_protocol::SubmitTaskRequest,
    store: &Mutex<Store>,
    submission: &Mutex<()>,
    package_root: &Path,
) -> Result<ResponseData, ApiError> {
    // Held for the whole sequence. The four steps below each take and release the store lock, and
    // two submissions of the same task interleaving in one of those gaps would both find nothing
    // captured and both capture it — producing the duplicate this command exists to prevent, plus
    // a package no unit publishes. A poisoned lock means an earlier submission panicked partway;
    // the state it left is not something a new submission should build on.
    let _submitting = submission.lock().map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "an earlier submission failed partway and left the service unable to accept another",
            false,
        )
    })?;

    let (plan_revision, existing) = {
        let store = lock_store(store)?;
        let plan = store
            .plan(request.feature_id, request.plan_revision)
            .map_err(store_api_error)?
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::NotFound,
                    format!("feature plan {} was not found", request.feature_id),
                    false,
                )
            })?;
        let plan_revision = plan.plan.revision;
        if request.task_ids.is_empty() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidRequest,
                "a submission must name at least one task",
                false,
            ));
        }
        let mut existing: Option<SnapshotRecord> = None;
        // Inspect every requested task before capture. Looking at only the first made task order
        // observable: `[new, already_captured]` could write a new package before the lifecycle
        // rejected the second task. A repeat is valid only when every named task is already
        // carried by one package with this exact set; anything else is a different submission and
        // must leave the queue untouched.
        for task_id in &request.task_ids {
            let Some(package) = store
                .snapshot_package_for_task(request.feature_id, plan_revision, *task_id)
                .map_err(store_api_error)?
            else {
                continue;
            };
            let carried: BTreeSet<_> = store
                .package_task_ids(package.package_id, package.revision)
                .map_err(store_api_error)?
                .into_iter()
                .collect();
            if carried != request.task_ids {
                return Err(ApiError::new(
                    ApiErrorCode::Conflict,
                    format!(
                        "task {task_id} is already captured in package {}, which carries \
                         different work; cancel it or submit a new plan revision",
                        package.package_id
                    ),
                    false,
                ));
            }
            if let Some(previous) = &existing {
                if previous.package_id != package.package_id
                    || previous.revision != package.revision
                {
                    return Err(ApiError::new(
                        ApiErrorCode::Conflict,
                        format!(
                            "submitted tasks are already captured by different packages ({} and \
                             {}); cancel them or submit a new plan revision",
                            previous.package_id, package.package_id
                        ),
                        false,
                    ));
                }
            } else {
                existing = Some(package);
            }
        }
        (plan_revision, existing)
    };

    let created = existing.is_none();
    let package = match existing {
        Some(record) => {
            let opened = SnapshotPackage::open(record.path.clone()).map_err(snapshot_api_error)?;
            if opened.manifest.content_hash != record.content_hash {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    "snapshot record does not match its authenticated manifest",
                    false,
                ));
            }
            package_view(record, opened.manifest)?
        }
        None => match capture_package(
            CapturePackageRequest {
                feature_id: request.feature_id,
                plan_revision,
                task_ids: request.task_ids.clone(),
            },
            store,
            package_root,
        )? {
            ResponseData::PackageCaptured { package } => package,
            _ => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    "capture returned an unexpected result",
                    false,
                ));
            }
        },
    };

    let now = current_unix_ms()?;
    let unit = lock_store(store)?
        .create_release_unit(
            reccursive_protocol::ReleaseUnitId::new(),
            request.feature_id,
            plan_revision,
            request.task_ids,
            now,
        )
        .map_err(store_api_error)?;
    let unit = release_unit_view(unit);

    let slot = {
        let mut store = lock_store(store)?;
        Scheduler::schedule(
            &mut store,
            unit.unit_id,
            package.package_id,
            package.revision,
            request.seed,
            now,
        )
        .map_err(scheduler_api_error)?
    };

    Ok(ResponseData::TaskSubmitted {
        submission: SubmissionView {
            package,
            unit,
            slot: schedule_slot_view(slot),
            created,
        },
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

fn create_release_unit(
    request: CreateReleaseUnitRequest,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let unit = lock_store(store)?
        .create_release_unit(
            request.release_unit_id,
            request.feature_id,
            request.plan_revision,
            request.task_ids,
            now,
        )
        .map_err(store_api_error)?;
    Ok(ResponseData::ReleaseUnitCreated {
        unit: release_unit_view(unit),
    })
}

fn get_release_unit(
    release_unit_id: reccursive_protocol::ReleaseUnitId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let unit = lock_store(store)?
        .release_unit(release_unit_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!("release unit {release_unit_id} was not found"),
                false,
            )
        })?;
    Ok(ResponseData::ReleaseUnit {
        unit: release_unit_view(unit),
    })
}

fn release_unit_view(unit: reccursive_store::ReleaseUnitRecord) -> ReleaseUnitView {
    ReleaseUnitView {
        unit_id: unit.unit_id,
        feature_id: unit.feature_id,
        plan_revision: unit.plan_revision,
        task_ids: unit.task_ids,
        created_at_unix_ms: unit.created_at_unix_ms,
    }
}

fn set_schedule_policy(
    request: SetSchedulePolicyRequest,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let mut store = lock_store(store)?;
    let next_revision = store
        .schedule_policy(request.repository_id)
        .map_err(store_api_error)?
        .map_or(Revision::FIRST, |current| {
            current.revision.next().unwrap_or(current.revision)
        });
    store
        .activate_schedule_policy(&StoredSchedulePolicy {
            repository_id: request.repository_id,
            revision: next_revision,
            policy: request.policy,
            created_at_unix_ms: now,
        })
        .map_err(store_api_error)?;
    let activated = store
        .schedule_policy(request.repository_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                "activated schedule policy could not be read back",
                false,
            )
        })?;
    Ok(ResponseData::SchedulePolicyActivated {
        policy: schedule_policy_view(activated),
    })
}

fn get_schedule_policy(
    repository_id: RepositoryId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let policy = lock_store(store)?
        .schedule_policy(repository_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!("repository {repository_id} has no active schedule policy"),
                false,
            )
        })?;
    Ok(ResponseData::SchedulePolicy {
        policy: schedule_policy_view(policy),
    })
}

fn schedule_policy_view(policy: StoredSchedulePolicy) -> SchedulePolicyView {
    SchedulePolicyView {
        repository_id: policy.repository_id,
        revision: policy.revision,
        policy: policy.policy,
        created_at_unix_ms: policy.created_at_unix_ms,
    }
}

fn schedule_unit(
    request: ScheduleUnitRequest,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let mut store = lock_store(store)?;
    let slot = Scheduler::schedule(
        &mut store,
        request.release_unit_id,
        request.package_id,
        request.package_revision,
        request.seed,
        now,
    )
    .map_err(scheduler_api_error)?;
    Ok(ResponseData::ScheduleSlot {
        slot: schedule_slot_view(slot),
    })
}

fn get_schedule_slot(
    release_unit_id: reccursive_protocol::ReleaseUnitId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let slot = lock_store(store)?
        .schedule_slot(release_unit_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!("release unit {release_unit_id} has no durable schedule slot"),
                false,
            )
        })?;
    Ok(ResponseData::ScheduleSlot {
        slot: schedule_slot_view(slot),
    })
}

fn withdraw_schedule_slot(
    release_unit_id: reccursive_protocol::ReleaseUnitId,
    reason: &str,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let withdrawn = lock_store(store)?
        .invalidate_schedule_slot(release_unit_id, reason, now)
        .map_err(store_api_error)?;
    // Reported as a recalculation rather than a slot, because after this call there is no live
    // slot to return: the unit is back in the queue awaiting a fresh selection.
    Ok(ResponseData::ScheduleRecalculated {
        recalculation: ScheduleRecalculationView {
            withdrawn: withdrawn.map(|_| vec![release_unit_id]).unwrap_or_default(),
            retained: Vec::new(),
        },
    })
}

fn recalculate_schedule(
    repository_id: RepositoryId,
    reason: &str,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let outcome = lock_store(store)?
        .invalidate_repository_schedule(repository_id, reason, now)
        .map_err(store_api_error)?;
    Ok(ResponseData::ScheduleRecalculated {
        recalculation: ScheduleRecalculationView {
            withdrawn: outcome.withdrawn,
            retained: outcome.retained,
        },
    })
}

fn reconcile_missed_windows(
    repository_id: RepositoryId,
    seed: u64,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let mut store = lock_store(store)?;
    let outcome = Scheduler::reconcile_missed_windows(&mut store, repository_id, seed, now)
        .map_err(scheduler_api_error)?;
    Ok(ResponseData::MissedWindowsReconciled {
        outcome: MissedWindowView {
            released_now: outcome.released_now,
            rescheduled: outcome.rescheduled,
            retained: outcome.retained,
        },
    })
}

fn set_schedule_override(
    repository_id: RepositoryId,
    override_policy: &reccursive_protocol::SchedulePolicyOverride,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let revision = lock_store(store)?
        .activate_schedule_override(repository_id, override_policy, now)
        .map_err(store_api_error)?;
    Ok(ResponseData::ScheduleOverrideActivated {
        repository_id,
        revision,
    })
}

fn list_due_units(
    concurrency_limit: usize,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let store = lock_store(store)?;
    let units = Scheduler::due_units(&store, now, concurrency_limit)
        .map_err(scheduler_api_error)?
        .into_iter()
        .map(|unit| DueUnitView {
            release_unit_id: unit.release_unit_id,
            repository_id: unit.repository_id,
            package_id: unit.package_id,
            package_revision: unit.package_revision,
            selected_at_unix_ms: unit.selected_at_unix_ms,
        })
        .collect();
    Ok(ResponseData::DueUnits { units })
}

fn pause_repository(
    repository_id: RepositoryId,
    reason: &str,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    lock_store(store)?
        .pause_repository(repository_id, reason, now)
        .map_err(store_api_error)?;
    Ok(ResponseData::RepositoryPaused {
        repository_id,
        reason: reason.to_owned(),
    })
}

fn resume_repository(
    repository_id: RepositoryId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let was_paused = lock_store(store)?
        .resume_repository(repository_id)
        .map_err(store_api_error)?;
    Ok(ResponseData::RepositoryResumed {
        repository_id,
        was_paused,
    })
}

fn release_unit_now(
    release_unit_id: reccursive_protocol::ReleaseUnitId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let now = current_unix_ms()?;
    let slot = lock_store(store)?
        .release_unit_now(release_unit_id, now)
        .map_err(store_api_error)?;
    Ok(ResponseData::ScheduleSlot {
        slot: schedule_slot_view(slot),
    })
}

fn preview_schedule(
    repository_id: RepositoryId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let store = lock_store(store)?;
    let slots = store
        .schedule_slots(repository_id)
        .map_err(store_api_error)?
        .into_iter()
        .map(schedule_slot_view)
        .collect();
    let paused = store
        .repository_pause(repository_id)
        .map_err(store_api_error)?
        .map(|pause| pause.reason);
    Ok(ResponseData::SchedulePreview { slots, paused })
}

/// Reports whether credentials and signing would currently let a repository publish.
///
/// Run before a release rather than discovered during one: the point is to answer "why can this
/// not publish" while someone is present to read the answer.
fn diagnose_repository(
    repository_id: RepositoryId,
    store: &Mutex<Store>,
) -> Result<ResponseData, ApiError> {
    let repository = lock_store(store)?
        .repository(repository_id)
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotFound,
                format!("repository {repository_id} was not found"),
                false,
            )
        })?;

    let managed = PathBuf::from(&repository.registration.managed_path);
    // Probes run against a directory that exists; before the first release the managed mirror has
    // not been created, so the user's own checkout answers the same question.
    let probe_root = if managed.is_dir() {
        managed
    } else {
        PathBuf::from(&repository.registration.checkout_path)
    };
    let credentials = crate::diagnostics::probe_credentials(
        &probe_root,
        &repository.registration.canonical_remote,
    );
    let signing =
        crate::diagnostics::probe_signing(&PathBuf::from(&repository.registration.checkout_path));
    let diagnostics = crate::diagnostics::RepositoryDiagnostics {
        credentials: credentials.clone(),
        signing: signing.clone(),
    };

    let (credential_state, credential_detail) = match &credentials {
        crate::diagnostics::CredentialStatus::Ready => ("ready", None),
        crate::diagnostics::CredentialStatus::Rejected { detail } => {
            ("rejected", Some(detail.clone()))
        }
        crate::diagnostics::CredentialStatus::Unreachable { detail } => {
            ("unreachable", Some(detail.clone()))
        }
        crate::diagnostics::CredentialStatus::TimedOut => ("timed_out", None),
    };
    let (signing_state, signing_detail) = match &signing {
        crate::diagnostics::SigningStatus::Disabled => ("disabled", None),
        crate::diagnostics::SigningStatus::Ready { format } => ("ready", Some(format.clone())),
        crate::diagnostics::SigningStatus::Unavailable { format, detail } => {
            ("unavailable", Some(format!("{format}: {detail}")))
        }
    };

    Ok(ResponseData::RepositoryDiagnostics {
        repository_id,
        credentials: credential_state.to_owned(),
        credential_detail,
        signing: signing_state.to_owned(),
        signing_detail,
        can_publish: diagnostics.can_publish(),
    })
}

fn list_integration_health(store: &Mutex<Store>) -> Result<ResponseData, ApiError> {
    let integrations = lock_store(store)?
        .failing_integrations()
        .map_err(store_api_error)?
        .into_iter()
        .map(|health| IntegrationHealthView {
            integration: health.integration,
            scope: health.scope,
            consecutive_failures: health.consecutive_failures,
            fault: health.fault,
            detail: health.detail,
            next_attempt_at_unix_ms: health.next_attempt_at_unix_ms,
        })
        .collect();
    Ok(ResponseData::IntegrationHealth { integrations })
}

fn schedule_slot_view(slot: reccursive_store::ScheduleSlot) -> ScheduleSlotView {
    ScheduleSlotView {
        release_unit_id: slot.release_unit_id,
        repository_id: slot.repository_id,
        package_id: slot.package_id,
        package_revision: slot.package_revision,
        policy_revision: slot.policy_revision,
        timezone: slot.timezone.as_str().to_owned(),
        eligible_at_unix_ms: slot.eligible_at_unix_ms,
        selected_at_unix_ms: slot.selected_at_unix_ms,
        created_at_unix_ms: slot.created_at_unix_ms,
    }
}

fn scheduler_api_error(error: SchedulerError) -> ApiError {
    match error {
        SchedulerError::Store(error) => store_api_error(error),
        SchedulerError::Policy(error) => {
            ApiError::new(ApiErrorCode::InvalidRequest, error.to_string(), false)
        }
        SchedulerError::MissingReleaseUnit(id) => ApiError::new(
            ApiErrorCode::NotFound,
            format!("release unit {id} was not found"),
            false,
        ),
        SchedulerError::MissingPlan => ApiError::new(
            ApiErrorCode::NotFound,
            "release unit plan was not found",
            false,
        ),
        SchedulerError::MissingPolicy(repository_id) => ApiError::new(
            ApiErrorCode::InvalidRequest,
            format!("repository {repository_id} has no active schedule policy"),
            false,
        ),
        SchedulerError::NoSelectableSlot => ApiError::new(
            ApiErrorCode::InvalidRequest,
            "the active schedule policy selected no future release slot",
            false,
        ),
    }
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
        // The store's conflict messages name the actual thing that conflicted — a task, an
        // attempt, a lease. Replacing all of them with one sentence about repositories and
        // policies made every conflict report a problem the caller did not have.
        StoreError::Conflict(message) => ApiError::new(ApiErrorCode::Conflict, message, false),
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
