use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use clap::{Parser, Subcommand, ValueEnum, error::ErrorKind};
use install::{InstallError, ServiceInstallation};
use reccursive_protocol::{
    ApiError, ApiErrorCode, CancelTaskRequest, CapturePackageRequest, Command,
    CreateReleaseUnitRequest, CreateWorkspaceRequest, DueUnitView, EnrollRepositoryRequest,
    EventSeverityView, EventView, FeatureId, FeaturePlan, IdempotencyKey, IntegrationHealthView,
    LocalClient, PackageId, PackageView, PlanView, PublicationMode, QueueAuditView,
    QueueExportView, ReleaseAttemptView, ReleasePackageRequest, ReleaseUnitId, ReleaseUnitView,
    RepositoryId, RepositoryView, ResponseData, Revision, SchedulePolicy, SchedulePolicyOverride,
    SchedulePolicyView, ScheduleSlotView, ScheduleUnitRequest, SetSchedulePolicyRequest,
    SubmissionView, SubmitTaskRequest, TargetRef, TaskId, TaskProgressView, WorkspaceView,
};
use serde::Serialize;
use serde_json::json;

mod install;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_INTERNAL: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_ACTION_REQUIRED: u8 = 10;
pub const EXIT_UNAVAILABLE: u8 = 11;
pub const EXIT_CONFLICT: u8 = 12;
pub const EXIT_INCOMPATIBLE: u8 = 13;

#[derive(Debug, Parser)]
#[command(
    name = "reccursive",
    version,
    about = "Schedule verified software changes safely",
    after_help = "Start here:\n  reccursive doctor\n  reccursive repository add /path/to/repository\n  reccursive status\n\nFor stable automation output, pass --json. For an agent workflow, see docs/AGENT_HANDOFF.md."
)]
struct Cli {
    /// Override the local service state directory.
    #[arg(long, global = true, value_name = "PATH")]
    state_dir: Option<PathBuf>,

    /// Emit stable JSON without interactive formatting.
    #[arg(long, global = true)]
    json: bool,

    /// Name this invocation's intent so a repeat of it returns the first result instead of
    /// acting again. Intended for agents, which retry after a dropped connection.
    #[arg(long, global = true, value_name = "KEY", value_parser = parse_idempotency_key)]
    idempotency_key: Option<IdempotencyKey>,

    #[command(subcommand)]
    command: TopLevelCommand,
}

#[derive(Debug, Subcommand)]
enum TopLevelCommand {
    /// Register and inspect repositories.
    Repository {
        #[command(subcommand)]
        command: RepositoryCommand,
    },
    /// Import and inspect versioned feature plans.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Create and inspect daemon-owned build workspaces.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Capture and inspect immutable snapshot packages.
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    /// Submit completed task work, or cancel a task that has not reached publication.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Report every task of a feature revision and the durable work attached to it.
    Feature {
        #[command(subcommand)]
        command: FeatureCommand,
    },
    /// Publish a verified package or inspect durable publication attempts.
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// Configure repository release timing and select durable release times.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// Inspect package recovery state or create a portable queue backup.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Install, remove, and inspect the background service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Show service and queue summary.
    Status,
    /// Check local prerequisites and service connectivity.
    Doctor,
    /// Show integrations that are currently failing and when each may be retried.
    Integrations,
    /// Check whether credentials and signing would let a repository publish right now.
    Diagnose { repository_id: RepositoryId },
    /// Show recent sanitized daemon events.
    Logs {
        /// Maximum number of newest events to return.
        #[arg(long, default_value_t = 50, value_parser = parse_event_limit)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum PackageCommand {
    /// Capture current owned-workspace changes for one or more plan tasks.
    Capture {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Revision,
        #[arg(long = "task", required = true)]
        task_ids: Vec<TaskId>,
    },
    /// Verify and show an immutable snapshot package.
    Show {
        package_id: PackageId,
        #[arg(long, value_parser = parse_revision, default_value = "1")]
        revision: Revision,
    },
}

#[derive(Debug, Subcommand)]
enum FeatureCommand {
    /// Show each task's durable state and the package, unit, and release time attached to it.
    Status {
        feature_id: FeatureId,
        /// Plan revision; defaults to the newest stored revision.
        #[arg(long, value_parser = parse_revision)]
        revision: Option<Revision>,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Capture, group, and schedule one task's completed work in a single step.
    Submit {
        feature_id: FeatureId,
        /// Plan revision; defaults to the newest stored revision.
        #[arg(long, value_parser = parse_revision)]
        revision: Option<Revision>,
        #[arg(long = "task", required = true)]
        task_ids: Vec<TaskId>,
        /// Seed for the deterministic release-time selection.
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
    /// Cancel a task and block any plan tasks that depend on it.
    Cancel {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Revision,
        #[arg(long = "task")]
        task_id: TaskId,
        #[arg(long)]
        message: String,
    },
}

#[derive(Debug, Subcommand)]
enum ReleaseCommand {
    /// Reconcile, validate, and publish one immutable package.
    Publish {
        package_id: PackageId,
        #[arg(long, value_parser = parse_revision, default_value = "1")]
        revision: Revision,
        #[arg(long)]
        message: String,
        #[arg(long)]
        author_name: String,
        #[arg(long)]
        author_email: String,
    },
    /// Show one durable publication attempt.
    Attempt {
        attempt_id: reccursive_protocol::AttemptId,
    },
    /// List recent publication attempts.
    Attempts {
        #[arg(long)]
        package_id: Option<PackageId>,
        #[arg(long, default_value_t = 50, value_parser = parse_event_limit)]
        limit: usize,
    },
    /// Group tasks that cannot independently leave the target usable into one release unit.
    CreateUnit {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Revision,
        #[arg(long = "task", required = true)]
        task_ids: Vec<TaskId>,
    },
    /// Show one durable release unit.
    ShowUnit { release_unit_id: ReleaseUnitId },
}

#[derive(Debug, Subcommand)]
enum ScheduleCommand {
    /// Activate a validated scheduling-policy revision for a repository.
    SetPolicy {
        repository_id: RepositoryId,
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Show a repository's active scheduling policy.
    ShowPolicy { repository_id: RepositoryId },
    /// Select a durable future release time for one captured release unit.
    Unit {
        release_unit_id: ReleaseUnitId,
        #[arg(long)]
        package_id: PackageId,
        #[arg(long, value_parser = parse_revision, default_value = "1")]
        revision: Revision,
        /// Deterministic selection seed; a random one is used when omitted.
        #[arg(long)]
        seed: Option<u64>,
    },
    /// Show the durable slot previously selected for a release unit.
    Show { release_unit_id: ReleaseUnitId },
    /// Withdraw one unit's release time so it returns to the queue for a fresh selection.
    Withdraw {
        release_unit_id: ReleaseUnitId,
        #[arg(long)]
        reason: String,
    },
    /// Withdraw every live selection for a repository after changing its policy.
    Recalculate {
        repository_id: RepositoryId,
        #[arg(long)]
        reason: String,
    },
    /// Apply the repository's missed-window policy to release times that have already passed.
    CatchUp {
        repository_id: RepositoryId,
        /// Deterministic selection seed; a random one is used when omitted.
        #[arg(long)]
        seed: Option<u64>,
    },
    /// Refine one repository's scheduling without changing the policy it shares.
    SetOverride {
        repository_id: RepositoryId,
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// List units due for release now, fairly across repositories and within a global limit.
    Due {
        #[arg(long, default_value_t = 10, value_parser = parse_event_limit)]
        concurrency_limit: usize,
    },
    /// Stop a repository from starting new releases until it is resumed.
    Pause {
        repository_id: RepositoryId,
        #[arg(long)]
        reason: String,
    },
    /// Resume a paused repository.
    Resume { repository_id: RepositoryId },
    /// Move one unit's release time to now, subject to the same eligibility rules.
    ReleaseNow { release_unit_id: ReleaseUnitId },
    /// Show a repository's upcoming release times without changing any of them.
    Preview { repository_id: RepositoryId },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Install the background service so it runs after the terminal is closed.
    Install {
        /// The daemon binary to run; defaults to reccursive-daemon beside this executable.
        #[arg(long, value_name = "PATH")]
        program: Option<PathBuf>,
    },
    /// Stop the service and remove its definition, keeping queued work on disk.
    Uninstall,
    /// Show whether the service is installed and loaded.
    Status,
    /// Print the service definition without installing it.
    Show {
        #[arg(long, value_name = "PATH")]
        program: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// Verify package storage and show unresolved crash-recovery evidence.
    Audit,
    /// Export verified queue state and immutable packages to a new directory.
    Export {
        #[arg(value_name = "DIRECTORY")]
        destination: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Create an isolated workspace for a sealed plan revision.
    Create {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Option<Revision>,
        /// Explicit dirty file to copy as a prerequisite; may be repeated.
        #[arg(long = "include", value_name = "PATH")]
        prerequisites: Vec<String>,
    },
    /// Show the recorded workspace for an exact plan revision.
    Show {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Revision,
    },
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    /// Validate and append a JSON plan revision.
    Import {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Show one plan revision, or the latest revision by default.
    Show {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Option<Revision>,
    },
    /// List all stored revisions for a feature.
    History { feature_id: FeatureId },
    /// Fix a feature's scope by appending a sealed copy of its newest revision.
    Seal { feature_id: FeatureId },
}

#[derive(Debug, Subcommand)]
enum RepositoryCommand {
    /// Register a repository with its first publication policy.
    Add {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Canonical remote URL; defaults to origin.
        #[arg(long)]
        remote: Option<String>,
        /// Target branch name or full refs/heads ref.
        #[arg(long, default_value = "main")]
        target: String,
        /// Publication behavior.
        #[arg(long, value_enum, default_value = "scheduled")]
        mode: PublicationModeArgument,
        /// Development branch required by immediate mode.
        #[arg(long)]
        development_target: Option<String>,
    },
    /// List registered repositories.
    List,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PublicationModeArgument {
    Scheduled,
    Immediate,
}

/// One CLI invocation's connection to the local service.
#[derive(Debug)]
struct Session {
    state_dir: PathBuf,
    socket: PathBuf,
    auth_token: PathBuf,
    /// Set when the caller named this invocation's intent, which makes a repeat of it return the
    /// first result rather than act a second time. Absent by default: a person typing a command
    /// has no dropped connection to recover from, and would only be naming keys for the sake of it.
    idempotency_key: Option<IdempotencyKey>,
}

impl Session {
    fn new(state_dir: PathBuf, idempotency_key: Option<IdempotencyKey>) -> Self {
        Self {
            socket: state_dir.join("service.sock"),
            auth_token: state_dir.join("auth.token"),
            state_dir,
            idempotency_key,
        }
    }
}

#[derive(Debug)]
struct CliFailure {
    exit_code: u8,
    code: &'static str,
    message: String,
    reported: bool,
}

impl CliFailure {
    fn new(exit: u8, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit_code: exit,
            code,
            message: message.into(),
            reported: false,
        }
    }

    fn already_reported(mut self) -> Self {
        self.reported = true;
        self
    }
}

#[derive(Serialize)]
struct DoctorReport {
    healthy: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: &'static str,
    detail: String,
}

/// Parses process arguments, writes output, and returns a stable process exit code.
pub fn run_from_env() -> u8 {
    run(
        env::args_os(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

pub fn run<I, T>(arguments: I, stdout: &mut impl Write, stderr: &mut impl Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    let json_requested = arguments
        .iter()
        .any(|argument| argument.to_string_lossy() == "--json");
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            return report_parse_error(error, json_requested, stdout, stderr);
        }
    };
    let json_output = cli.json;
    let result = execute(cli, stdout);
    match result {
        Ok(()) => EXIT_SUCCESS,
        Err(failure) => {
            if failure.reported {
                // This command already emitted its structured multi-check report.
            } else if json_output {
                let value = json!({
                    "ok": false,
                    "error": { "code": failure.code, "message": failure.message }
                });
                let _ = writeln!(stderr, "{value}");
            } else {
                let _ = writeln!(stderr, "error [{}]: {}", failure.code, failure.message);
            }
            failure.exit_code
        }
    }
}

fn report_parse_error(
    error: clap::Error,
    json_requested: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = write!(stdout, "{error}");
            EXIT_SUCCESS
        }
        _ if json_requested => {
            let value = json!({
                "ok": false,
                "error": {
                    "code": "usage",
                    "message": "Invalid command or arguments. Run `reccursive --help` for usage."
                }
            });
            let _ = writeln!(stderr, "{value}");
            EXIT_USAGE
        }
        _ => {
            let _ = write!(stderr, "{error}");
            EXIT_USAGE
        }
    }
}

fn execute(cli: Cli, stdout: &mut impl Write) -> Result<(), CliFailure> {
    let state_dir = resolve_state_dir(cli.state_dir)?;
    let session = Session::new(state_dir, cli.idempotency_key);
    match cli.command {
        TopLevelCommand::Doctor => doctor(&session, cli.json, stdout),
        TopLevelCommand::Diagnose { repository_id } => {
            let data = send(&session, Command::DiagnoseRepository { repository_id })?;
            output_data(data, cli.json, stdout)
        }
        TopLevelCommand::Integrations => {
            let data = send(&session, Command::ListIntegrationHealth)?;
            output_data(data, cli.json, stdout)
        }
        TopLevelCommand::Service { command } => {
            service_command(command, &session, cli.json, stdout)
        }
        TopLevelCommand::Status => {
            let data = send(&session, Command::Status)?;
            output_data(data, cli.json, stdout)
        }
        TopLevelCommand::Logs { limit } => {
            let data = send(&session, Command::ListEvents { limit })?;
            output_data(data, cli.json, stdout)
        }
        TopLevelCommand::Plan { command } => match command {
            PlanCommand::Import { file } => {
                let plan = load_plan(&file)?;
                let data = send(&session, Command::ImportPlan { plan })?;
                output_data(data, cli.json, stdout)
            }
            PlanCommand::Show {
                feature_id,
                revision,
            } => {
                let data = send(
                    &session,
                    Command::GetPlan {
                        feature_id,
                        revision,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            PlanCommand::History { feature_id } => {
                let data = send(&session, Command::PlanHistory { feature_id })?;
                output_data(data, cli.json, stdout)
            }
            PlanCommand::Seal { feature_id } => {
                let data = send(&session, Command::SealPlan { feature_id })?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Feature { command } => match command {
            FeatureCommand::Status {
                feature_id,
                revision,
            } => {
                let data = send(
                    &session,
                    Command::GetFeatureStatus {
                        feature_id,
                        revision,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Workspace { command } => match command {
            WorkspaceCommand::Create {
                feature_id,
                revision,
                prerequisites,
            } => {
                let data = send(
                    &session,
                    Command::CreateWorkspace(CreateWorkspaceRequest {
                        feature_id,
                        revision,
                        prerequisites,
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            WorkspaceCommand::Show {
                feature_id,
                revision,
            } => {
                let data = send(
                    &session,
                    Command::GetWorkspace {
                        feature_id,
                        revision,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Package { command } => match command {
            PackageCommand::Capture {
                feature_id,
                revision,
                task_ids,
            } => {
                let data = send(
                    &session,
                    Command::CapturePackage(CapturePackageRequest {
                        feature_id,
                        plan_revision: revision,
                        task_ids: task_ids.into_iter().collect(),
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            PackageCommand::Show {
                package_id,
                revision,
            } => {
                let data = send(
                    &session,
                    Command::GetPackage {
                        package_id,
                        revision,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Task { command } => match command {
            TaskCommand::Submit {
                feature_id,
                revision,
                task_ids,
                seed,
            } => {
                let data = send(
                    &session,
                    Command::SubmitTask(SubmitTaskRequest {
                        feature_id,
                        plan_revision: revision,
                        task_ids: task_ids.into_iter().collect(),
                        seed,
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            TaskCommand::Cancel {
                feature_id,
                revision,
                task_id,
                message,
            } => {
                let data = send(
                    &session,
                    Command::CancelTask(CancelTaskRequest {
                        feature_id,
                        plan_revision: revision,
                        task_id,
                        message,
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Release { command } => match command {
            ReleaseCommand::Publish {
                package_id,
                revision,
                message,
                author_name,
                author_email,
            } => {
                let data = send(
                    &session,
                    Command::ReleasePackage(ReleasePackageRequest {
                        package_id,
                        revision,
                        message,
                        author_name,
                        author_email,
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            ReleaseCommand::Attempt { attempt_id } => {
                let data = send(&session, Command::GetReleaseAttempt { attempt_id })?;
                output_data(data, cli.json, stdout)
            }
            ReleaseCommand::Attempts { package_id, limit } => {
                let data = send(&session, Command::ListReleaseAttempts { package_id, limit })?;
                output_data(data, cli.json, stdout)
            }
            ReleaseCommand::CreateUnit {
                feature_id,
                revision,
                task_ids,
            } => {
                let data = send(
                    &session,
                    Command::CreateReleaseUnit(CreateReleaseUnitRequest {
                        release_unit_id: ReleaseUnitId::new(),
                        feature_id,
                        plan_revision: revision,
                        task_ids: task_ids.into_iter().collect(),
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            ReleaseCommand::ShowUnit { release_unit_id } => {
                let data = send(&session, Command::GetReleaseUnit { release_unit_id })?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Schedule { command } => match command {
            ScheduleCommand::SetPolicy {
                repository_id,
                file,
            } => {
                let policy = load_schedule_policy(&file)?;
                let data = send(
                    &session,
                    Command::SetSchedulePolicy(SetSchedulePolicyRequest {
                        repository_id,
                        policy,
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::ShowPolicy { repository_id } => {
                let data = send(&session, Command::GetSchedulePolicy { repository_id })?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Unit {
                release_unit_id,
                package_id,
                revision,
                seed,
            } => {
                let seed = seed.unwrap_or_else(random_seed);
                let data = send(
                    &session,
                    Command::ScheduleUnit(ScheduleUnitRequest {
                        release_unit_id,
                        package_id,
                        package_revision: revision,
                        seed,
                    }),
                )?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Show { release_unit_id } => {
                let data = send(&session, Command::GetScheduleSlot { release_unit_id })?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Withdraw {
                release_unit_id,
                reason,
            } => {
                let data = send(
                    &session,
                    Command::WithdrawScheduleSlot {
                        release_unit_id,
                        reason,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Pause {
                repository_id,
                reason,
            } => {
                let data = send(
                    &session,
                    Command::PauseRepository {
                        repository_id,
                        reason,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Resume { repository_id } => {
                let data = send(&session, Command::ResumeRepository { repository_id })?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::ReleaseNow { release_unit_id } => {
                let data = send(&session, Command::ReleaseUnitNow { release_unit_id })?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Preview { repository_id } => {
                let data = send(&session, Command::PreviewSchedule { repository_id })?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::SetOverride {
                repository_id,
                file,
            } => {
                let override_policy = load_schedule_override(&file)?;
                let data = send(
                    &session,
                    Command::SetScheduleOverride {
                        repository_id,
                        override_policy,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Due { concurrency_limit } => {
                let data = send(&session, Command::ListDueUnits { concurrency_limit })?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::CatchUp {
                repository_id,
                seed,
            } => {
                let seed = seed.unwrap_or_else(random_seed);
                let data = send(
                    &session,
                    Command::ReconcileMissedWindows {
                        repository_id,
                        seed,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            ScheduleCommand::Recalculate {
                repository_id,
                reason,
            } => {
                let data = send(
                    &session,
                    Command::RecalculateSchedule {
                        repository_id,
                        reason,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Queue { command } => match command {
            QueueCommand::Audit => {
                let data = send(&session, Command::AuditQueue)?;
                output_data(data, cli.json, stdout)
            }
            QueueCommand::Export { destination } => {
                let data = send(
                    &session,
                    Command::ExportQueue {
                        destination: destination.to_string_lossy().into_owned(),
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Repository { command } => match command {
            RepositoryCommand::List => {
                let data = send(&session, Command::ListRepositories)?;
                output_data(data, cli.json, stdout)
            }
            RepositoryCommand::Add {
                path,
                remote,
                target,
                mode,
                development_target,
            } => {
                let request = enrollment_request(path, remote, target, mode, development_target)?;
                let data = send(&session, Command::EnrollRepository(request))?;
                output_data(data, cli.json, stdout)
            }
        },
    }
}

fn load_plan(path: &Path) -> Result<FeaturePlan, CliFailure> {
    let bytes = fs::read(path).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "plan_unreadable",
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    let plan: FeaturePlan = serde_json::from_slice(&bytes).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_plan",
            format!("{} is not a valid plan document: {error}", path.display()),
        )
    })?;
    plan.validate().map_err(|error| {
        CliFailure::new(EXIT_ACTION_REQUIRED, "invalid_plan", error.to_string())
    })?;
    Ok(plan)
}

/// Runs one `service` subcommand.
///
/// Service management is the one part of the CLI that does not go through the daemon: it is what
/// arranges for the daemon to exist in the first place, so it must work when nothing is running.
fn service_command(
    command: ServiceCommand,
    session: &Session,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "home_unknown",
            "HOME is not set, so the user's LaunchAgents directory cannot be located",
        )
    })?;
    let resolve = |program: Option<PathBuf>| -> Result<PathBuf, CliFailure> {
        match program {
            Some(path) => Ok(path),
            None => default_daemon_program(),
        }
    };

    match command {
        ServiceCommand::Show { program } => {
            let installation =
                ServiceInstallation::describe(resolve(program)?, &session.state_dir, &home);
            write!(out, "{}", installation.plist()).map_err(output_error)
        }
        ServiceCommand::Install { program } => {
            let installation =
                ServiceInstallation::describe(resolve(program)?, &session.state_dir, &home);
            installation.install().map_err(install_failure)?;
            report_service(out, json_output, &installation, true, "installed")
        }
        ServiceCommand::Uninstall => {
            let installation =
                ServiceInstallation::describe(default_daemon_program()?, &session.state_dir, &home);
            installation.uninstall().map_err(install_failure)?;
            report_service(out, json_output, &installation, false, "removed")
        }
        ServiceCommand::Status => {
            let installation =
                ServiceInstallation::describe(default_daemon_program()?, &session.state_dir, &home);
            let loaded = installation.is_loaded();
            report_service(
                out,
                json_output,
                &installation,
                loaded,
                if loaded { "running" } else { "not running" },
            )
        }
    }
}

fn report_service(
    out: &mut impl Write,
    json_output: bool,
    installation: &ServiceInstallation,
    loaded: bool,
    state: &str,
) -> Result<(), CliFailure> {
    if json_output {
        let value = json!({
            "ok": true,
            "data": {
                "type": "service",
                "payload": {
                    "label": installation.label,
                    "state": state,
                    "loaded": loaded,
                    "definition": installation.plist_path.to_string_lossy(),
                    "state_dir": installation.state_dir.to_string_lossy(),
                    "log": installation.log_path.to_string_lossy(),
                }
            }
        });
        return writeln!(out, "{value}").map_err(output_error);
    }
    writeln!(
        out,
        "Service {} · {}\nDefinition: {}\nLog: {}",
        installation.label,
        state,
        installation.plist_path.display(),
        installation.log_path.display()
    )
    .map_err(output_error)
}

/// Finds the daemon binary shipped alongside this CLI.
fn default_daemon_program() -> Result<PathBuf, CliFailure> {
    let executable = std::env::current_exe().map_err(|error| {
        CliFailure::new(
            EXIT_INTERNAL,
            "program_unknown",
            format!("cannot locate this executable: {error}"),
        )
    })?;
    let candidate = executable
        .parent()
        .map(|directory| directory.join("reccursive-daemon"))
        .ok_or_else(|| {
            CliFailure::new(
                EXIT_INTERNAL,
                "program_unknown",
                "this executable has no parent directory",
            )
        })?;
    if !candidate.exists() {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "daemon_not_found",
            format!(
                "no daemon binary at {}; pass --program to name one",
                candidate.display()
            ),
        ));
    }
    Ok(candidate)
}

fn install_failure(error: InstallError) -> CliFailure {
    let code = match error {
        InstallError::MissingProgram(_) => "daemon_not_found",
        InstallError::Io(_) => "service_unwritable",
        InstallError::LaunchctlUnavailable(_) | InstallError::Launchctl { .. } => {
            "service_manager_failed"
        }
    };
    CliFailure::new(EXIT_ACTION_REQUIRED, code, error.to_string())
}

fn load_schedule_override(path: &Path) -> Result<SchedulePolicyOverride, CliFailure> {
    let bytes = fs::read(path).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "schedule_override_unreadable",
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_schedule_override",
            format!(
                "{} is not a valid schedule override: {error}",
                path.display()
            ),
        )
    })
}

fn load_schedule_policy(path: &Path) -> Result<SchedulePolicy, CliFailure> {
    let bytes = fs::read(path).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "schedule_policy_unreadable",
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    // SchedulePolicy validates its own fields during deserialization, so a structurally invalid
    // document and a semantically invalid one (overlapping windows, an empty day set, and so on)
    // are both reported here with no separate validation pass required.
    serde_json::from_slice(&bytes).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_schedule_policy",
            format!("{} is not a valid schedule policy: {error}", path.display()),
        )
    })
}

/// A seed for deterministic slot selection. Real usage does not care which slot among the
/// eligible ones is picked, only that a restart never redraws it — which the store guarantees
/// independently of this value. Reused rather than adding a `rand` dependency for one call.
fn random_seed() -> u64 {
    uuid::Uuid::new_v4().as_u64_pair().0
}

fn parse_revision(value: &str) -> Result<Revision, String> {
    let value = value
        .parse::<u32>()
        .map_err(|_| "revision must be a positive integer".to_owned())?;
    Revision::new(value).map_err(|error| error.to_string())
}

fn parse_idempotency_key(value: &str) -> Result<IdempotencyKey, String> {
    IdempotencyKey::new(value).map_err(|error| error.to_string())
}

fn parse_event_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer between 1 and 1000".to_owned())?;
    if (1..=1_000).contains(&limit) {
        Ok(limit)
    } else {
        Err("limit must be between 1 and 1000".to_owned())
    }
}

fn send(session: &Session, command: Command) -> Result<ResponseData, CliFailure> {
    let client = LocalClient::from_token_file(&session.auth_token).map_err(|error| {
        CliFailure::new(
            EXIT_UNAVAILABLE,
            "service_unavailable",
            format!("cannot read local service credentials: {error}"),
        )
    })?;
    let response = client
        .send_with_key(&session.socket, command, session.idempotency_key.clone())
        .map_err(|error| {
            CliFailure::new(
                EXIT_UNAVAILABLE,
                "service_unavailable",
                format!("cannot reach local service: {error}"),
            )
        })?;
    response.result.map_err(api_failure)
}

fn api_failure(error: ApiError) -> CliFailure {
    let (exit, code) = match error.code {
        ApiErrorCode::InvalidRequest => (EXIT_ACTION_REQUIRED, "invalid_request"),
        ApiErrorCode::NotFound => (EXIT_ACTION_REQUIRED, "not_found"),
        ApiErrorCode::UnsupportedVersion | ApiErrorCode::Unauthorized => {
            (EXIT_INCOMPATIBLE, "incompatible_service")
        }
        ApiErrorCode::Conflict => (EXIT_CONFLICT, "conflict"),
        ApiErrorCode::TemporarilyUnavailable => (EXIT_UNAVAILABLE, "temporarily_unavailable"),
        ApiErrorCode::Internal => (EXIT_INTERNAL, "internal"),
    };
    CliFailure::new(exit, code, error.message)
}

fn enrollment_request(
    path: PathBuf,
    remote: Option<String>,
    target: String,
    mode: PublicationModeArgument,
    development_target: Option<String>,
) -> Result<EnrollRepositoryRequest, CliFailure> {
    let requested_path = fs::canonicalize(&path).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_repository",
            format!("cannot access {}: {error}", path.display()),
        )
    })?;
    let checkout = git_repository_root(&requested_path)?;
    let canonical_remote = match remote {
        Some(remote) => remote,
        None => git_stdout(&checkout, &["remote", "get-url", "origin"]).map_err(|_| {
            CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "remote_missing",
                "origin has no readable URL; pass --remote explicitly",
            )
        })?,
    };
    let publication_mode = match mode {
        PublicationModeArgument::Scheduled => PublicationMode::ScheduledCreation,
        PublicationModeArgument::Immediate => PublicationMode::ImmediateAvailability,
    };
    let request = EnrollRepositoryRequest {
        checkout_path: checkout.to_string_lossy().into_owned(),
        canonical_remote,
        publication_mode,
        target: branch_ref(&target)?,
        development_target: development_target.as_deref().map(branch_ref).transpose()?,
    };
    request.validate().map_err(|error| {
        CliFailure::new(EXIT_ACTION_REQUIRED, "invalid_policy", error.to_string())
    })?;
    Ok(request)
}

fn git_repository_root(path: &Path) -> Result<PathBuf, CliFailure> {
    let root = git_stdout(path, &["rev-parse", "--show-toplevel"]).map_err(|_| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_repository",
            format!("{} is not inside a Git worktree", path.display()),
        )
    })?;
    git_stdout(path, &["rev-parse", "--verify", "HEAD"]).map_err(|_| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "empty_repository",
            "repository must have at least one commit before enrollment",
        )
    })?;
    fs::canonicalize(&root).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_repository",
            format!("cannot resolve repository root {root}: {error}"),
        )
    })
}

fn git_stdout(path: &Path, arguments: &[&str]) -> Result<String, io::Error> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("Git command failed"));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(io::Error::other)
}

fn branch_ref(value: &str) -> Result<TargetRef, CliFailure> {
    let value = if value.starts_with("refs/heads/") {
        value.to_owned()
    } else {
        format!("refs/heads/{value}")
    };
    let valid = ProcessCommand::new("git")
        .args(["check-ref-format", &value])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !valid {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_branch",
            format!("{value:?} is not a valid Git branch ref"),
        ));
    }
    TargetRef::new(value)
        .map_err(|error| CliFailure::new(EXIT_ACTION_REQUIRED, "invalid_branch", error.to_string()))
}

fn output_feature_status(
    out: &mut impl Write,
    status: &reccursive_protocol::FeatureStatusView,
) -> Result<(), CliFailure> {
    writeln!(
        out,
        "{} revision {} · {} · {}",
        status.feature_id,
        status.plan_revision.get(),
        if status.sealed { "sealed" } else { "draft" },
        status.goal
    )
    .map_err(output_error)?;
    if status.tasks.is_empty() {
        return writeln!(out, "  (no tasks)").map_err(output_error);
    }
    for task in &status.tasks {
        output_task_progress(out, task)?;
    }
    Ok(())
}

fn output_task_progress(out: &mut impl Write, task: &TaskProgressView) -> Result<(), CliFailure> {
    // The status is printed verbatim rather than collapsed into ready/not-ready, because
    // "blocked" and "cancelled" are answers a caller has to act on differently from "not yet".
    writeln!(
        out,
        "  {} · {:?} · {}",
        task.task_id, task.status, task.name
    )
    .map_err(output_error)?;
    if let Some(blocked_from) = task.blocked_from {
        writeln!(out, "      blocked out of: {blocked_from:?}").map_err(output_error)?;
    }
    if let Some(package_id) = task.package_id {
        writeln!(out, "      package: {package_id}").map_err(output_error)?;
    }
    if let Some(unit_id) = task.release_unit_id {
        writeln!(out, "      unit:    {unit_id}").map_err(output_error)?;
    }
    if let Some(selected) = task.selected_at_unix_ms {
        writeln!(out, "      due:     {selected} (unix ms)").map_err(output_error)?;
    }
    Ok(())
}

fn output_submission(out: &mut impl Write, submission: &SubmissionView) -> Result<(), CliFailure> {
    writeln!(
        out,
        "{} {}\n  package: {}\n  unit:    {}\n  due:     {} (unix ms)",
        if submission.created {
            "Submitted"
        } else {
            "Already submitted"
        },
        submission.unit.unit_id,
        submission.package.package_id,
        submission.unit.unit_id,
        submission.slot.selected_at_unix_ms
    )
    .map_err(output_error)
}

fn output_data(
    data: ResponseData,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    if json_output {
        let value = json!({ "ok": true, "data": data });
        writeln!(out, "{value}").map_err(output_error)?;
        return Ok(());
    }
    match data {
        ResponseData::Pong {
            service_version,
            schema_version,
        } => writeln!(out, "Service {service_version}, schema {schema_version}"),
        ResponseData::Status {
            service_version,
            schema_version,
            repository_count,
        } => writeln!(
            out,
            "Service {service_version} · schema {schema_version} · {repository_count} repositories"
        ),
        ResponseData::RepositoryEnrolled { repository } => {
            writeln!(
                out,
                "Enrolled {}\nTarget: {}",
                repository.id,
                repository.target.as_str()
            )
        }
        ResponseData::Repositories { repositories } => output_repositories(out, &repositories),
        ResponseData::Events { events } => output_events(out, &events),
        ResponseData::PlanSealed { plan } => {
            writeln!(
                out,
                "Sealed {} as revision {}",
                plan.plan.feature_id,
                plan.plan.revision.get()
            )
        }
        ResponseData::FeatureStatus { status } => {
            return output_feature_status(out, &status);
        }
        ResponseData::TaskSubmitted { submission } => {
            return output_submission(out, &submission);
        }
        ResponseData::PlanImported { plan } => {
            writeln!(
                out,
                "Imported {} revision {}",
                plan.plan.feature_id,
                plan.plan.revision.get()
            )
        }
        ResponseData::Plan { plan } => output_plan(out, &plan),
        ResponseData::PlanHistory { plans } => output_plan_history(out, &plans),
        ResponseData::WorkspaceCreated { workspace } => {
            writeln!(out, "Created owned workspace {}", workspace.path)
        }
        ResponseData::Workspace { workspace } => output_workspace(out, &workspace),
        ResponseData::PackageCaptured { package } => {
            writeln!(out, "Captured immutable package {}", package.package_id)
        }
        ResponseData::Package { package } => output_package(out, &package),
        ResponseData::TaskCancelled { cancellation } => writeln!(
            out,
            "Cancelled {} and blocked {} dependent task(s)",
            cancellation.task_id,
            cancellation.blocked_dependents.len()
        ),
        ResponseData::ReleaseAttempt { attempt } => output_release_attempt(out, &attempt),
        ResponseData::ReleaseAttempts { attempts } => output_release_attempts(out, &attempts),
        ResponseData::ReleaseUnitCreated { unit } => {
            writeln!(
                out,
                "Created release unit {} with {} task(s)",
                unit.unit_id,
                unit.task_ids.len()
            )
        }
        ResponseData::ReleaseUnit { unit } => output_release_unit(out, &unit),
        ResponseData::SchedulePolicyActivated { policy } => writeln!(
            out,
            "Activated schedule policy revision {} for repository {}",
            policy.revision.get(),
            policy.repository_id
        ),
        ResponseData::SchedulePolicy { policy } => output_schedule_policy(out, &policy),
        ResponseData::ScheduleSlot { slot } => output_schedule_slot(out, &slot),
        ResponseData::ScheduleOverrideActivated {
            repository_id,
            revision,
        } => writeln!(
            out,
            "Activated schedule override revision {} for repository {repository_id}",
            revision.get()
        ),
        ResponseData::DueUnits { units } => output_due_units(out, &units),
        ResponseData::IntegrationHealth { integrations } => {
            output_integration_health(out, &integrations)
        }
        ResponseData::RepositoryDiagnostics {
            repository_id,
            credentials,
            credential_detail,
            signing,
            signing_detail,
            can_publish,
        } => writeln!(
            out,
            "Repository {repository_id}\nCredentials: {credentials}{}\nSigning: {signing}{}\n\
             Can publish now: {}",
            credential_detail.map_or_else(String::new, |detail| format!(" ({detail})")),
            signing_detail.map_or_else(String::new, |detail| format!(" ({detail})")),
            if can_publish { "yes" } else { "no" }
        ),
        ResponseData::RepositoryPaused {
            repository_id,
            reason,
        } => writeln!(out, "Paused {repository_id}: {reason}"),
        ResponseData::RepositoryResumed {
            repository_id,
            was_paused,
        } => writeln!(
            out,
            "{repository_id} is running{}",
            if was_paused {
                " again"
            } else {
                " (was not paused)"
            }
        ),
        ResponseData::SchedulePreview { slots, paused } => {
            output_schedule_preview(out, &slots, paused.as_deref())
        }
        ResponseData::MissedWindowsReconciled { outcome } => writeln!(
            out,
            "Released {} overdue unit(s) now, moved {} forward, left {} in flight",
            outcome.released_now.len(),
            outcome.rescheduled.len(),
            outcome.retained.len()
        ),
        ResponseData::ScheduleRecalculated { recalculation } => writeln!(
            out,
            "Withdrew {} release time(s); left {} in-flight unit(s) untouched",
            recalculation.withdrawn.len(),
            recalculation.retained.len()
        ),
        ResponseData::QueueAudit { audit } => output_queue_audit(out, &audit),
        ResponseData::QueueExported { export } => output_queue_export(out, &export),
    }
    .map_err(output_error)
}

fn output_plan(out: &mut impl Write, plan: &PlanView) -> io::Result<()> {
    writeln!(
        out,
        "Feature: {}\nRevision: {}\nState: {}\nTarget: {}\nGoal: {}\nPhases: {}\nTasks: {}",
        plan.plan.feature_id,
        plan.plan.revision.get(),
        if plan.plan.sealed { "sealed" } else { "draft" },
        plan.plan.target.as_str(),
        plan.plan.goal,
        plan.plan.phases.len(),
        plan.plan
            .phases
            .iter()
            .map(|phase| phase.tasks.len())
            .sum::<usize>()
    )
}

fn output_plan_history(out: &mut impl Write, plans: &[PlanView]) -> io::Result<()> {
    if plans.is_empty() {
        return writeln!(out, "No revisions found.");
    }
    for plan in plans {
        writeln!(
            out,
            "{}  revision {}  {}  {}",
            plan.plan.feature_id,
            plan.plan.revision.get(),
            if plan.plan.sealed { "sealed" } else { "draft" },
            plan.plan.goal
        )?;
    }
    Ok(())
}

fn output_workspace(out: &mut impl Write, workspace: &WorkspaceView) -> io::Result<()> {
    writeln!(
        out,
        "Feature: {}\nRevision: {}\nBase: {}\nPath: {}\nPrerequisites: {}",
        workspace.feature_id,
        workspace.revision.get(),
        workspace.base_commit,
        workspace.path,
        workspace.prerequisites.len()
    )
}

fn output_package(out: &mut impl Write, package: &PackageView) -> io::Result<()> {
    writeln!(
        out,
        "Package: {}\nRevision: {}\nFeature: {}\nBase tree: {}\nResult tree: {}\nContent hash: {}\nTasks: {}",
        package.package_id,
        package.revision.get(),
        package.feature_id,
        package.base_tree,
        package.result_tree,
        package.content_hash,
        package.task_ids.len()
    )
}

fn output_release_unit(out: &mut impl Write, unit: &ReleaseUnitView) -> io::Result<()> {
    writeln!(
        out,
        "Unit: {}\nFeature: {} revision {}\nTasks: {}",
        unit.unit_id,
        unit.feature_id,
        unit.plan_revision.get(),
        unit.task_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn output_schedule_policy(out: &mut impl Write, policy: &SchedulePolicyView) -> io::Result<()> {
    writeln!(
        out,
        "Repository: {}\nPolicy revision: {}\nTime zone: {}\nAllowed days: {}\nDaily releases: {}-{}\nMinimum spacing: {} minute(s)",
        policy.repository_id,
        policy.revision.get(),
        policy.policy.timezone.as_str(),
        policy
            .policy
            .allowed_days
            .iter()
            .map(|day| format!("{day:?}"))
            .collect::<Vec<_>>()
            .join(", "),
        policy.policy.daily_releases.minimum,
        policy.policy.daily_releases.maximum,
        policy.policy.minimum_spacing_minutes,
    )
}

fn output_schedule_preview(
    out: &mut impl Write,
    slots: &[ScheduleSlotView],
    paused: Option<&str>,
) -> io::Result<()> {
    if let Some(reason) = paused {
        writeln!(out, "Paused: {reason}")?;
    }
    if slots.is_empty() {
        return writeln!(out, "No release times are scheduled.");
    }
    for slot in slots {
        writeln!(
            out,
            "{} · package {} · {} ({})",
            slot.release_unit_id, slot.package_id, slot.selected_at_unix_ms, slot.timezone
        )?;
    }
    Ok(())
}

fn output_integration_health(
    out: &mut impl Write,
    integrations: &[IntegrationHealthView],
) -> io::Result<()> {
    if integrations.is_empty() {
        return writeln!(out, "All integrations are healthy.");
    }
    for health in integrations {
        let next = match health.next_attempt_at_unix_ms {
            Some(at) => format!("retry after {at}"),
            // Deliberately explicit: this will not clear on its own.
            None => "needs attention; no retry scheduled".to_owned(),
        };
        writeln!(
            out,
            "{:?} · {} · {:?} after {} failure(s) · {next}\n  {}",
            health.integration,
            health.scope,
            health.fault,
            health.consecutive_failures,
            health.detail
        )?;
    }
    Ok(())
}

fn output_due_units(out: &mut impl Write, units: &[DueUnitView]) -> io::Result<()> {
    if units.is_empty() {
        return writeln!(out, "No units are due for release.");
    }
    for unit in units {
        writeln!(
            out,
            "{} · repository {} · package {} · due {}",
            unit.release_unit_id, unit.repository_id, unit.package_id, unit.selected_at_unix_ms
        )?;
    }
    Ok(())
}

fn output_schedule_slot(out: &mut impl Write, slot: &ScheduleSlotView) -> io::Result<()> {
    writeln!(
        out,
        "Unit: {}\nPackage: {} revision {}\nSelected: {} ({})\nEligible since: {}",
        slot.release_unit_id,
        slot.package_id,
        slot.package_revision.get(),
        slot.selected_at_unix_ms,
        slot.timezone,
        slot.eligible_at_unix_ms,
    )
}

fn output_release_attempt(out: &mut impl Write, attempt: &ReleaseAttemptView) -> io::Result<()> {
    writeln!(
        out,
        "Attempt: {}\nPackage: {} revision {}\nStatus: {:?}\nTarget: {} {}\nCandidate: {}\nRemote observed: {}\nRetry after: {}\nFailure: {}",
        attempt.attempt_id,
        attempt.package_id,
        attempt.package_revision.get(),
        attempt.status,
        attempt.target_remote,
        attempt.target.as_str(),
        attempt.candidate_sha.as_deref().unwrap_or("not prepared"),
        attempt
            .observed_remote_sha
            .as_deref()
            .unwrap_or("not yet observed"),
        attempt
            .retry_not_before_unix_ms
            .map_or_else(|| "not scheduled".into(), |value| value.to_string()),
        attempt.failure_classification.as_deref().unwrap_or("none"),
    )?;
    if let Some(reason) = &attempt.reason {
        writeln!(out, "Reason: {:?}: {}", reason.code, reason.message)?;
    }
    Ok(())
}

fn output_release_attempts(
    out: &mut impl Write,
    attempts: &[ReleaseAttemptView],
) -> io::Result<()> {
    if attempts.is_empty() {
        return writeln!(out, "No release attempts found.");
    }
    for attempt in attempts {
        writeln!(
            out,
            "{}  {:?}  {}@{}  {}",
            attempt.attempt_id,
            attempt.status,
            attempt.package_id,
            attempt.package_revision.get(),
            attempt.failure_classification.as_deref().unwrap_or("-")
        )?;
    }
    Ok(())
}

fn output_queue_audit(out: &mut impl Write, audit: &QueueAuditView) -> io::Result<()> {
    writeln!(
        out,
        "Verified packages: {}\nUnresolved recovery issues: {}",
        audit.verified_package_count,
        audit.issues.len()
    )?;
    for issue in &audit.issues {
        writeln!(out, "- [{}] {}: {}", issue.kind, issue.path, issue.message)?;
    }
    Ok(())
}

fn output_queue_export(out: &mut impl Write, export: &QueueExportView) -> io::Result<()> {
    writeln!(
        out,
        "Exported {} verified package(s) to {}\nUnresolved recovery issues retained in backup: {}\nDatabase SHA-256: {}",
        export.verified_package_count,
        export.destination,
        export.unresolved_issue_count,
        export.database_sha256
    )
}

fn output_events(out: &mut impl Write, events: &[EventView]) -> io::Result<()> {
    if events.is_empty() {
        return writeln!(out, "No diagnostic events have been recorded.");
    }
    for event in events {
        let severity = match event.severity {
            EventSeverityView::Debug => "DEBUG",
            EventSeverityView::Info => "INFO",
            EventSeverityView::Warning => "WARN",
            EventSeverityView::Error => "ERROR",
        };
        let request_id = event
            .request_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned());
        writeln!(
            out,
            "#{:<6} {:<5} {:<24} {}  {}",
            event.sequence, severity, event.kind, request_id, event.message
        )?;
    }
    Ok(())
}

fn output_repositories(out: &mut impl Write, repositories: &[RepositoryView]) -> io::Result<()> {
    if repositories.is_empty() {
        return writeln!(out, "No repositories are registered.");
    }
    for repository in repositories {
        writeln!(
            out,
            "{}  {}  {}",
            repository.id,
            repository.target.as_str(),
            repository.checkout_path
        )?;
    }
    Ok(())
}

fn doctor(session: &Session, json_output: bool, out: &mut impl Write) -> Result<(), CliFailure> {
    let mut checks = Vec::new();
    let git = ProcessCommand::new("git").arg("--version").output();
    match git {
        Ok(output) if output.status.success() => checks.push(DoctorCheck {
            name: "git",
            status: "ready",
            detail: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        }),
        _ => checks.push(DoctorCheck {
            name: "git",
            status: "action_required",
            detail: "Git is unavailable".into(),
        }),
    }

    let state_ready = fs::metadata(&session.state_dir)
        .map(|metadata| metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0)
        .unwrap_or(false);
    checks.push(DoctorCheck {
        name: "state_directory",
        status: if state_ready {
            "ready"
        } else {
            "action_required"
        },
        detail: if state_ready {
            format!("{} is owner-only", session.state_dir.display())
        } else {
            format!(
                "{} is missing or not owner-only",
                session.state_dir.display()
            )
        },
    });

    let service_result = send(session, Command::Ping);
    checks.push(DoctorCheck {
        name: "service",
        status: if service_result.is_ok() {
            "ready"
        } else {
            "unavailable"
        },
        detail: service_result
            .map(|_| "local API is reachable".into())
            .unwrap_or_else(|error| error.message),
    });
    let healthy = checks.iter().all(|check| check.status == "ready");
    let report = DoctorReport { healthy, checks };
    if json_output {
        writeln!(out, "{}", json!({ "ok": healthy, "data": report })).map_err(output_error)?;
    } else {
        for check in &report.checks {
            writeln!(
                out,
                "{:<18} {:<16} {}",
                check.name, check.status, check.detail
            )
            .map_err(output_error)?;
        }
    }
    if report.healthy {
        Ok(())
    } else if report
        .checks
        .iter()
        .any(|check| check.name == "service" && check.status != "ready")
    {
        Err(CliFailure::new(
            EXIT_UNAVAILABLE,
            "doctor_failed",
            "one or more diagnostics require attention",
        )
        .already_reported())
    } else {
        Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "doctor_failed",
            "one or more diagnostics require attention",
        )
        .already_reported())
    }
}

fn resolve_state_dir(override_path: Option<PathBuf>) -> Result<PathBuf, CliFailure> {
    if let Some(path) =
        override_path.or_else(|| env::var_os("RECCURSIVE_STATE_DIR").map(PathBuf::from))
    {
        return Ok(path);
    }
    let home = env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "state_directory_missing",
            "HOME is unavailable; pass --state-dir",
        )
    })?;
    #[cfg(target_os = "macos")]
    return Ok(home
        .join("Library")
        .join("Application Support")
        .join("ReccursiveCommits"));
    #[cfg(not(target_os = "macos"))]
    Ok(env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
        .join("reccursive-commits"))
}

fn output_error(error: io::Error) -> CliFailure {
    CliFailure::new(EXIT_INTERNAL, "output_failed", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_normalized_to_full_refs() {
        assert_eq!(branch_ref("main").unwrap().as_str(), "refs/heads/main");
        assert_eq!(
            branch_ref("refs/heads/release").unwrap().as_str(),
            "refs/heads/release"
        );
    }

    #[test]
    fn log_limits_are_bounded_at_parse_time() {
        assert_eq!(parse_event_limit("1"), Ok(1));
        assert_eq!(parse_event_limit("1000"), Ok(1_000));
        assert!(parse_event_limit("0").is_err());
        assert!(parse_event_limit("1001").is_err());
        assert!(parse_event_limit("many").is_err());
    }

    #[test]
    fn release_attempt_listing_is_a_valid_cli_command() {
        let directory = tempfile::tempdir().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            [
                "reccursive",
                "--state-dir",
                directory.path().to_str().unwrap(),
                "release",
                "attempts",
            ],
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(
            exit,
            EXIT_UNAVAILABLE,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
    }

    #[test]
    fn unavailable_service_has_a_stable_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            [
                "reccursive",
                "--state-dir",
                directory.path().to_str().unwrap(),
                "--json",
                "status",
            ],
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, EXIT_UNAVAILABLE);
        let error: serde_json::Value = serde_json::from_slice(&stderr).unwrap();
        assert_eq!(error["error"]["code"], "service_unavailable");
    }

    #[test]
    fn invalid_command_uses_the_usage_exit_code() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(["reccursive", "unknown"], &mut stdout, &mut stderr),
            EXIT_USAGE
        );
        assert!(!stderr.is_empty());
    }

    #[test]
    fn invalid_json_command_has_a_stable_error_envelope() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(
                ["reccursive", "--json", "unknown"],
                &mut stdout,
                &mut stderr,
            ),
            EXIT_USAGE
        );
        assert!(stdout.is_empty());
        let error: serde_json::Value = serde_json::from_slice(&stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "usage");
        assert_eq!(
            error["error"]["message"],
            "Invalid command or arguments. Run `reccursive --help` for usage."
        );
    }

    #[test]
    fn help_is_successful_and_is_written_to_standard_output() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(["reccursive", "--help"], &mut stdout, &mut stderr),
            EXIT_SUCCESS
        );
        assert!(stderr.is_empty());
        let help = String::from_utf8(stdout).unwrap();
        assert!(help.contains("Start here:"));
        assert!(help.contains("For stable automation output, pass --json."));
    }

    #[test]
    fn doctor_json_is_one_report_even_when_service_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            [
                "reccursive",
                "--state-dir",
                directory.path().to_str().unwrap(),
                "--json",
                "doctor",
            ],
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, EXIT_UNAVAILABLE);
        assert!(stderr.is_empty());
        let lines = String::from_utf8(stdout).unwrap();
        assert_eq!(lines.lines().count(), 1);
        let report: serde_json::Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(report["ok"], false);
        assert!(report["data"]["checks"].is_array());
    }
}
