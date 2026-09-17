//! Scheduling the changes already sitting in the user's own checkout.
//!
//! The rest of the product expects work to arrive through a plan, a daemon-owned workspace, and a
//! capture. That sequence is right for an agent and far too much for a person who has just edited
//! three files. This module builds the same records on their behalf, from what `git status` says
//! is in their working tree, and never shows them any of it.
//!
//! What it must not do is point the release worker at a mutable working tree. The snapshot is
//! taken now, into the daemon's own storage, so editing afterwards cannot change what was
//! scheduled — and the checkout itself is only ever read.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use inquire::Text;
use reccursive_protocol::{
    Command, CreateWorkspaceRequest, FeaturePlan, PlanPhase, PlanTask, RepositoryView,
    ResponseData, Revision,
};

use crate::{CliFailure, EXIT_ACTION_REQUIRED, Session, human_time, send};

/// One entry from the user's working tree.
pub struct Change {
    /// Git's own two-letter status, rendered for a person: `M`, `A`, `D`, `?`.
    pub mark: char,
    pub path: String,
}

/// Reads what has changed in a checkout, without touching it.
///
/// `--untracked-files=all` so a new file counts as work; ignored files are excluded because git
/// excludes them, which is the behaviour a `.gitignore` is for.
pub fn changes(checkout: &Path) -> Result<Vec<Change>, CliFailure> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .map_err(|error| {
            CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "git_unavailable",
                format!("could not read the checkout: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "git_failed",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut changes = Vec::new();
    let mut records = text.split('\0').filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        // A rename's second record is the old path, and the capture layer refuses renames as
        // ambiguous anyway. Consuming it here keeps the list honest rather than listing a path
        // that is not a change on its own.
        if status.contains('R') || status.contains('C') {
            let _ = records.next();
            return Err(CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "rename_unsupported",
                format!(
                    "{} is a rename, which this flow cannot capture yet. Commit or unstage it, \
                     or use the advanced capture commands.",
                    record[3..].trim()
                ),
            ));
        }
        // Git writes the index state then the working-tree state. Whichever of the two is not a
        // space is the one that describes the change a person made.
        let mut marks = status.chars();
        let index = marks.next().unwrap_or(' ');
        let worktree = marks.next().unwrap_or(' ');
        let mark = match (index, worktree) {
            ('?', _) => '?',
            (staged, ' ') => staged,
            (_, unstaged) => unstaged,
        };
        changes.push(Change {
            mark,
            path: record[3..].to_owned(),
        });
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// Finds the enrolled repository the current directory belongs to.
///
/// By path rather than by identifier, because a person standing in their project should not have
/// to know one. Compares canonical paths so a symlinked or relative route to the same checkout
/// still matches.
pub fn enrolled_here(
    session: &Session,
    working_directory: &Path,
) -> Result<Option<(RepositoryView, PathBuf)>, CliFailure> {
    let Some(root) = git_root(working_directory) else {
        return Ok(None);
    };
    let ResponseData::Repositories { repositories } = send(session, Command::ListRepositories)?
    else {
        return Ok(None);
    };
    let canonical = std::fs::canonicalize(&root).unwrap_or(root.clone());
    Ok(repositories
        .into_iter()
        .find(|repository| {
            std::fs::canonicalize(&repository.checkout_path)
                .map(|enrolled| enrolled == canonical)
                .unwrap_or(false)
        })
        .map(|repository| (repository, root)))
}

fn git_root(from: &Path) -> Option<PathBuf> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(from)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Asks what the change should be called, which is the only thing a person has to name.
pub fn ask_name() -> Result<String, CliFailure> {
    let name = match Text::new("What should this change be called?")
        .with_help_message("this becomes the commit subject")
        .prompt()
    {
        Ok(value) => value.trim().to_owned(),
        Err(inquire::InquireError::OperationCanceled)
        | Err(inquire::InquireError::OperationInterrupted) => {
            return Err(CliFailure::new(
                crate::EXIT_SUCCESS,
                "cancelled",
                "Nothing was scheduled.",
            ));
        }
        Err(error) => {
            return Err(CliFailure::new(
                EXIT_ACTION_REQUIRED,
                "prompt_failed",
                error.to_string(),
            ));
        }
    };
    if name.is_empty() {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "name_required",
            "A change needs a name, because it becomes the commit subject.",
        ));
    }
    Ok(name)
}

/// Turns the approved checkout changes into an immutable package the release engine can use.
///
/// Builds the plan, workspace and capture the engine requires, all from one name and one list of
/// paths. The plan is sealed on import because nothing will ever edit it — it exists to give the
/// work an identity, not to be maintained.
///
/// Returns the feature, revision and package that now hold a frozen copy of those changes.
pub fn capture(
    session: &Session,
    repository: &RepositoryView,
    name: &str,
    changes: &[Change],
) -> Result<
    (
        reccursive_protocol::FeatureId,
        Revision,
        reccursive_protocol::PackageId,
    ),
    CliFailure,
> {
    let feature_id = reccursive_protocol::FeatureId::new();
    let task_id = reccursive_protocol::TaskId::new();
    let plan = FeaturePlan {
        schema_version: reccursive_protocol::PLAN_SCHEMA_VERSION,
        feature_id,
        revision: Revision::FIRST,
        repository_id: repository.id,
        goal: name.to_owned(),
        target: repository.target.clone(),
        // Sealed immediately: a workspace may only be built against a revision whose scope cannot
        // change, and this one is complete the moment it is written.
        sealed: true,
        phases: vec![PlanPhase {
            id: "delivery".to_owned(),
            name: "Delivery".to_owned(),
            tasks: vec![PlanTask {
                id: task_id,
                name: name.to_owned(),
                dependencies: Default::default(),
                acceptance_checks: vec![reccursive_protocol::AcceptanceCheck {
                    id: "captured".to_owned(),
                    description: "the change captured from the checkout is published".to_owned(),
                }],
            }],
        }],
    };
    send(session, Command::ImportPlan { plan })?;

    // The workspace is cloned at HEAD and each approved path copied into it. That copy is what
    // makes later edits to the checkout irrelevant to what was scheduled.
    send(
        session,
        Command::CreateWorkspace(CreateWorkspaceRequest {
            feature_id,
            revision: Some(Revision::FIRST),
            prerequisites: changes.iter().map(|change| change.path.clone()).collect(),
        }),
    )?;

    let ResponseData::PackageCaptured { package } = send(
        session,
        Command::CapturePackage(reccursive_protocol::CapturePackageRequest {
            feature_id,
            plan_revision: Revision::FIRST,
            task_ids: [task_id].into_iter().collect(),
        }),
    )?
    else {
        return Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not capture the change",
        ));
    };
    Ok((feature_id, Revision::FIRST, package.package_id))
}

/// Shows what is about to be captured, before anything is.
pub fn show(stdout: &mut impl Write, changes: &[Change]) -> Result<(), CliFailure> {
    writeln!(stdout, "\nChanges in your checkout\n").map_err(crate::output_error)?;
    for change in changes {
        writeln!(stdout, "  {} {}", change.mark, change.path).map_err(crate::output_error)?;
    }
    writeln!(stdout).map_err(crate::output_error)
}

/// The confirmation a person reads after their own working tree has been scheduled.
pub fn confirmation(
    stdout: &mut impl Write,
    name: &str,
    file_count: usize,
    delivery: &str,
    selected_at_unix_ms: i64,
) -> Result<(), CliFailure> {
    writeln!(
        stdout,
        "\n{}\n\n{name}\n{}\n\n{} · {delivery}\n\nReccursive will handle it automatically.",
        crate::paint(crate::ansi::GREEN, "✓ Scheduled"),
        human_time(selected_at_unix_ms),
        if file_count == 1 {
            "1 file".to_owned()
        } else {
            format!("{file_count} files")
        },
    )
    .map_err(crate::output_error)
}
