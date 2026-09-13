//! Guided first-run setup.
//!
//! Enrolling a repository by hand means knowing which flags matter, which branch is the target,
//! what a schedule policy document looks like, and that publication silently does nothing when the
//! checkout has no committer identity. Setup walks that once, in order, and refuses rather than
//! enrolling something that cannot publish.
//!
//! It performs no privileged action of its own: every step is a command a user could issue
//! separately, sent through the same local API with the same authorization. What it adds is order,
//! defaults, and a preflight.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

/// How the schedule step should be satisfied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleChoice {
    /// Activate the built-in weekday policy in the named time zone.
    Default { timezone: String },
    /// Activate a policy document the user wrote.
    File(PathBuf),
    /// Leave the repository without a policy; nothing will be scheduled until one is set.
    None,
}

/// Everything setup needs before it may change anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPlan {
    pub repository: PathBuf,
    pub remote: String,
    pub target: String,
    pub development_target: Option<String>,
    pub immediate: bool,
    pub schedule: ScheduleChoice,
    pub install_service: bool,
}

/// Why setup will not proceed.
///
/// Each names the value that is missing and the flag that supplies it, because the whole point of
/// refusing non-interactively is that the caller can fix it without guessing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupRefusal {
    /// Prompting was required but input is not a terminal.
    NotATerminal,
    MissingRemote,
    MissingDevelopmentTarget,
    MissingIdentity {
        checkout: PathBuf,
    },
    NotARepository {
        path: PathBuf,
    },
}

impl SetupRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotATerminal => "setup needs answers but input is not a terminal; pass \
                 --non-interactive with the values it would have asked for"
                .to_owned(),
            Self::MissingRemote => {
                "no remote to publish to: origin has no readable URL, so pass --remote".to_owned()
            }
            Self::MissingDevelopmentTarget => {
                "--mode immediate publishes to a development branch first, so pass \
                 --development-target"
                    .to_owned()
            }
            // Worth refusing over. A checkout with no identity enrolls fine and then never
            // publishes: the release pass skips a unit it cannot attribute, and does so quietly.
            // Better to stop here, where there is someone to tell, than to queue work that waits
            // forever.
            Self::MissingIdentity { checkout } => format!(
                "{} has no committer identity, so nothing captured there could ever be \
                 published. Set one with:\n  git -C {} config user.name \"Your Name\"\n  \
                 git -C {} config user.email you@example.com",
                checkout.display(),
                checkout.display(),
                checkout.display()
            ),
            Self::NotARepository { path } => {
                format!("{} is not inside a Git repository", path.display())
            }
        }
    }
}

/// What setup actually did, in the order it did it.
///
/// Reported whether setup finishes or stops partway. A later step failing does not undo an earlier
/// one — enrollment is atomic on its own, and silently reversing a repository someone is already
/// using would be worse than saying plainly what stands.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SetupProgress {
    pub enrolled: Option<String>,
    pub policy_activated: bool,
    pub service_installed: bool,
}

impl SetupProgress {
    /// Describes what remains after setup stopped early.
    #[must_use]
    pub fn remaining_advice(&self, plan: &SetupPlan) -> Vec<String> {
        let mut advice = Vec::new();
        if self.enrolled.is_none() {
            advice.push(format!(
                "reccursive repository add {}",
                plan.repository.display()
            ));
        }
        if !self.policy_activated
            && let ScheduleChoice::File(path) = &plan.schedule
        {
            advice.push(format!(
                "reccursive schedule set-policy <repository> {}",
                path.display()
            ));
        }
        if plan.install_service && !self.service_installed {
            advice.push("reccursive service install".to_owned());
        }
        advice
    }
}

/// Decides whether setup asks questions, or refuses because it cannot.
///
/// Gated on *input*, not output: a caller piping answers in, or a CI job with no terminal at all,
/// must never reach a prompt that waits forever.
///
/// The third case is the one that matters. Without a terminal and without `--non-interactive`,
/// setup does not quietly fall back to defaults — it refuses. Falling back would mean a script
/// silently accepting a target branch, a schedule, and a publication mode nobody chose, which is a
/// worse outcome than being told to say what it wants.
pub fn interaction_mode(non_interactive: bool) -> Result<bool, SetupRefusal> {
    if non_interactive {
        return Ok(false);
    }
    if io::stdin().is_terminal() {
        Ok(true)
    } else {
        Err(SetupRefusal::NotATerminal)
    }
}

/// Reads the committer identity Git would use for a commit made in this checkout.
#[must_use]
pub fn checkout_identity(checkout: &Path) -> Option<(String, String)> {
    let read = |key: &str| -> Option<String> {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(checkout)
            .args(["config", "--get", key])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!value.is_empty()).then_some(value)
    };
    Some((read("user.name")?, read("user.email")?))
}

/// The built-in policy offered when a user does not bring their own.
///
/// Deliberately conservative: weekday afternoons, a few releases a day, well spaced. A first-run
/// default that published often, or at night, would be a surprising thing to have agreed to.
#[must_use]
pub fn default_policy_document(timezone: &str) -> String {
    format!(
        r#"{{
  "timezone": "{timezone}",
  "allowed_days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
  "windows": [{{"start": {{"hour": 9, "minute": 0}}, "end": {{"hour": 17, "minute": 0}}}}],
  "daily_releases": {{"minimum": 1, "maximum": 3}},
  "minimum_spacing_minutes": 45,
  "missed_window_behavior": {{"kind": "reschedule_forward"}}
}}
"#
    )
}

/// The machine's own time zone, for the default policy.
#[must_use]
pub fn local_timezone() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|path| {
            path.to_string_lossy()
                .split_once("/zoneinfo/")
                .map(|(_, zone)| zone.to_owned())
        })
        .unwrap_or_else(|| "UTC".to_owned())
}

/// Asks one question, returning the default when the answer is empty.
pub fn ask(
    out: &mut impl Write,
    input: &mut impl io::BufRead,
    question: &str,
    default: &str,
) -> io::Result<String> {
    if default.is_empty() {
        write!(out, "{question}: ")?;
    } else {
        write!(out, "{question} [{default}]: ")?;
    }
    out.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(default.to_owned());
    }
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer.to_owned()
    })
}

/// Asks a yes/no question.
pub fn confirm(
    out: &mut impl Write,
    input: &mut impl io::BufRead,
    question: &str,
    default: bool,
) -> io::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let answer = ask(out, input, &format!("{question} ({hint})"), "")?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_interactive_run_never_prompts_even_on_a_terminal() {
        // The flag is the authority. A script that happens to run where a terminal is attached
        // must behave exactly as it does in CI, or it will hang somewhere nobody is watching.
        assert_eq!(interaction_mode(true), Ok(false));
    }

    #[test]
    fn without_a_terminal_and_without_the_flag_setup_refuses_rather_than_guessing() {
        // Under `cargo test` stdin is not a terminal, which is the same condition CI runs in.
        assert_eq!(interaction_mode(false), Err(SetupRefusal::NotATerminal));
    }

    #[test]
    fn every_refusal_names_what_is_missing_and_how_to_supply_it() {
        assert!(SetupRefusal::MissingRemote.message().contains("--remote"));
        assert!(
            SetupRefusal::MissingDevelopmentTarget
                .message()
                .contains("--development-target")
        );
        assert!(
            SetupRefusal::NotATerminal
                .message()
                .contains("--non-interactive")
        );
        let identity = SetupRefusal::MissingIdentity {
            checkout: PathBuf::from("/tmp/example"),
        };
        assert!(identity.message().contains("git -C /tmp/example config"));
    }

    #[test]
    fn the_default_policy_is_a_document_the_daemon_accepts() {
        let document = default_policy_document("America/New_York");
        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        assert_eq!(parsed["timezone"], "America/New_York");
        assert_eq!(parsed["daily_releases"]["maximum"], 3);
        assert_eq!(
            parsed["missed_window_behavior"]["kind"],
            "reschedule_forward"
        );
    }

    #[test]
    fn stopping_partway_reports_only_the_steps_that_remain() {
        let plan = SetupPlan {
            repository: PathBuf::from("/tmp/example"),
            remote: "git@example.invalid:me/project.git".into(),
            target: "refs/heads/main".into(),
            development_target: None,
            immediate: false,
            schedule: ScheduleChoice::File(PathBuf::from("/tmp/policy.json")),
            install_service: true,
        };
        let enrolled_only = SetupProgress {
            enrolled: Some("repo_1".into()),
            policy_activated: false,
            service_installed: false,
        };
        let advice = enrolled_only.remaining_advice(&plan);
        assert!(
            !advice.iter().any(|line| line.contains("repository add")),
            "a completed step must not be offered again: {advice:?}"
        );
        assert!(advice.iter().any(|line| line.contains("set-policy")));
        assert!(advice.iter().any(|line| line.contains("service install")));
    }

    #[test]
    fn an_answer_left_empty_keeps_the_default() {
        let mut out = Vec::new();
        let mut input = "\n".as_bytes();
        assert_eq!(
            ask(&mut out, &mut input, "Target branch", "main").unwrap(),
            "main"
        );
        let mut input = "develop\n".as_bytes();
        assert_eq!(
            ask(&mut out, &mut input, "Target branch", "main").unwrap(),
            "develop"
        );
    }

    #[test]
    fn confirmation_defaults_apply_when_the_answer_is_empty() {
        let mut out = Vec::new();
        assert!(confirm(&mut out, &mut "\n".as_bytes(), "Install?", true).unwrap());
        assert!(!confirm(&mut out, &mut "\n".as_bytes(), "Install?", false).unwrap());
        assert!(confirm(&mut out, &mut "y\n".as_bytes(), "Install?", false).unwrap());
        assert!(!confirm(&mut out, &mut "n\n".as_bytes(), "Install?", true).unwrap());
    }
}
