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

use inquire::{MultiSelect, Text};
use reccursive_protocol::{
    Command, CreateWorkspaceRequest, FeaturePlan, PlanPhase, PlanTask, RepositoryView,
    ResponseData, Revision,
};

use crate::{CliFailure, EXIT_ACTION_REQUIRED, Session, send};

/// One entry from the user's working tree.
pub struct Change {
    /// Git's own two-letter status, rendered for a person: `M`, `A`, `D`, `?`.
    pub mark: char,
    pub path: String,
    /// Lines added and removed relative to `HEAD`, when that is cheap to know.
    ///
    /// `None` for an untracked file (there is no tracked version to diff against) or a binary one
    /// (git reports no line counts for those). Either way the person is told why, not left to
    /// wonder whether the number was simply omitted.
    pub diffstat: Option<(u64, u64)>,
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
            diffstat: None,
        });
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    let stats = diffstats(checkout);
    for change in &mut changes {
        change.diffstat = stats.get(&change.path).copied();
    }
    Ok(changes)
}

/// Lines added and removed per tracked path, relative to `HEAD`.
///
/// One `git diff` call for every tracked change at once — comparing the working tree straight to
/// `HEAD` reports a path's total delta whether it is staged, unstaged, or both, which is exactly
/// what `mark` already collapses those two states into. Untracked files never appear here, since
/// there is no tracked blob for them to diff against; the caller reads that as "no stat available"
/// rather than "zero changes".
fn diffstats(checkout: &Path) -> std::collections::HashMap<String, (u64, u64)> {
    let mut stats = std::collections::HashMap::new();
    let Ok(output) = ProcessCommand::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["diff", "--numstat", "HEAD"])
        .output()
    else {
        return stats;
    };
    if !output.status.success() {
        return stats;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // A binary file reports `-` for both counts, which is not a line count at all.
        if let (Ok(added), Ok(removed)) = (added.parse(), removed.parse()) {
            stats.insert(path.to_owned(), (added, removed));
        }
    }
    stats
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

/// The `+X -Y` (or `untracked`) label shown next to one file, without its mark or path.
fn stat_label(change: &Change) -> String {
    match change.diffstat {
        Some((added, 0)) => format!("+{added}"),
        Some((0, removed)) => format!("-{removed}"),
        Some((added, removed)) => format!("+{added} -{removed}"),
        None if change.mark == '?' => "untracked".to_owned(),
        None => String::new(),
    }
}

/// One line per file for a list or a selector: mark, path, and its diffstat, columns aligned to
/// the longest path so the stats line up regardless of how many files are shown.
fn render_changes_aligned(changes: &[Change]) -> Vec<String> {
    let width = changes
        .iter()
        .map(|change| change.path.len())
        .max()
        .unwrap_or(0);
    changes
        .iter()
        .map(|change| {
            format!(
                "{} {:<width$}  {}",
                change.mark,
                change.path,
                stat_label(change)
            )
        })
        .collect()
}

/// Total lines added and removed across every listed change, for the header above a selector.
fn total_diffstat(changes: &[Change]) -> (u64, u64) {
    changes.iter().fold((0, 0), |(added, removed), change| {
        let (a, r) = change.diffstat.unwrap_or((0, 0));
        (added + a, removed + r)
    })
}

/// The line above a file selector: how many changes there are and their combined diffstat.
pub fn summary_line(changes: &[Change]) -> String {
    let (added, removed) = total_diffstat(changes);
    let count = if changes.len() == 1 {
        "1 change".to_owned()
    } else {
        format!("{} changes", changes.len())
    };
    if added == 0 && removed == 0 {
        count
    } else {
        format!("{count} · +{added} \u{2212}{removed}")
    }
}

/// Lets the person choose which of the remaining changes belong in this batch.
///
/// One checkout path can belong to at most one batch in a session, so this is always offered
/// against whatever has not already been claimed by an earlier batch — a file picked here simply
/// stops being offered on the next round.
pub fn select_files(changes: &[Change]) -> Result<Vec<usize>, CliFailure> {
    let labels = render_changes_aligned(changes);
    let chosen = match MultiSelect::new("Which files belong in this release?", labels.clone())
        .with_help_message("space to toggle, enter to confirm")
        .prompt()
    {
        Ok(chosen) => chosen,
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
    let mut indices: Vec<usize> = chosen
        .into_iter()
        .filter_map(|label| labels.iter().position(|candidate| *candidate == label))
        .collect();
    indices.sort_unstable();
    indices.dedup();
    Ok(indices)
}

/// Splits `changes` into the ones at `indices` and everything else, preserving order in both.
pub fn partition(changes: Vec<Change>, indices: &[usize]) -> (Vec<Change>, Vec<Change>) {
    let mut chosen = Vec::with_capacity(indices.len());
    let mut rest = Vec::with_capacity(changes.len().saturating_sub(indices.len()));
    for (index, change) in changes.into_iter().enumerate() {
        if indices.contains(&index) {
            chosen.push(change);
        } else {
            rest.push(change);
        }
    }
    (chosen, rest)
}

/// One batch built during a checkout scheduling session: which files it owns, what it is called,
/// when it should go out, and the daemon identity that already holds its frozen snapshot.
pub struct Batch {
    pub title: String,
    pub changes: Vec<Change>,
    pub instant_unix_ms: i64,
    pub feature_id: reccursive_protocol::FeatureId,
    pub plan_revision: Revision,
    pub package_id: reccursive_protocol::PackageId,
}

fn file_count_label(count: usize) -> String {
    if count == 1 {
        "1 file".to_owned()
    } else {
        format!("{count} files")
    }
}

/// Renders in the given zone, never this machine's own — see [`review_plan`]'s doc comment for
/// why that distinction is the whole point of this function existing.
fn day_heading(unix_ms: i64, zone: &str) -> String {
    jiff::Timestamp::from_millisecond(unix_ms)
        .and_then(|timestamp| timestamp.in_tz(zone))
        .map(|zoned| zoned.strftime("%a · %b %-d").to_string().to_uppercase())
        .unwrap_or_else(|_| unix_ms.to_string())
}

/// Renders in the given zone, never this machine's own — see [`review_plan`]'s doc comment.
fn clock(unix_ms: i64, zone: &str) -> String {
    jiff::Timestamp::from_millisecond(unix_ms)
        .and_then(|timestamp| timestamp.in_tz(zone))
        .map(|zoned| zoned.strftime("%-I:%M %p").to_string())
        .unwrap_or_else(|_| unix_ms.to_string())
}

/// Shows the whole plan, every batch grouped by day plus anything left unscheduled, before any of
/// it is persisted. This is the one screen a person approves the entire session from.
///
/// `zone` is the repository's schedule-policy zone — the same one every batch's time was entered
/// and validated in. Rendering here in anything else (this machine's own zone, in particular)
/// would show a different clock time than the one the person just typed and had accepted, which
/// is exactly the four-hour-shift class of bug this function, `clock`, `day_heading` and
/// `confirmation_multi` all exist to rule out. There is deliberately no other timezone source
/// consulted here.
pub fn review_plan(
    stdout: &mut impl Write,
    batches: &[Batch],
    unscheduled: &[Change],
    delivery: &str,
    zone: &str,
) -> Result<(), CliFailure> {
    const RULE: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";
    writeln!(stdout, "\nSchedule plan\n{RULE}").map_err(crate::output_error)?;
    let mut ordered: Vec<&Batch> = batches.iter().collect();
    ordered.sort_by_key(|batch| batch.instant_unix_ms);
    let mut last_day: Option<String> = None;
    for batch in ordered {
        let day = day_heading(batch.instant_unix_ms, zone);
        if last_day.as_deref() != Some(day.as_str()) {
            writeln!(stdout, "{day}").map_err(crate::output_error)?;
            last_day = Some(day);
        }
        writeln!(
            stdout,
            "{}\n{}\n{} · {delivery}",
            clock(batch.instant_unix_ms, zone),
            batch.title,
            file_count_label(batch.changes.len()),
        )
        .map_err(crate::output_error)?;
        for change in &batch.changes {
            writeln!(stdout, "  {}", change.path).map_err(crate::output_error)?;
        }
    }
    if !unscheduled.is_empty() {
        writeln!(stdout, "{RULE}\nUnscheduled").map_err(crate::output_error)?;
        for change in unscheduled {
            writeln!(stdout, "  {}", change.path).map_err(crate::output_error)?;
        }
    }
    writeln!(stdout, "{RULE}\n").map_err(crate::output_error)
}

/// The confirmation a person reads after a whole multi-batch plan has been scheduled at once.
pub fn confirmation_multi(
    stdout: &mut impl Write,
    batches: &[Batch],
    slots: &[reccursive_protocol::ScheduleSlotView],
    delivery: &str,
) -> Result<(), CliFailure> {
    let heading = if batches.len() == 1 {
        "1 release scheduled".to_owned()
    } else {
        format!("{} releases scheduled", batches.len())
    };
    writeln!(
        stdout,
        "\n{} {heading}\n",
        crate::paint(crate::ansi::GREEN, "✓")
    )
    .map_err(crate::output_error)?;
    let mut ordered: Vec<(&Batch, &reccursive_protocol::ScheduleSlotView)> =
        batches.iter().zip(slots).collect();
    ordered.sort_by_key(|(batch, _)| batch.instant_unix_ms);
    for (batch, slot) in ordered {
        // The slot's own timezone, not the repository's current policy zone: this is the zone
        // that was actually in effect when the instant was chosen and persisted, which is the
        // authoritative record of what the displayed clock time means. A policy change between
        // scheduling and this confirmation must not change what is shown here.
        writeln!(
            stdout,
            "{}\n{}\n{} · {delivery}\n",
            batch.title,
            crate::human_time_in(slot.selected_at_unix_ms, &slot.timezone),
            file_count_label(batch.changes.len()),
        )
        .map_err(crate::output_error)?;
    }
    writeln!(stdout, "Reccursive will handle them automatically.").map_err(crate::output_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(mark: char, path: &str, diffstat: Option<(u64, u64)>) -> Change {
        Change {
            mark,
            path: path.to_owned(),
            diffstat,
        }
    }

    #[test]
    fn the_review_screens_clock_renders_in_the_given_zone_not_this_machines_own() {
        // The regression this guards: `clock`/`day_heading` used to render in
        // `jiff::tz::TimeZone::system()`, so a time entered against a repository whose policy zone
        // differs from the machine running the CLI would show a different hour on the review
        // screen than the one just typed and validated.
        let instant = crate::interactive::to_instant("2026-09-17", "04:29", "America/New_York")
            .expect("a valid civil time");
        assert_eq!(clock(instant, "America/New_York"), "4:29 AM");
        assert_eq!(day_heading(instant, "America/New_York"), "THU · SEP 17");
        // The same instant, read in a different zone, is a different wall-clock time — proving
        // the function actually consults its `zone` argument rather than always agreeing by
        // coincidence.
        assert_eq!(clock(instant, "UTC"), "8:29 AM");
    }

    #[test]
    fn a_tracked_change_shows_added_and_removed_lines() {
        let modified = change('M', "src/feed.rs", Some((84, 12)));
        assert_eq!(stat_label(&modified), "+84 -12");
    }

    #[test]
    fn a_pure_addition_omits_the_zero_removed_count() {
        let added = change('M', "README.md", Some((8, 0)));
        assert_eq!(stat_label(&added), "+8");
    }

    #[test]
    fn a_pure_deletion_omits_the_zero_added_count() {
        let deleted = change('D', "doomed.txt", Some((0, 25)));
        assert_eq!(stat_label(&deleted), "-25");
    }

    #[test]
    fn an_untracked_file_is_labelled_rather_than_scored() {
        let untracked = change('?', "notes.txt", None);
        assert_eq!(stat_label(&untracked), "untracked");
    }

    #[test]
    fn a_tracked_file_with_no_stat_available_shows_nothing_rather_than_a_wrong_number() {
        // A binary file, for instance: `git diff --numstat` reports no line counts for it, and
        // that must not be confused with "no changes" or with "untracked".
        let binary = change('M', "logo.png", None);
        assert_eq!(stat_label(&binary), "");
    }

    #[test]
    fn the_summary_line_totals_every_listed_change() {
        let changes = vec![
            change('M', "src/feed.rs", Some((84, 12))),
            change('M', "tests/feed.rs", Some((42, 3))),
            change('?', "notes.txt", None),
        ];
        assert_eq!(summary_line(&changes), "3 changes · +126 \u{2212}15");
    }

    #[test]
    fn a_single_change_is_singular() {
        let changes = vec![change('M', "src/feed.rs", Some((1, 0)))];
        assert_eq!(summary_line(&changes), "1 change · +1 \u{2212}0");
    }

    #[test]
    fn partitioning_selects_exactly_the_chosen_indices_and_leaves_the_rest_in_order() {
        let changes = vec![
            change('M', "a.rs", None),
            change('M', "b.rs", None),
            change('M', "c.rs", None),
            change('M', "d.rs", None),
        ];
        let (chosen, rest) = partition(changes, &[1, 3]);
        let chosen_paths: Vec<&str> = chosen.iter().map(|change| change.path.as_str()).collect();
        let rest_paths: Vec<&str> = rest.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(chosen_paths, ["b.rs", "d.rs"]);
        assert_eq!(rest_paths, ["a.rs", "c.rs"]);
    }

    #[test]
    fn partitioning_with_no_indices_selects_nothing() {
        let changes = vec![change('M', "a.rs", None), change('M', "b.rs", None)];
        let (chosen, rest) = partition(changes, &[]);
        assert!(chosen.is_empty());
        assert_eq!(rest.len(), 2);
    }

    #[test]
    fn a_file_claimed_by_one_batch_cannot_also_appear_in_the_rest() {
        // The invariant a multi-batch session depends on: partitioning is a true split, so a path
        // that ends up in one batch's `chosen` can never also be offered for the next one.
        let changes = vec![
            change('M', "a.rs", None),
            change('M', "b.rs", None),
            change('M', "c.rs", None),
        ];
        let (chosen, rest) = partition(changes, &[0, 2]);
        for change in &chosen {
            assert!(
                !rest.iter().any(|other| other.path == change.path),
                "{} appeared in both the chosen batch and the remaining files",
                change.path
            );
        }
    }
}
