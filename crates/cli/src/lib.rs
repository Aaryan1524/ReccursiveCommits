use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use clap::{Parser, Subcommand, ValueEnum, error::ErrorKind};
use install::{InstallError, ServiceInstallation};
use reccursive_protocol::{
    ApiError, ApiErrorCode, CancelTaskRequest, CapturePackageRequest,
    ChangeRepositoryPolicyRequest, CheckResultsView, Command, CreateReleaseUnitRequest,
    CreateWorkspaceRequest, DevelopmentTargetChange, DueUnitView, EnrollRepositoryRequest,
    EventSeverityView, EventView, FeatureId, FeaturePlan, IdempotencyKey, IntegrationHealthView,
    LocalClient, PackageChangesView, PackageId, PackageView, PlanView, PublicationMode,
    QueueAuditView, QueueExportView, QueueSummaryView, QueuedUnitState, ReleaseAttemptView,
    ReleasePackageRequest, ReleaseUnitId, ReleaseUnitView, RepositoryId, RepositoryView,
    ResponseData, Revision, ScheduleChangeView, SchedulePolicy, SchedulePolicyOverride,
    SchedulePolicyView, ScheduleSlotView, ScheduleUnitRequest, SetSchedulePolicyRequest,
    SubmissionView, SubmitTaskRequest, TargetIntegration, TargetMilestone, TargetRef, TaskId,
    TaskProgressView, WorkspaceView, next_action_for_attempt,
};
use serde::Serialize;
use serde_json::json;

mod authoring;
mod install;
mod setup;

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
    /// Manage the GitHub token the pull-request strategy uses.
    Github {
        #[command(subcommand)]
        command: GithubCommand,
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
    /// Set up a repository end to end: enrollment, schedule, and the background service.
    Setup {
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
        /// Development branch, required by immediate mode.
        #[arg(long)]
        development_target: Option<String>,
        /// Activate this schedule policy instead of the built-in weekday default.
        #[arg(long, value_name = "FILE")]
        schedule: Option<PathBuf>,
        /// Enroll without activating any schedule policy.
        #[arg(long, conflicts_with = "schedule")]
        no_schedule: bool,
        /// Time zone for the built-in default policy; defaults to this machine's.
        #[arg(long)]
        timezone: Option<String>,
        /// Install the background service as part of setup.
        #[arg(long)]
        install_service: bool,
        /// Never prompt. Every value must come from a flag or a safe default.
        #[arg(long)]
        non_interactive: bool,
    },
    /// Check local prerequisites and service connectivity.
    Doctor,
    /// Show integrations that are currently failing and when each may be retried.
    Integrations,
    /// Check whether credentials and signing would let a repository publish right now.
    Diagnose { repository_id: RepositoryId },
    /// Explain what does and does not decide whether published work counts as a contribution.
    Contributions { repository_id: RepositoryId },
    /// Write a shareable diagnostic report, leaving the queue untouched.
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
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
    /// Show what a captured package changes, relative to the base it was captured against.
    Changes {
        package_id: PackageId,
        #[arg(long, value_parser = parse_revision, default_value = "1")]
        revision: Revision,
        /// Include the patch text, not only which files changed.
        #[arg(long)]
        patch: bool,
    },
    /// Show the checks that ran for a package, at capture and against each release candidate.
    Checks {
        package_id: PackageId,
        #[arg(long, value_parser = parse_revision, default_value = "1")]
        revision: Revision,
    },
}

#[derive(Debug, Subcommand)]
enum DiagnosticsCommand {
    /// Collect service state into a directory you can share.
    Export {
        #[arg(value_name = "DIRECTORY")]
        destination: PathBuf,
        /// How many recent events to include.
        #[arg(long, default_value_t = 200, value_parser = parse_event_limit)]
        limit: usize,
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
        /// The repository to pause. Omit only with --all.
        repository_id: Option<RepositoryId>,
        #[arg(long)]
        reason: String,
        /// Pause every enrolled repository.
        #[arg(long, conflicts_with = "repository_id")]
        all: bool,
    },
    /// Resume a paused repository.
    Resume { repository_id: RepositoryId },
    /// Move one unit's release time to now, subject to the same eligibility rules.
    ReleaseNow { release_unit_id: ReleaseUnitId },
    /// Show a repository's upcoming release times without changing any of them.
    Preview { repository_id: RepositoryId },
    /// List every release time selected for one unit, and why each stopped being valid.
    History { release_unit_id: ReleaseUnitId },
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
    /// Show queued release work and where each unit stands.
    Status {
        /// Restrict to one repository.
        #[arg(long)]
        repository_id: Option<RepositoryId>,
        /// Show only units in this state.
        #[arg(long, value_enum)]
        state: Option<QueueStateFilter>,
        /// Show only units that need a person: blocked, or whose release time was withdrawn.
        #[arg(long, conflicts_with = "state")]
        needs_attention: bool,
    },
    /// Redraw the queue on an interval until you stop watching.
    Watch {
        #[arg(long)]
        repository_id: Option<RepositoryId>,
        /// Seconds between redraws.
        #[arg(long, default_value_t = 5, value_parser = parse_watch_interval)]
        interval: u64,
        /// Stop after this many seconds instead of running until interrupted.
        #[arg(long, value_name = "SECONDS")]
        r#for: Option<u64>,
    },
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
    /// Write a starter plan document that already imports.
    Template {
        /// Repository the plan delivers into.
        repository_id: RepositoryId,
        /// Target branch name or full refs/heads ref.
        #[arg(long, default_value = "main")]
        target: String,
        /// Write here instead of standard output.
        #[arg(long, short = 'o', value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Validate a plan document locally, without sending it anywhere.
    Check {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Write a stored revision back out as a document.
    Export {
        feature_id: FeatureId,
        #[arg(long, value_parser = parse_revision)]
        revision: Option<Revision>,
        #[arg(long, short = 'o', value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Open the newest revision in your editor and append the result as the next revision.
    Edit { feature_id: FeatureId },
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
        /// How the target branch is reached.
        #[arg(long, value_enum, default_value = "direct-push")]
        integration: TargetIntegrationArgument,
    },
    /// Change where or how an enrolled repository publishes.
    ///
    /// Only what you name changes. The change is refused, with the reason, if anything is
    /// mid-publication under the old policy.
    SetPolicy {
        /// The repository to change.
        repository_id: RepositoryId,
        /// New publication behavior.
        #[arg(long, value_enum)]
        mode: Option<PublicationModeArgument>,
        /// New target branch.
        #[arg(long)]
        target: Option<String>,
        /// New development branch.
        #[arg(long, conflicts_with = "no_development_target")]
        development_target: Option<String>,
        /// Remove the development branch.
        #[arg(long)]
        no_development_target: bool,
        /// New way of reaching the target.
        #[arg(long, value_enum)]
        integration: Option<TargetIntegrationArgument>,
    },
    /// List registered repositories.
    List,
}

#[derive(Debug, Subcommand)]
enum GithubCommand {
    /// Store a GitHub token for a repository, read from standard input.
    ///
    /// Read from stdin rather than taken as an argument, because an argument would be visible in
    /// the process list and saved in shell history.
    SetToken {
        /// The repository the token belongs to.
        repository_id: String,
    },
    /// Report whether a token is stored, without printing it.
    Status {
        /// The repository to check.
        repository_id: String,
    },
    /// Remove a stored token.
    ForgetToken {
        /// The repository to remove the token for.
        repository_id: String,
    },
}

/// How the target branch is reached.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TargetIntegrationArgument {
    /// Push straight to the target using the Git credentials already in use.
    DirectPush,
    /// Open a pull request against the target. You merge it yourself; nothing merges it for you.
    PullRequest,
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
        TopLevelCommand::Setup {
            path,
            remote,
            target,
            mode,
            development_target,
            schedule,
            no_schedule,
            timezone,
            install_service,
            non_interactive,
        } => run_setup(
            &session,
            SetupArguments {
                path,
                remote,
                target,
                mode,
                development_target,
                schedule,
                no_schedule,
                timezone,
                install_service,
                non_interactive,
            },
            cli.json,
            stdout,
        ),
        TopLevelCommand::Diagnostics { command } => match command {
            DiagnosticsCommand::Export { destination, limit } => {
                export_diagnostics(&session, &destination, limit, cli.json, stdout)
            }
        },
        TopLevelCommand::Doctor => doctor(&session, cli.json, stdout),
        TopLevelCommand::Contributions { repository_id } => {
            let data = send(&session, Command::ListRepositories)?;
            output_contributions(stdout, repository_id, data)
        }
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
            PlanCommand::Template {
                repository_id,
                target,
                output,
            } => plan_template(repository_id, &target, output.as_deref(), stdout),
            PlanCommand::Check { file } => plan_check(&file, cli.json, stdout),
            PlanCommand::Export {
                feature_id,
                revision,
                output,
            } => plan_export(&session, feature_id, revision, output.as_deref(), stdout),
            PlanCommand::Edit { feature_id } => plan_edit(&session, feature_id, cli.json, stdout),
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
            PackageCommand::Changes {
                package_id,
                revision,
                patch,
            } => {
                let data = send(
                    &session,
                    Command::InspectPackageChanges {
                        package_id,
                        revision,
                        include_patch: patch,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            PackageCommand::Checks {
                package_id,
                revision,
            } => {
                let data = send(
                    &session,
                    Command::ListCheckResults {
                        package_id,
                        revision,
                    },
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
                all,
            } => pause_repositories(&session, repository_id, &reason, all, cli.json, stdout),
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
            ScheduleCommand::History { release_unit_id } => {
                let data = send(&session, Command::ScheduleHistory { release_unit_id })?;
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
            QueueCommand::Status {
                repository_id,
                state,
                needs_attention,
            } => {
                let data = send(&session, Command::SummarizeQueue { repository_id })?;
                let data = filter_queue(data, state, needs_attention);
                output_data(data, cli.json, stdout)
            }
            QueueCommand::Watch {
                repository_id,
                interval,
                r#for,
            } => queue_watch(&session, repository_id, interval, r#for, cli.json, stdout),
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
        TopLevelCommand::Github { command } => run_github(command, &session.state_dir, stdout),
        TopLevelCommand::Repository { command } => match command {
            RepositoryCommand::SetPolicy {
                repository_id,
                mode,
                target,
                development_target,
                no_development_target,
                integration,
            } => {
                let request = ChangeRepositoryPolicyRequest {
                    repository_id,
                    publication_mode: mode.map(|mode| match mode {
                        PublicationModeArgument::Scheduled => PublicationMode::ScheduledCreation,
                        PublicationModeArgument::Immediate => {
                            PublicationMode::ImmediateAvailability
                        }
                    }),
                    target: target.as_deref().map(branch_ref).transpose()?,
                    // Absent means "leave it alone", so removing a branch has to be said out loud
                    // rather than implied by omitting the flag.
                    development_target: match (no_development_target, &development_target) {
                        (true, _) => DevelopmentTargetChange::Remove,
                        (false, Some(branch)) => DevelopmentTargetChange::Set(branch_ref(branch)?),
                        (false, None) => DevelopmentTargetChange::Keep,
                    },
                    target_integration: integration.map(|integration| match integration {
                        TargetIntegrationArgument::DirectPush => TargetIntegration::DirectPush,
                        TargetIntegrationArgument::PullRequest => TargetIntegration::PullRequest,
                    }),
                };
                let data = send(&session, Command::ChangeRepositoryPolicy(request))?;
                output_data(data, cli.json, stdout)
            }
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
                integration,
            } => {
                let request = enrollment_request(
                    path,
                    remote,
                    target,
                    mode,
                    development_target,
                    integration,
                )?;
                let data = send(&session, Command::EnrollRepository(request))?;
                output_data(data, cli.json, stdout)
            }
        },
    }
}

fn write_document(
    document: &str,
    output: Option<&Path>,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    match output {
        Some(path) => {
            fs::write(path, document).map_err(|error| {
                CliFailure::new(
                    EXIT_ACTION_REQUIRED,
                    "write_failed",
                    format!("cannot write {}: {error}", path.display()),
                )
            })?;
            writeln!(out, "Wrote {}", path.display()).map_err(output_error)
        }
        None => write!(out, "{document}").map_err(output_error),
    }
}

fn parse_watch_interval(value: &str) -> Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "interval must be a whole number of seconds".to_owned())?;
    if (1..=3_600).contains(&seconds) {
        Ok(seconds)
    } else {
        Err("interval must be between 1 and 3600 seconds".to_owned())
    }
}

/// Redraws the queue until the watcher is stopped.
///
/// Watching is a *display*. It holds no lease, claims no work, and tells the daemon nothing — it
/// asks the same question `queue status` asks, repeatedly. Stopping it therefore cannot affect
/// what is queued or whether the service publishes: the CLI is a protocol client with no authority
/// to stop anything, so this is structural rather than careful.
fn queue_watch(
    session: &Session,
    repository_id: Option<RepositoryId>,
    interval_seconds: u64,
    stop_after_seconds: Option<u64>,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    let interval = std::time::Duration::from_secs(interval_seconds);
    let deadline = stop_after_seconds
        .map(|seconds| std::time::Instant::now() + std::time::Duration::from_secs(seconds));
    loop {
        let data = send(session, Command::SummarizeQueue { repository_id })?;
        // Clear and home, so a terminal shows one queue that updates rather than a transcript.
        // Only when a terminal is actually there: piping a watch into a file or another program
        // should produce readable text, not control characters someone has to strip. Nothing here
        // is a full-screen mode, and nothing requires a mouse.
        if !json_output && io::stdout().is_terminal() {
            let _ = write!(out, "\x1b[2J\x1b[H");
        }
        match output_data(data, json_output, out) {
            Ok(()) => {}
            // The reader went away — a pipe closed, a pager quit. That is the watch ending
            // normally, not a failure worth an error message.
            Err(failure) if failure.code == "output_failed" => return Ok(()),
            Err(failure) => return Err(failure),
        }
        if let Some(deadline) = deadline
            && std::time::Instant::now() + interval > deadline
        {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn plan_template(
    repository_id: RepositoryId,
    target: &str,
    output: Option<&Path>,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    // A plan names the full ref, while every other command accepts a bare branch name. Normalizing
    // here keeps the template consistent with how the user just typed it everywhere else.
    let target = branch_ref(target)?;
    write_document(
        &authoring::template(repository_id, target.as_str()),
        output,
        out,
    )
}

/// Validates a document locally, using the same rules the daemon applies at import.
///
/// Local on purpose: an author iterating on a plan should not have to reach the service, and a
/// document rejected here is a document import would have rejected anyway.
fn plan_check(file: &Path, json_output: bool, out: &mut impl Write) -> Result<(), CliFailure> {
    let plan = load_plan(file)?;
    if let Err(error) = plan.validate() {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_plan",
            format!("{}: {error}", file.display()),
        ));
    }
    let tasks: usize = plan.phases.iter().map(|phase| phase.tasks.len()).sum();
    if json_output {
        let value = json!({
            "ok": true,
            "data": {
                "type": "plan_checked",
                "payload": {
                    "feature_id": plan.feature_id.to_string(),
                    "revision": plan.revision.get(),
                    "sealed": plan.sealed,
                    "phases": plan.phases.len(),
                    "tasks": tasks,
                }
            }
        });
        return writeln!(out, "{value}").map_err(output_error);
    }
    writeln!(
        out,
        "{} is a valid plan: revision {}, {} phase(s), {tasks} task(s), {}",
        file.display(),
        plan.revision.get(),
        plan.phases.len(),
        if plan.sealed { "sealed" } else { "draft" }
    )
    .map_err(output_error)
}

fn plan_export(
    session: &Session,
    feature_id: FeatureId,
    revision: Option<Revision>,
    output: Option<&Path>,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    let plan = fetch_plan(session, feature_id, revision)?;
    let document = serde_json::to_string_pretty(&plan.plan)
        .map_err(|error| CliFailure::new(EXIT_INTERNAL, "encode_failed", error.to_string()))?;
    write_document(&format!("{document}\n"), output, out)
}

fn fetch_plan(
    session: &Session,
    feature_id: FeatureId,
    revision: Option<Revision>,
) -> Result<PlanView, CliFailure> {
    match send(
        session,
        Command::GetPlan {
            feature_id,
            revision,
        },
    )? {
        ResponseData::Plan { plan } => Ok(plan),
        _ => Err(CliFailure::new(
            EXIT_INTERNAL,
            "unexpected_response",
            "reading a plan returned an unexpected result",
        )),
    }
}

/// Opens the newest revision in the user's editor and appends the result as the next revision.
///
/// The stored revision is never written to. An edit reads revision N, and imports what comes back
/// as N+1 — so published work, which names the revision it was built from, is untouched by
/// construction rather than by care.
fn plan_edit(
    session: &Session,
    feature_id: FeatureId,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    let editor = authoring::preferred_editor().ok_or_else(|| {
        CliFailure::new(
            EXIT_USAGE,
            "editor_unset",
            "no editor configured; set VISUAL or EDITOR, or use plan export, edit the file, and plan import",
        )
    })?;
    let current = fetch_plan(session, feature_id, None)?;
    let read_revision = current.plan.revision;

    let scratch = tempfile::tempdir()
        .map_err(|error| CliFailure::new(EXIT_INTERNAL, "scratch_failed", error.to_string()))?;
    let path = authoring::scratch_path(scratch.path(), feature_id, read_revision.get());
    let document = serde_json::to_string_pretty(&current.plan)
        .map_err(|error| CliFailure::new(EXIT_INTERNAL, "encode_failed", error.to_string()))?;
    fs::write(&path, format!("{document}\n"))
        .map_err(|error| CliFailure::new(EXIT_INTERNAL, "scratch_failed", error.to_string()))?;

    if !authoring::open_in_editor(&editor, &path).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "editor_failed",
            format!("could not run {}: {error}", editor.to_string_lossy()),
        )
    })? {
        return Err(CliFailure::new(
            EXIT_USAGE,
            "editor_failed",
            "the editor exited with an error; nothing was imported",
        ));
    }

    let edited = load_plan(&path)?;
    if let Err(error) = edited.validate() {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_plan",
            format!("the edited plan is not valid, so nothing was imported: {error}"),
        ));
    }
    if edited == current.plan {
        return Err(CliFailure::new(
            EXIT_USAGE,
            "unchanged",
            format!(
                "revision {} was not changed, so no revision was appended",
                read_revision.get()
            ),
        ));
    }

    let next = Revision::new(read_revision.get() + 1).map_err(|error| {
        CliFailure::new(EXIT_ACTION_REQUIRED, "invalid_revision", error.to_string())
    })?;
    let plan = authoring::as_next_revision(edited, next);
    // Imported against the revision that was read, not whatever is newest now. If something
    // appended while the editor was open, the daemon refuses this as a conflict rather than
    // silently rebasing edits onto a revision the author never saw.
    let data = send(session, Command::ImportPlan { plan })?;
    output_data(data, json_output, out)
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

/// Stores, reports on, or removes the GitHub token for one repository.
///
/// Handled entirely in the CLI, against the same state directory the service uses. The token never
/// crosses the local API in either direction: the fewer places a secret travels, the fewer places
/// it can be logged, cached, or exported by accident.
/// Explains what this machine actually controls about a contribution, and what it does not.
///
/// The point of this command is a refusal: it will not tell you that scheduling a push for a date
/// makes a green square on that date. Those are different things, decided by different systems,
/// and a tool that blurred them would be selling a promise it cannot keep. So it reports the facts
/// it can establish locally — the identity commits will carry, the branch they will land on, and
/// which date Git will record — and names the conditions it cannot check from here, rather than
/// guessing at them.
fn output_contributions(
    out: &mut impl Write,
    repository_id: RepositoryId,
    data: ResponseData,
) -> Result<(), CliFailure> {
    let ResponseData::Repositories { repositories } = data else {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not return the repository list",
        ));
    };
    let repository = repositories
        .into_iter()
        .find(|candidate| candidate.id == repository_id)
        .ok_or_else(|| {
            CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "repository_missing",
                format!("{repository_id} is not enrolled"),
            )
        })?;

    let identity = setup::checkout_identity(std::path::Path::new(&repository.checkout_path));
    writeln!(out, "Contributions for {repository_id}\n").map_err(output_error)?;

    writeln!(out, "What this machine decides:").map_err(output_error)?;
    match &identity {
        Some((name, email)) => writeln!(
            out,
            "  author      {name} <{email}>\n\
             \x20             read from the checkout; every commit published from here carries it",
        ),
        None => writeln!(
            out,
            "  author      not configured\n\
             \x20             nothing can be published until the checkout has user.name and \
             user.email"
        ),
    }
    .map_err(output_error)?;
    writeln!(
        out,
        "  branch      {}{}",
        repository.target.as_str(),
        match &repository.development_target {
            Some(development) => format!(
                "\n              work reaches {} first; only the target above is the \
                 destination that finishes it",
                development.as_str()
            ),
            None => String::new(),
        }
    )
    .map_err(output_error)?;
    writeln!(
        out,
        "  date        the commit is created at its release time, so the author date and the \
         push are the same moment\n\
         \x20             no commit is backdated, and none is invented to fill a day"
    )
    .map_err(output_error)?;

    writeln!(
        out,
        "\nWhat GitHub decides, and this cannot check from here:\n\
         \x20 - whether {} is linked to your GitHub account\n\
         \x20 - whether {} is that repository's default branch\n\
         \x20 - whether the repository is a fork, and whether private contributions are shown\n\
         \x20 - how its own rules count a commit on any given day\n\
         \x20\n\
         \x20 These are GitHub's rules and they change without notice. Read them at\n\
         \x20 https://docs.github.com/account-and-profile rather than trusting a summary here.",
        identity
            .as_ref()
            .map_or("your commit email", |(_, email)| email.as_str()),
        repository.target.as_str(),
    )
    .map_err(output_error)?;

    // The sentence this whole command exists to say.
    writeln!(
        out,
        "\nA release time is when this service will publish. It is not a promise about your\n\
         contribution graph, and nothing here treats the two as the same thing."
    )
    .map_err(output_error)
}

fn run_github(
    command: GithubCommand,
    state_dir: &std::path::Path,
    stdout: &mut impl Write,
) -> Result<(), CliFailure> {
    let refusal =
        |detail: String| CliFailure::new(EXIT_ACTION_REQUIRED, "github_token_unusable", detail);
    match command {
        GithubCommand::SetToken { repository_id } => {
            let mut token = String::new();
            std::io::Read::read_to_string(&mut io::stdin(), &mut token).map_err(|error| {
                refusal(format!(
                    "could not read the token from standard input: {error}"
                ))
            })?;
            reccursive_github::storage::store(state_dir, &repository_id, token.trim())
                .map_err(|error| refusal(error.to_string()))?;
            writeln!(
                stdout,
                "Stored a GitHub token for {repository_id}.\n\
                 It is readable only by you, and is never written to logs, events, or the \
                 diagnostic export.\n\
                 Nothing merges pull requests for you; opening them is all this token is used for."
            )
            .map_err(output_error)
        }
        GithubCommand::Status { repository_id } => {
            let present = reccursive_github::storage::is_present(state_dir, &repository_id);
            writeln!(
                stdout,
                "{}",
                if present {
                    format!("A GitHub token is stored for {repository_id}.")
                } else {
                    format!(
                        "No GitHub token is stored for {repository_id}.\n\
                         Repositories that publish by direct push do not need one."
                    )
                }
            )
            .map_err(output_error)
        }
        GithubCommand::ForgetToken { repository_id } => {
            reccursive_github::storage::forget(state_dir, &repository_id)
                .map_err(|error| refusal(error.to_string()))?;
            writeln!(
                stdout,
                "Removed the stored GitHub token for {repository_id}.\n\
                 Revoke it on GitHub as well if you no longer want it to exist."
            )
            .map_err(output_error)
        }
    }
}

fn enrollment_request(
    path: PathBuf,
    remote: Option<String>,
    target: String,
    mode: PublicationModeArgument,
    development_target: Option<String>,
    integration: TargetIntegrationArgument,
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
    let target_integration = match integration {
        TargetIntegrationArgument::DirectPush => TargetIntegration::DirectPush,
        TargetIntegrationArgument::PullRequest => TargetIntegration::PullRequest,
    };
    let request = EnrollRepositoryRequest {
        target_integration,
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

fn output_package_changes(
    out: &mut impl Write,
    changes: &PackageChangesView,
) -> Result<(), CliFailure> {
    writeln!(
        out,
        "{} revision {}\nBase: {}\n{} file(s) changed",
        changes.package_id,
        changes.revision.get(),
        changes.base_commit,
        changes.files.len()
    )
    .map_err(output_error)?;
    if changes.files.is_empty() {
        // A package that changes nothing should never have been captured, so say so plainly
        // rather than printing an empty list that reads like a rendering failure.
        writeln!(out, "This package changes nothing.").map_err(output_error)?;
    }
    for file in &changes.files {
        writeln!(out, "  {:<2} {}", file.change, file.path).map_err(output_error)?;
    }
    if let Some(patch) = &changes.patch {
        writeln!(out, "\n{patch}").map_err(output_error)?;
        if changes.patch_truncated {
            writeln!(
                out,
                "\n[patch cut short at the size limit; the file list above is complete]"
            )
            .map_err(output_error)?;
        }
    }
    Ok(())
}

fn output_check_results(
    out: &mut impl Write,
    results: &CheckResultsView,
) -> Result<(), CliFailure> {
    if results.at_capture.is_empty() && results.at_release.is_empty() {
        return writeln!(out, "No checks have run for this package yet.").map_err(output_error);
    }
    for (heading, group) in [
        (
            "At capture, in the package's own workspace:",
            &results.at_capture,
        ),
        (
            "At release, against a reconciled candidate:",
            &results.at_release,
        ),
    ] {
        if group.is_empty() {
            continue;
        }
        writeln!(out, "{heading}").map_err(output_error)?;
        for result in group {
            // Passed means exit zero and no timeout. A check that never returned has no exit code
            // at all, which is a different answer from failing and is reported as one.
            let verdict = if result.timed_out {
                "timed out".to_owned()
            } else {
                match result.exit_code {
                    Some(0) => "passed".to_owned(),
                    Some(code) => format!("failed ({code})"),
                    None => "did not return".to_owned(),
                }
            };
            writeln!(out, "  {} · {verdict}", result.check_id).map_err(output_error)?;
            writeln!(out, "    {}", result.command.join(" ")).map_err(output_error)?;
            if let Some(attempt_id) = result.attempt_id {
                writeln!(out, "    attempt {attempt_id}").map_err(output_error)?;
            }
            if let Some(reason) = &result.invalidated_reason {
                writeln!(out, "    no longer stands: {reason}").map_err(output_error)?;
            }
            if !result.output_summary.trim().is_empty() {
                writeln!(out, "    {}", result.output_summary.trim()).map_err(output_error)?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum QueueStateFilter {
    Ready,
    Scheduled,
    Due,
    Publishing,
    Blocked,
    Cancelled,
    Published,
}

impl QueueStateFilter {
    const fn matches(self, state: QueuedUnitState) -> bool {
        matches!(
            (self, state),
            (Self::Ready, QueuedUnitState::Ready)
                | (Self::Scheduled, QueuedUnitState::Scheduled)
                | (Self::Due, QueuedUnitState::Due)
                | (Self::Publishing, QueuedUnitState::Publishing)
                | (Self::Blocked, QueuedUnitState::Blocked)
                | (Self::Cancelled, QueuedUnitState::Cancelled)
                | (Self::Published, QueuedUnitState::Published)
        )
    }
}

/// Narrows a queue summary to the units a caller asked about.
///
/// Filtering happens here rather than in the daemon because it is a question about presentation,
/// not about state: the same summary answers every filter, and pushing the predicate across the
/// protocol would mean a new API shape each time someone wants a different view. Repositories are
/// kept even when empty, so a filter that matches nothing still shows where it looked.
fn filter_queue(
    data: ResponseData,
    state: Option<QueueStateFilter>,
    needs_attention: bool,
) -> ResponseData {
    let ResponseData::QueueSummary { mut summary } = data else {
        return data;
    };
    if state.is_none() && !needs_attention {
        return ResponseData::QueueSummary { summary };
    }
    for repository in &mut summary.repositories {
        repository.units.retain(|unit| {
            if needs_attention {
                // What a person has to look at: stopped work, and work that quietly lost its
                // place in the schedule.
                return unit.state == QueuedUnitState::Blocked
                    || unit.last_schedule_change.is_some();
            }
            state.is_none_or(|filter| filter.matches(unit.state))
        });
    }
    ResponseData::QueueSummary { summary }
}

/// Pauses one repository, or every enrolled one.
///
/// Pausing all of them is several calls, not one: pausing is per repository, and there is no
/// durable "everything is paused" state to set. That is reported honestly — each repository is
/// named as it is paused, and if one refuses, the ones already paused stay paused and are listed,
/// because silently rolling them back would resume publishing an operator asked to stop.
fn pause_repositories(
    session: &Session,
    repository_id: Option<RepositoryId>,
    reason: &str,
    all: bool,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    if !all {
        let repository_id = repository_id.ok_or_else(|| {
            CliFailure::new(
                EXIT_USAGE,
                "repository_required",
                "name a repository to pause, or pass --all to pause every enrolled repository",
            )
        })?;
        let data = send(
            session,
            Command::PauseRepository {
                repository_id,
                reason: reason.to_owned(),
            },
        )?;
        return output_data(data, json_output, out);
    }

    let repositories = match send(session, Command::ListRepositories)? {
        ResponseData::Repositories { repositories } => repositories,
        _ => {
            return Err(CliFailure::new(
                EXIT_INTERNAL,
                "unexpected_response",
                "listing repositories returned an unexpected result",
            ));
        }
    };
    if repositories.is_empty() {
        return writeln!(out, "No repositories are enrolled.").map_err(output_error);
    }

    let mut paused = Vec::new();
    for repository in &repositories {
        match send(
            session,
            Command::PauseRepository {
                repository_id: repository.id,
                reason: reason.to_owned(),
            },
        ) {
            Ok(_) => paused.push(repository.id),
            Err(failure) => {
                let mut message = format!("{}: {}", repository.id, failure.message);
                if !paused.is_empty() {
                    message.push_str("\nAlready paused and left paused:");
                    for id in &paused {
                        message.push_str(&format!("\n  {id}"));
                    }
                }
                return Err(CliFailure::new(failure.exit_code, failure.code, message));
            }
        }
    }

    if json_output {
        let value = json!({
            "ok": true,
            "data": {
                "type": "repositories_paused",
                "payload": {
                    "paused": paused.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "reason": reason,
                }
            }
        });
        return writeln!(out, "{value}").map_err(output_error);
    }
    writeln!(out, "Paused {} repositories: {reason}", paused.len()).map_err(output_error)?;
    for id in paused {
        writeln!(out, "  {id}").map_err(output_error)?;
    }
    Ok(())
}

/// Collects service state into a directory the user can hand to someone else.
///
/// Everything written here is read back through the ordinary local API, which is what makes it
/// safe to share: events are scrubbed for credential-shaped text on the way into storage, check
/// output is scrubbed when it is recorded, and enrollment already refuses a remote with embedded
/// credentials. Nothing reads the auth token, the database, or package contents.
///
/// It is a copy. The queue is never moved, trimmed, or otherwise disturbed by being described —
/// exporting diagnostics must never cost someone the work they were trying to get help with.
fn export_diagnostics(
    session: &Session,
    destination: &Path,
    limit: usize,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "destination_exists",
            format!(
                "{} already exists; name a directory that does not yet exist",
                destination.display()
            ),
        ));
    }
    fs::create_dir_all(destination).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "write_failed",
            format!("cannot create {}: {error}", destination.display()),
        )
    })?;

    // Read-only commands only. A diagnostic report that changed state would be a trap.
    let sections: [(&str, Command); 5] = [
        ("status.json", Command::Status),
        ("repositories.json", Command::ListRepositories),
        (
            "queue.json",
            Command::SummarizeQueue {
                repository_id: None,
            },
        ),
        ("integrations.json", Command::ListIntegrationHealth),
        ("events.json", Command::ListEvents { limit }),
    ];
    let mut written = Vec::new();
    for (name, command) in sections {
        // One unavailable section must not lose the rest: a report from a half-broken
        // installation is exactly the report worth having.
        let body = match send(session, command) {
            Ok(data) => json!({ "ok": true, "data": data }),
            Err(failure) => json!({
                "ok": false,
                "error": { "code": failure.code, "message": failure.message }
            }),
        };
        let path = destination.join(name);
        fs::write(&path, format!("{body}\n")).map_err(|error| {
            CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "write_failed",
                format!("cannot write {}: {error}", path.display()),
            )
        })?;
        written.push(name);
    }

    let manifest = json!({
        "kind": "reccursive-diagnostics",
        "cli_version": env!("CARGO_PKG_VERSION"),
        "api_version": reccursive_protocol::API_VERSION,
        "sections": written,
        "excluded": [
            "the local API authentication token",
            "the queue database",
            "captured package contents and patches",
        ],
        "redaction": "event and check output are scrubbed for credential-shaped text when stored",
    });
    let manifest_path = destination.join("manifest.json");
    fs::write(&manifest_path, format!("{manifest}\n")).map_err(|error| {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "write_failed",
            format!("cannot write {}: {error}", manifest_path.display()),
        )
    })?;

    if json_output {
        let value = json!({
            "ok": true,
            "data": {
                "type": "diagnostics_exported",
                "payload": {
                    "destination": destination.display().to_string(),
                    "sections": written,
                }
            }
        });
        return writeln!(out, "{value}").map_err(output_error);
    }
    writeln!(
        out,
        "Wrote diagnostics to {}\nThe queue is unchanged. Review the files before sharing them.",
        destination.display()
    )
    .map_err(output_error)
}

fn queued_state_label(state: QueuedUnitState) -> &'static str {
    match state {
        QueuedUnitState::Ready => "ready",
        QueuedUnitState::Scheduled => "scheduled",
        QueuedUnitState::Due => "due now",
        QueuedUnitState::Publishing => "publishing",
        QueuedUnitState::Blocked => "blocked",
        QueuedUnitState::Cancelled => "cancelled",
        QueuedUnitState::AvailableEarly => "available early",
        QueuedUnitState::AwaitingMerge => "awaiting your merge",
        QueuedUnitState::Published => "published",
    }
}

fn output_queue_summary(
    out: &mut impl Write,
    summary: &QueueSummaryView,
) -> Result<(), CliFailure> {
    if summary.repositories.is_empty() {
        return writeln!(out, "No repositories are enrolled.").map_err(output_error);
    }
    for repository in &summary.repositories {
        writeln!(
            out,
            "{} → {}{}",
            repository.repository_id,
            repository.target.as_str(),
            match &repository.paused {
                Some(reason) => format!("  [paused: {reason}]"),
                None => String::new(),
            }
        )
        .map_err(output_error)?;
        if repository.units.is_empty() {
            writeln!(out, "  nothing queued").map_err(output_error)?;
            continue;
        }
        output_table(
            out,
            &["UNIT", "STATE", "DUE", "WORK"],
            repository
                .units
                .iter()
                .map(|unit| {
                    vec![
                        unit.release_unit_id.to_string(),
                        queued_state_label(unit.state).to_owned(),
                        unit.selected_at_unix_ms
                            .map_or_else(|| "—".to_owned(), |at| at.to_string()),
                        unit.task_names.join(", "),
                    ]
                })
                .collect::<Vec<_>>(),
        )
        .map_err(output_error)?;
        // Anything that needs a person is called out under the table rather than truncated into a
        // column, because it is the only part of this view that asks for an action.
        for unit in &repository.units {
            if let Some(reason) = &unit.reason {
                writeln!(out, "  {} blocked: {reason}", unit.release_unit_id)
                    .map_err(output_error)?;
            }
            if let Some(pull_request) = &unit.pull_request {
                writeln!(
                    out,
                    "  {} pull request #{} {} — {}",
                    unit.release_unit_id,
                    pull_request.number,
                    if pull_request.merged_at_unix_ms.is_some() {
                        "was merged"
                    } else {
                        "is waiting for you to merge it"
                    },
                    pull_request.url,
                )
                .map_err(output_error)?;
            }
            for publication in &unit.published_to {
                writeln!(
                    out,
                    "  {} {} {} at {}",
                    unit.release_unit_id,
                    if publication.is_target {
                        "is on the target"
                    } else {
                        "is available early on"
                    },
                    publication.target.as_str(),
                    short_commit(&publication.commit),
                )
                .map_err(output_error)?;
            }
            if let Some(change) = &unit.last_schedule_change {
                writeln!(
                    out,
                    "  {} release time withdrawn: {change}",
                    unit.release_unit_id
                )
                .map_err(output_error)?;
            }
        }
    }
    Ok(())
}

fn output_schedule_history(
    out: &mut impl Write,
    changes: &[ScheduleChangeView],
) -> Result<(), CliFailure> {
    if changes.is_empty() {
        return writeln!(out, "No release time has been selected for this unit yet.")
            .map_err(output_error);
    }
    output_table(
        out,
        &["SELECTED", "WITHDRAWN", "REASON"],
        changes
            .iter()
            .map(|change| {
                vec![
                    change.selected_at_unix_ms.to_string(),
                    change
                        .withdrawn_at_unix_ms
                        .map_or_else(|| "—  (live)".to_owned(), |at| at.to_string()),
                    change.reason.clone().unwrap_or_else(|| "—".to_owned()),
                ]
            })
            .collect::<Vec<_>>(),
    )
    .map_err(output_error)
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
    // Where the work actually is. A task whose unit published to a development branch returns to
    // the queue awaiting its integration, so its status alone cannot say that the work is already
    // on a real branch that other people can see.
    for publication in &task.published_to {
        writeln!(
            out,
            "      {} {} at {}",
            if publication.is_target {
                "on target:"
            } else {
                "available:"
            },
            publication.target.as_str(),
            short_commit(&publication.commit),
        )
        .map_err(output_error)?;
    }
    Ok(())
}

/// Shortens a commit for display without ever inventing one.
fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
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
            "Service {service_version} · schema {schema_version} · {repository_count} {}",
            if repository_count == 1 {
                "repository"
            } else {
                "repositories"
            }
        ),
        ResponseData::RepositoryPolicyChanged { change } => {
            writeln!(
                out,
                "Updated {} to policy revision {}\nMode: {:?}\nTarget: {}\nDevelopment: {}\nIntegration: {:?}",
                change.repository_id,
                change.policy_revision.get(),
                change.publication_mode,
                change.target.as_str(),
                change
                    .development_target
                    .as_ref()
                    .map_or("none", |branch| branch.as_str()),
                change.target_integration,
            )
            .map_err(output_error)?;
            // A withdrawn time is the part a reader has to know about: their work has not been
            // cancelled, but the time they were told to expect is no longer the time.
            if !change.withdrawn.is_empty() {
                writeln!(
                    out,
                    "\nRelease times withdrawn, because they were chosen under the old policy.\n\
                     Each unit is scheduled again from the new one; nothing was published in \
                     between and nothing was duplicated:"
                )
                .map_err(output_error)?;
                for unit in &change.withdrawn {
                    writeln!(out, "  {unit}").map_err(output_error)?;
                }
            }
            Ok(())
        }
        ResponseData::RepositoryEnrolled { repository } => {
            writeln!(
                out,
                "Enrolled {}\nTarget: {}",
                repository.id,
                repository.target.as_str()
            )
        }
        ResponseData::RepositoryInitialized {
            repository,
            schedule_policy,
        } => writeln!(
            out,
            "Initialized {}\nTarget: {}\nSchedule policy revision: {}",
            repository.id,
            repository.target.as_str(),
            schedule_policy.revision.get(),
        ),
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
        ResponseData::PackageChanges { changes } => {
            return output_package_changes(out, &changes);
        }
        ResponseData::CheckResults { results } => {
            return output_check_results(out, &results);
        }
        ResponseData::QueueSummary { summary } => {
            return output_queue_summary(out, &summary);
        }
        ResponseData::ScheduleHistory { changes } => {
            return output_schedule_history(out, &changes);
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

/// Renders a plan for review, not just for counting.
///
/// Approving a plan means agreeing to what it will publish, so the review has to show the work
/// itself: every phase, every task, what each one waits for and at which milestone, and what it
/// claims will prove it is done. A summary of counts is not something anyone can approve.
fn output_plan(out: &mut impl Write, plan: &PlanView) -> io::Result<()> {
    let tasks: usize = plan.plan.phases.iter().map(|phase| phase.tasks.len()).sum();
    writeln!(
        out,
        "Feature: {}\nRevision: {} ({})\nTarget:  {}\nGoal:    {}\n{} phase(s), {tasks} task(s)",
        plan.plan.feature_id,
        plan.plan.revision.get(),
        if plan.plan.sealed {
            "sealed"
        } else {
            "draft — seal it before any work can start"
        },
        plan.plan.target.as_str(),
        plan.plan.goal,
        plan.plan.phases.len(),
    )?;
    for phase in &plan.plan.phases {
        writeln!(out, "\n{} ({})", phase.name, phase.id)?;
        for task in &phase.tasks {
            writeln!(out, "  {}\n    {}", task.name, task.id)?;
            for (prerequisite, milestone) in &task.dependencies {
                // The milestone is the whole point of a dependency here: waiting for a
                // prerequisite to be captured groups the work into one release, while waiting for
                // it to be published holds this task back until the target actually moved.
                writeln!(
                    out,
                    "    waits for {prerequisite} to be {}",
                    match milestone {
                        TargetMilestone::Captured => "captured",
                        TargetMilestone::DevelopmentAvailable =>
                            "available on the development branch",
                        TargetMilestone::TargetPublished => "published to the target",
                    }
                )?;
            }
            for check in &task.acceptance_checks {
                writeln!(out, "    done when: {}", check.description)?;
            }
        }
    }
    Ok(())
}

fn output_plan_history(out: &mut impl Write, plans: &[PlanView]) -> io::Result<()> {
    if plans.is_empty() {
        return writeln!(out, "No revisions found.");
    }
    output_table(
        out,
        &["FEATURE", "REVISION", "STATE", "GOAL"],
        plans
            .iter()
            .map(|plan| {
                vec![
                    plan.plan.feature_id.to_string(),
                    plan.plan.revision.get().to_string(),
                    if plan.plan.sealed {
                        "sealed".into()
                    } else {
                        "draft".into()
                    },
                    plan.plan.goal.clone(),
                ]
            })
            .collect(),
    )
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
    output_table(
        out,
        &["UNIT", "PACKAGE", "RELEASE TIME", "TIME ZONE"],
        slots
            .iter()
            .map(|slot| {
                vec![
                    slot.release_unit_id.to_string(),
                    slot.package_id.to_string(),
                    slot.selected_at_unix_ms.to_string(),
                    slot.timezone.clone(),
                ]
            })
            .collect(),
    )
}

fn output_integration_health(
    out: &mut impl Write,
    integrations: &[IntegrationHealthView],
) -> io::Result<()> {
    if integrations.is_empty() {
        return writeln!(out, "All integrations are healthy.");
    }
    output_table(
        out,
        &[
            "INTEGRATION",
            "SCOPE",
            "FAULT",
            "FAILURES",
            "NEXT ACTION",
            "DETAIL",
        ],
        integrations
            .iter()
            .map(|health| {
                vec![
                    format!("{:?}", health.integration),
                    health.scope.clone(),
                    format!("{:?}", health.fault),
                    health.consecutive_failures.to_string(),
                    match health.next_attempt_at_unix_ms {
                        Some(at) => format!("retry after {at}"),
                        // Deliberately explicit: this will not clear on its own.
                        None => "needs attention; no retry scheduled".to_owned(),
                    },
                    health.detail.clone(),
                ]
            })
            .collect(),
    )
}

fn output_due_units(out: &mut impl Write, units: &[DueUnitView]) -> io::Result<()> {
    if units.is_empty() {
        return writeln!(out, "No units are due for release.");
    }
    output_table(
        out,
        &["UNIT", "REPOSITORY", "PACKAGE", "DUE"],
        units
            .iter()
            .map(|unit| {
                vec![
                    unit.release_unit_id.to_string(),
                    unit.repository_id.to_string(),
                    unit.package_id.to_string(),
                    unit.selected_at_unix_ms.to_string(),
                ]
            })
            .collect(),
    )
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
    // A blocked attempt that leaves the reader with nothing to do is the failure this is here to
    // remove. Where no action is known, nothing is printed rather than something plausible.
    if let Some(action) = next_action_for_attempt(attempt) {
        writeln!(out, "\nNext:")?;
        for line in action.lines() {
            writeln!(out, "  {line}")?;
        }
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
    output_table(
        out,
        &["ATTEMPT", "STATUS", "PACKAGE", "FAILURE"],
        attempts
            .iter()
            .map(|attempt| {
                vec![
                    attempt.attempt_id.to_string(),
                    format!("{:?}", attempt.status),
                    format!("{}@{}", attempt.package_id, attempt.package_revision.get()),
                    attempt
                        .failure_classification
                        .clone()
                        .unwrap_or_else(|| "-".into()),
                ]
            })
            .collect(),
    )
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
    output_table(
        out,
        &["#", "SEVERITY", "KIND", "REQUEST", "MESSAGE"],
        events
            .iter()
            .map(|event| {
                vec![
                    event.sequence.to_string(),
                    match event.severity {
                        EventSeverityView::Debug => "DEBUG",
                        EventSeverityView::Info => "INFO",
                        EventSeverityView::Warning => "WARN",
                        EventSeverityView::Error => "ERROR",
                    }
                    .into(),
                    event.kind.clone(),
                    event
                        .request_id
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_owned()),
                    event.message.clone(),
                ]
            })
            .collect(),
    )
}

fn output_repositories(out: &mut impl Write, repositories: &[RepositoryView]) -> io::Result<()> {
    if repositories.is_empty() {
        return writeln!(out, "No repositories are registered.");
    }
    output_table(
        out,
        &["REPOSITORY", "TARGET", "MODE", "CHECKOUT"],
        repositories
            .iter()
            .map(|repository| {
                vec![
                    repository.id.to_string(),
                    repository.target.as_str().into(),
                    format!("{:?}", repository.publication_mode),
                    repository.checkout_path.clone(),
                ]
            })
            .collect(),
    )
}

const MAX_TABLE_CELL_WIDTH: usize = 48;

fn output_table(out: &mut impl Write, headings: &[&str], rows: Vec<Vec<String>>) -> io::Result<()> {
    debug_assert!(rows.iter().all(|row| row.len() == headings.len()));
    let rows = rows
        .into_iter()
        .map(|row| row.into_iter().map(table_cell).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let widths = headings
        .iter()
        .enumerate()
        .map(|(index, heading)| {
            rows.iter()
                .map(|row| row[index].chars().count())
                .max()
                .unwrap_or_default()
                .max(heading.chars().count())
        })
        .collect::<Vec<_>>();

    output_table_row(out, headings.iter().copied(), &widths)?;
    let separators = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>();
    output_table_row(out, separators.iter().map(String::as_str), &widths)?;
    for row in &rows {
        output_table_row(out, row.iter().map(String::as_str), &widths)?;
    }
    Ok(())
}

fn output_table_row<'a>(
    out: &mut impl Write,
    cells: impl IntoIterator<Item = &'a str>,
    widths: &[usize],
) -> io::Result<()> {
    for (index, (cell, width)) in cells.into_iter().zip(widths).enumerate() {
        if index > 0 {
            write!(out, " | ")?;
        }
        write!(out, "{cell:<width$}")?;
    }
    writeln!(out)
}

fn table_cell(value: String) -> String {
    let value = value.replace(['\n', '\r'], " ");
    let mut characters = value.chars();
    let shortened = characters
        .by_ref()
        .take(MAX_TABLE_CELL_WIDTH)
        .collect::<String>();
    if characters.next().is_some() {
        format!(
            "{}…",
            shortened
                .chars()
                .take(MAX_TABLE_CELL_WIDTH - 1)
                .collect::<String>()
        )
    } else {
        shortened
    }
}

/// Everything `setup` was asked for on the command line.
struct SetupArguments {
    path: PathBuf,
    remote: Option<String>,
    target: String,
    mode: PublicationModeArgument,
    development_target: Option<String>,
    schedule: Option<PathBuf>,
    no_schedule: bool,
    timezone: Option<String>,
    install_service: bool,
    non_interactive: bool,
}

fn setup_refusal(refusal: &setup::SetupRefusal) -> CliFailure {
    let code = match refusal {
        setup::SetupRefusal::NotATerminal => "not_a_terminal",
        setup::SetupRefusal::MissingRemote => "remote_missing",
        setup::SetupRefusal::MissingDevelopmentTarget => "development_target_missing",
        setup::SetupRefusal::MissingIdentity { .. } => "identity_missing",
        setup::SetupRefusal::NotARepository { .. } => "invalid_repository",
    };
    // Usage for a flag the caller can add, action-required for state on their machine.
    let exit = match refusal {
        setup::SetupRefusal::NotATerminal
        | setup::SetupRefusal::MissingRemote
        | setup::SetupRefusal::MissingDevelopmentTarget => EXIT_USAGE,
        setup::SetupRefusal::MissingIdentity { .. }
        | setup::SetupRefusal::NotARepository { .. } => EXIT_ACTION_REQUIRED,
    };
    CliFailure::new(exit, code, refusal.message())
}

/// Runs first-run setup: decide everything, then act, then say what stands.
///
/// Nothing is sent until every question is answered, so a refusal leaves the installation exactly
/// as it was rather than half-configured.
fn run_setup(
    session: &Session,
    arguments: SetupArguments,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    let interactive = setup::interaction_mode(arguments.non_interactive)
        .map_err(|refusal| setup_refusal(&refusal))?;
    let plan = decide_setup(session, &arguments, interactive, out)?;
    if interactive {
        let mut input = io::stdin().lock();
        writeln!(out, "\nAbout to:").map_err(output_error)?;
        writeln!(out, "  enroll   {}", plan.repository.display()).map_err(output_error)?;
        writeln!(out, "  publish  {} on {}", plan.target, plan.remote).map_err(output_error)?;
        match &plan.schedule {
            setup::ScheduleChoice::Default { timezone } => {
                writeln!(out, "  schedule weekday releases in {timezone}")
            }
            setup::ScheduleChoice::File(path) => {
                writeln!(out, "  schedule from {}", path.display())
            }
            setup::ScheduleChoice::None => writeln!(out, "  schedule nothing yet"),
        }
        .map_err(output_error)?;
        if plan.install_service {
            writeln!(out, "  install  the background service").map_err(output_error)?;
        }
        if !setup::confirm(out, &mut input, "\nProceed?", true).map_err(output_error)? {
            return Err(CliFailure::new(
                EXIT_USAGE,
                "cancelled",
                "setup was cancelled; nothing was changed",
            ));
        }
    }
    apply_setup(session, &plan, json_output, out)
}

/// Resolves every value setup needs, asking only where it may and refusing where it cannot.
fn decide_setup(
    session: &Session,
    arguments: &SetupArguments,
    interactive: bool,
    out: &mut impl Write,
) -> Result<setup::SetupPlan, CliFailure> {
    let requested = fs::canonicalize(&arguments.path).map_err(|_| {
        setup_refusal(&setup::SetupRefusal::NotARepository {
            path: arguments.path.clone(),
        })
    })?;
    let checkout = git_repository_root(&requested)
        .map_err(|_| setup_refusal(&setup::SetupRefusal::NotARepository { path: requested }))?;

    // Checked before anything else is asked. A checkout with no identity can be enrolled and
    // scheduled and will still never publish, because the release pass skips a unit it cannot
    // attribute — and it does so without an error anyone sees.
    if setup::checkout_identity(&checkout).is_none() {
        return Err(setup_refusal(&setup::SetupRefusal::MissingIdentity {
            checkout,
        }));
    }

    let discovered_remote = git_stdout(&checkout, &["remote", "get-url", "origin"]).ok();
    let remote = match arguments.remote.clone().or(discovered_remote) {
        Some(remote) => remote,
        None => return Err(setup_refusal(&setup::SetupRefusal::MissingRemote)),
    };

    let mut target = arguments.target.clone();
    let mut development_target = arguments.development_target.clone();
    let immediate = matches!(arguments.mode, PublicationModeArgument::Immediate);
    let mut install_service = arguments.install_service;
    let mut schedule = match (&arguments.schedule, arguments.no_schedule) {
        (Some(path), _) => setup::ScheduleChoice::File(path.clone()),
        (None, true) => setup::ScheduleChoice::None,
        (None, false) => setup::ScheduleChoice::Default {
            timezone: arguments
                .timezone
                .clone()
                .unwrap_or_else(setup::local_timezone),
        },
    };

    if interactive {
        let mut input = io::stdin().lock();
        writeln!(out, "Setting up {}", checkout.display()).map_err(output_error)?;
        writeln!(out, "Publishing to {remote}\n").map_err(output_error)?;
        target = setup::ask(out, &mut input, "Target branch", &target).map_err(output_error)?;
        if immediate {
            let suggested = development_target.clone().unwrap_or_default();
            let answer = setup::ask(out, &mut input, "Development branch", &suggested)
                .map_err(output_error)?;
            development_target = (!answer.is_empty()).then_some(answer);
        }
        if matches!(schedule, setup::ScheduleChoice::Default { .. }) {
            let timezone = match &schedule {
                setup::ScheduleChoice::Default { timezone } => timezone.clone(),
                _ => setup::local_timezone(),
            };
            let timezone = setup::ask(out, &mut input, "Time zone for releases", &timezone)
                .map_err(output_error)?;
            schedule = if setup::confirm(
                out,
                &mut input,
                "Release on weekday afternoons, up to three times a day",
                true,
            )
            .map_err(output_error)?
            {
                setup::ScheduleChoice::Default { timezone }
            } else {
                setup::ScheduleChoice::None
            };
        }
        install_service = setup::confirm(
            out,
            &mut input,
            "Install the background service so releases happen with the terminal closed",
            true,
        )
        .map_err(output_error)?;
    }

    if immediate && development_target.is_none() {
        return Err(setup_refusal(
            &setup::SetupRefusal::MissingDevelopmentTarget,
        ));
    }
    let _ = session;
    Ok(setup::SetupPlan {
        repository: checkout,
        remote,
        target,
        development_target,
        immediate,
        schedule,
        install_service,
    })
}

/// Carries out a decided plan, reporting exactly how far it got.
fn apply_setup(
    session: &Session,
    plan: &setup::SetupPlan,
    json_output: bool,
    out: &mut impl Write,
) -> Result<(), CliFailure> {
    let mut progress = setup::SetupProgress::default();
    let outcome = apply_setup_steps(session, plan, &mut progress);
    let repository_id = progress.enrolled.clone().unwrap_or_default();
    match outcome {
        Ok(()) => {
            if json_output {
                let value = json!({
                    "ok": true,
                    "data": {
                        "type": "setup_complete",
                        "payload": {
                            "repository_id": repository_id,
                            "target": plan.target,
                            "schedule_activated": progress.policy_activated,
                            "service_installed": progress.service_installed,
                        }
                    }
                });
                writeln!(out, "{value}").map_err(output_error)
            } else {
                writeln!(out, "\nReady. {repository_id} publishes to {}", plan.target)
                    .map_err(output_error)?;
                if progress.policy_activated {
                    writeln!(
                        out,
                        "A schedule is active; nothing publishes until work is submitted."
                    )
                    .map_err(output_error)?;
                }
                writeln!(
                    out,
                    "\nNext:\n  reccursive status\n  reccursive diagnose {repository_id}"
                )
                .map_err(output_error)
            }
        }
        Err(failure) => {
            // A later step failing does not undo an earlier one. Enrollment is atomic by itself,
            // and quietly reversing a repository the user may already be using would be worse than
            // saying plainly what stands and what is left.
            let remaining = progress.remaining_advice(plan);
            let mut message = failure.message.clone();
            if progress.enrolled.is_some() {
                message.push_str(&format!(
                    "\n{repository_id} is enrolled and stays enrolled."
                ));
            }
            if !remaining.is_empty() {
                message.push_str("\nFinish with:");
                for line in remaining {
                    message.push_str(&format!("\n  {line}"));
                }
            }
            Err(CliFailure::new(failure.exit_code, failure.code, message))
        }
    }
}

fn apply_setup_steps(
    session: &Session,
    plan: &setup::SetupPlan,
    progress: &mut setup::SetupProgress,
) -> Result<(), CliFailure> {
    let mode = if plan.immediate {
        PublicationModeArgument::Immediate
    } else {
        PublicationModeArgument::Scheduled
    };
    let request = enrollment_request(
        plan.repository.clone(),
        Some(plan.remote.clone()),
        plan.target.clone(),
        mode,
        plan.development_target.clone(),
        // The guided setup enrols by direct push. Choosing the pull-request strategy means
        // creating a token first, which is a decision to make deliberately rather than inside a
        // wizard that is otherwise about getting started quickly.
        TargetIntegrationArgument::DirectPush,
    )?;
    let enrolled = send(session, Command::EnrollRepository(request))?;
    let repository_id = match enrolled {
        ResponseData::RepositoryEnrolled { repository } => repository.id,
        _ => {
            return Err(CliFailure::new(
                EXIT_INTERNAL,
                "unexpected_response",
                "enrollment returned an unexpected result",
            ));
        }
    };
    progress.enrolled = Some(repository_id.to_string());

    match &plan.schedule {
        setup::ScheduleChoice::None => {}
        setup::ScheduleChoice::File(path) => {
            let policy = load_schedule_policy(path)?;
            send(
                session,
                Command::SetSchedulePolicy(SetSchedulePolicyRequest {
                    repository_id,
                    policy,
                }),
            )?;
            progress.policy_activated = true;
        }
        setup::ScheduleChoice::Default { timezone } => {
            let document = setup::default_policy_document(timezone);
            let policy: SchedulePolicy = serde_json::from_str(&document).map_err(|error| {
                CliFailure::new(
                    EXIT_ACTION_REQUIRED,
                    "invalid_timezone",
                    format!("the default schedule is not valid for time zone {timezone}: {error}"),
                )
            })?;
            send(
                session,
                Command::SetSchedulePolicy(SetSchedulePolicyRequest {
                    repository_id,
                    policy,
                }),
            )?;
            progress.policy_activated = true;
        }
    }

    if plan.install_service {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "home_unknown",
                "HOME is not set, so the user's LaunchAgents directory cannot be located",
            )
        })?;
        let installation =
            ServiceInstallation::describe(default_daemon_program()?, &session.state_dir, &home);
        installation.install().map_err(install_failure)?;
        progress.service_installed = true;
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
    fn tables_have_headings_and_bound_untrusted_cell_width() {
        let mut output = Vec::new();
        output_table(
            &mut output,
            &["NAME", "DETAIL"],
            vec![vec!["first\nline".into(), "x".repeat(60)]],
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("NAME"));
        assert!(lines[2].contains("first line"));
        assert!(lines[2].ends_with('…'));
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
