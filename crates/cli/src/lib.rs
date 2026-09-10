use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use clap::{Parser, Subcommand, ValueEnum};
use reccursive_protocol::{
    ApiError, ApiErrorCode, CapturePackageRequest, Command, CreateWorkspaceRequest,
    EnrollRepositoryRequest, EventSeverityView, EventView, FeatureId, FeaturePlan, LocalClient,
    PackageId, PackageView, PlanView, PublicationMode, QueueAuditView, QueueExportView,
    RepositoryView, ResponseData, Revision, TargetRef, TaskId, WorkspaceView,
};
use serde::Serialize;
use serde_json::json;

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
    about = "Schedule verified software changes safely"
)]
struct Cli {
    /// Override the local service state directory.
    #[arg(long, global = true, value_name = "PATH")]
    state_dir: Option<PathBuf>,

    /// Emit stable JSON without interactive formatting.
    #[arg(long, global = true)]
    json: bool,

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
    /// Inspect package recovery state or create a portable queue backup.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Show service and queue summary.
    Status,
    /// Check local prerequisites and service connectivity.
    Doctor,
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

#[derive(Debug)]
struct ClientPaths {
    state_dir: PathBuf,
    socket: PathBuf,
    auth_token: PathBuf,
}

impl ClientPaths {
    fn new(state_dir: PathBuf) -> Self {
        Self {
            socket: state_dir.join("service.sock"),
            auth_token: state_dir.join("auth.token"),
            state_dir,
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
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = write!(stderr, "{error}");
            return EXIT_USAGE;
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

fn execute(cli: Cli, stdout: &mut impl Write) -> Result<(), CliFailure> {
    let state_dir = resolve_state_dir(cli.state_dir)?;
    let paths = ClientPaths::new(state_dir);
    match cli.command {
        TopLevelCommand::Doctor => doctor(&paths, cli.json, stdout),
        TopLevelCommand::Status => {
            let data = send(&paths, Command::Status)?;
            output_data(data, cli.json, stdout)
        }
        TopLevelCommand::Logs { limit } => {
            let data = send(&paths, Command::ListEvents { limit })?;
            output_data(data, cli.json, stdout)
        }
        TopLevelCommand::Plan { command } => match command {
            PlanCommand::Import { file } => {
                let plan = load_plan(&file)?;
                let data = send(&paths, Command::ImportPlan { plan })?;
                output_data(data, cli.json, stdout)
            }
            PlanCommand::Show {
                feature_id,
                revision,
            } => {
                let data = send(
                    &paths,
                    Command::GetPlan {
                        feature_id,
                        revision,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
            PlanCommand::History { feature_id } => {
                let data = send(&paths, Command::PlanHistory { feature_id })?;
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
                    &paths,
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
                    &paths,
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
                    &paths,
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
                    &paths,
                    Command::GetPackage {
                        package_id,
                        revision,
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Queue { command } => match command {
            QueueCommand::Audit => {
                let data = send(&paths, Command::AuditQueue)?;
                output_data(data, cli.json, stdout)
            }
            QueueCommand::Export { destination } => {
                let data = send(
                    &paths,
                    Command::ExportQueue {
                        destination: destination.to_string_lossy().into_owned(),
                    },
                )?;
                output_data(data, cli.json, stdout)
            }
        },
        TopLevelCommand::Repository { command } => match command {
            RepositoryCommand::List => {
                let data = send(&paths, Command::ListRepositories)?;
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
                let data = send(&paths, Command::EnrollRepository(request))?;
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

fn parse_revision(value: &str) -> Result<Revision, String> {
    let value = value
        .parse::<u32>()
        .map_err(|_| "revision must be a positive integer".to_owned())?;
    Revision::new(value).map_err(|error| error.to_string())
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

fn send(paths: &ClientPaths, command: Command) -> Result<ResponseData, CliFailure> {
    let client = LocalClient::from_token_file(&paths.auth_token).map_err(|error| {
        CliFailure::new(
            EXIT_UNAVAILABLE,
            "service_unavailable",
            format!("cannot read local service credentials: {error}"),
        )
    })?;
    let response = client.send(&paths.socket, command).map_err(|error| {
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

fn doctor(paths: &ClientPaths, json_output: bool, out: &mut impl Write) -> Result<(), CliFailure> {
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

    let state_ready = fs::metadata(&paths.state_dir)
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
            format!("{} is owner-only", paths.state_dir.display())
        } else {
            format!("{} is missing or not owner-only", paths.state_dir.display())
        },
    });

    let service_result = send(paths, Command::Ping);
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
