//! Credential and signing diagnostics.
//!
//! Background publication must never stop on a question nobody is present to answer. The Git
//! adapter already makes that structural — terminal prompts are disabled, askpass is pointed at a
//! program that always fails, and every invocation is bounded by a timeout — so a missing
//! credential fails quickly instead of waiting forever.
//!
//! Failing quickly is only half of it. The other half is being able to say *why*, before a release
//! is attempted rather than after it has been blocked, which is what this module is for.

use std::{path::Path, time::Duration};

use reccursive_core::ConnectivityFault;
use reccursive_git::{CancellationToken, GitInvocation, GitRunner};
use serde::{Deserialize, Serialize};

/// How long any single probe may run. Short: a probe exists to answer quickly or not at all.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Whether the daemon can currently authenticate to a remote.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CredentialStatus {
    /// The remote answered and accepted the stored credentials.
    Ready,
    /// The remote answered and refused them. A person has to act; retrying cannot help.
    Rejected { detail: String },
    /// The remote could not be reached. This may clear on its own.
    Unreachable { detail: String },
    /// The probe ran out of time. Treated as reachable-but-slow rather than as a refusal.
    TimedOut,
}

impl CredentialStatus {
    /// Reports whether publication could proceed right now.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Reports whether a person has to intervene before this can succeed.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }
}

/// Whether commit signing, if the repository asks for it, can actually be performed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SigningStatus {
    /// The repository does not sign commits, so nothing can block on a signing agent.
    Disabled,
    /// Signing is configured and the key is usable without a prompt.
    Ready { format: String },
    /// Signing is configured but cannot be performed — typically a locked agent, or a key that is
    /// configured but not present.
    Unavailable { format: String, detail: String },
}

impl SigningStatus {
    /// Reports whether a release would be blocked by signing.
    #[must_use]
    pub const fn blocks_release(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

/// Whether the enrolled checkout is still there and still usable.
///
/// Credentials and signing are probed against the managed mirror and survive the checkout being
/// moved or deleted, so without this a repository whose working copy is gone reports itself
/// perfectly healthy while nothing can publish — the release worker needs the checkout to read the
/// author identity, and skips any repository it cannot read one from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckoutStatus {
    /// Present, a Git repository, and carrying an identity to attribute commits to.
    Ready { author: String },
    /// The enrolled path no longer exists.
    Missing { path: String },
    /// The path exists but is not a Git repository any more.
    NotARepository { path: String },
    /// Present, but with no `user.name`/`user.email` to attribute a commit to.
    ///
    /// Publication refuses rather than inventing an author, so this blocks releases exactly as
    /// firmly as a missing checkout does.
    IdentityMissing { path: String },
}

impl CheckoutStatus {
    /// Reports whether a commit could be created and attributed from this checkout.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

/// What the daemon can and cannot do with one repository right now.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryDiagnostics {
    pub checkout: CheckoutStatus,
    pub credentials: CredentialStatus,
    pub signing: SigningStatus,
}

impl RepositoryDiagnostics {
    /// Reports whether publication would succeed as far as this machine is concerned.
    #[must_use]
    pub const fn can_publish(&self) -> bool {
        self.checkout.is_ready() && self.credentials.is_ready() && !self.signing.blocks_release()
    }
}

/// Reports whether the enrolled checkout can still produce an attributed commit.
pub fn probe_checkout(checkout_path: &Path) -> CheckoutStatus {
    let path = checkout_path.to_string_lossy().into_owned();
    if !checkout_path.exists() {
        return CheckoutStatus::Missing { path };
    }
    // Asked of Git rather than by looking for a `.git` entry, so a worktree or a repository with a
    // separate git dir is not mistaken for a broken one.
    match git_config(checkout_path, "core.bare") {
        Some(_) => {}
        None => {
            let invocation =
                GitInvocation::new(checkout_path, ["rev-parse", "--git-dir"], PROBE_TIMEOUT);
            let usable = invocation.is_ok_and(|invocation| {
                GitRunner::run(&invocation, &CancellationToken::default()).is_ok()
            });
            if !usable {
                return CheckoutStatus::NotARepository { path };
            }
        }
    }
    match (
        git_config(checkout_path, "user.name"),
        git_config(checkout_path, "user.email"),
    ) {
        (Some(name), Some(email)) if !name.is_empty() && !email.is_empty() => {
            CheckoutStatus::Ready {
                author: format!("{name} <{email}>"),
            }
        }
        _ => CheckoutStatus::IdentityMissing { path },
    }
}

/// Asks a remote whether the stored credentials are accepted, without changing anything.
///
/// `ls-remote` is a read: it proves the credential works without creating a ref, a commit, or any
/// other trace. Run through the hardened adapter, so a missing credential produces a refusal in
/// seconds rather than a process waiting on a prompt.
pub fn probe_credentials(managed_path: &Path, remote: &str) -> CredentialStatus {
    let invocation = match GitInvocation::new(
        managed_path,
        ["ls-remote", "--quiet", remote, "HEAD"],
        PROBE_TIMEOUT,
    ) {
        Ok(invocation) => invocation,
        Err(error) => {
            return CredentialStatus::Unreachable {
                detail: error.to_string(),
            };
        }
    };
    match GitRunner::run(&invocation, &CancellationToken::default()) {
        Ok(_) => CredentialStatus::Ready,
        Err(error) => {
            let detail = error.to_string();
            match ConnectivityFault::classify_git(&detail) {
                ConnectivityFault::Rejected => CredentialStatus::Rejected { detail },
                ConnectivityFault::TimedOut => CredentialStatus::TimedOut,
                // A refused *push* is not a credential problem, and neither is an unrecognized
                // failure. Both are reported as reachability rather than as a rejection, because
                // wrongly declaring a credential dead sends someone to re-authenticate for nothing.
                ConnectivityFault::Refused | ConnectivityFault::Unreachable => {
                    CredentialStatus::Unreachable { detail }
                }
            }
        }
    }
}

/// Reports whether this repository signs commits and, if so, whether it currently can.
///
/// The probe signs a throwaway string rather than a commit, so it proves the key is usable without
/// writing anything to the repository.
pub fn probe_signing(repository_path: &Path) -> SigningStatus {
    if !git_flag_is_true(repository_path, "commit.gpgsign") {
        return SigningStatus::Disabled;
    }
    let format = git_config(repository_path, "gpg.format").unwrap_or_else(|| "openpgp".to_owned());
    let key = git_config(repository_path, "user.signingkey");

    match format.as_str() {
        "ssh" => match key {
            Some(key) => ssh_signing_status(repository_path, &format, &key),
            None => SigningStatus::Unavailable {
                format,
                detail: "gpg.format is ssh but user.signingkey is not set".to_owned(),
            },
        },
        _ => openpgp_signing_status(repository_path, &format, key.as_deref()),
    }
}

fn openpgp_signing_status(
    repository_path: &Path,
    format: &str,
    key: Option<&str>,
) -> SigningStatus {
    // A clear-sign of nothing proves the agent is unlocked and the key is present, and writes
    // nothing anywhere. With askpass disabled, a locked agent fails here instead of prompting.
    let mut arguments = vec!["--batch".to_owned(), "--yes".to_owned()];
    if let Some(key) = key {
        arguments.push("--local-user".to_owned());
        arguments.push(key.to_owned());
    }
    arguments.push("--sign".to_owned());
    arguments.push("--output".to_owned());
    arguments.push("/dev/null".to_owned());
    arguments.push("/dev/null".to_owned());

    match run_tool(repository_path, "gpg", &arguments) {
        Ok(()) => SigningStatus::Ready {
            format: format.to_owned(),
        },
        Err(detail) => SigningStatus::Unavailable {
            format: format.to_owned(),
            detail,
        },
    }
}

fn ssh_signing_status(_repository_path: &Path, format: &str, key: &str) -> SigningStatus {
    // `user.signingkey` is either a path to a key file or the key material itself. Checking which
    // one it is, and whether the file is actually readable, answers the question directly —
    // rather than inferring it from how some version of ssh-keygen happens to word an error.
    if key.starts_with("ssh-") || key.starts_with("ecdsa-") {
        return SigningStatus::Ready {
            format: format.to_owned(),
        };
    }
    match std::fs::metadata(key) {
        Ok(_) => SigningStatus::Ready {
            format: format.to_owned(),
        },
        Err(error) => SigningStatus::Unavailable {
            format: format.to_owned(),
            detail: format!("signing key {key} cannot be read: {error}"),
        },
    }
}

/// Runs a short, non-interactive external tool and reports only whether it succeeded.
fn run_tool(directory: &Path, program: &str, arguments: &[String]) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        // No inherited terminal: a tool that decides to ask for a passphrase finds nothing to ask.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("GPG_TTY", "")
        .env("DISPLAY", "")
        .env("SSH_ASKPASS", "/bin/false")
        .spawn()
        .map_err(|error| format!("{program} could not be started: {error}"))?;

    let status = wait_timeout::ChildExt::wait_timeout(&mut child, PROBE_TIMEOUT)
        .map_err(|error| format!("{program} could not be observed: {error}"))?;
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("{program} exited with {status}")),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            // A probe that has to be killed is itself the finding: something is waiting on input
            // that will never arrive.
            Err(format!(
                "{program} did not finish within {PROBE_TIMEOUT:?}; it may be waiting for input"
            ))
        }
    }
}

fn git_config(repository_path: &Path, key: &str) -> Option<String> {
    let invocation =
        GitInvocation::new(repository_path, ["config", "--get", key], PROBE_TIMEOUT).ok()?;
    let output = GitRunner::run(&invocation, &CancellationToken::default()).ok()?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn git_flag_is_true(repository_path: &Path, key: &str) -> bool {
    git_config(repository_path, key)
        .is_some_and(|value| matches!(value.as_str(), "true" | "yes" | "on" | "1"))
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::tempdir;

    use super::*;

    fn git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {arguments:?} failed");
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        git(directory.path(), &["config", "user.name", "Fixture"]);
        git(
            directory.path(),
            &["config", "user.email", "fixture@example.invalid"],
        );
        directory
    }

    #[test]
    fn a_repository_that_does_not_sign_reports_signing_as_disabled() {
        let directory = repository();

        let status = probe_signing(directory.path());

        assert_eq!(status, SigningStatus::Disabled);
        assert!(
            !status.blocks_release(),
            "a repository that never signs cannot be blocked by a signing agent"
        );
    }

    #[test]
    fn signing_configured_without_a_key_is_reported_rather_than_attempted() {
        let directory = repository();
        git(directory.path(), &["config", "commit.gpgsign", "true"]);
        git(directory.path(), &["config", "gpg.format", "ssh"]);

        let status = probe_signing(directory.path());

        match status {
            SigningStatus::Unavailable { format, detail } => {
                assert_eq!(format, "ssh");
                assert!(detail.contains("user.signingkey"), "{detail}");
            }
            other => panic!("expected an actionable report, got {other:?}"),
        }
        assert!(probe_signing(directory.path()).blocks_release());
    }

    #[test]
    fn a_signing_key_that_is_not_there_is_reported_as_unavailable() {
        let directory = repository();
        git(directory.path(), &["config", "commit.gpgsign", "true"]);
        git(directory.path(), &["config", "gpg.format", "ssh"]);
        git(
            directory.path(),
            &[
                "config",
                "user.signingkey",
                "/nonexistent/path/to/signing.key",
            ],
        );

        let status = probe_signing(directory.path());

        assert!(
            status.blocks_release(),
            "a configured key that is not present must be reported, not discovered mid-release"
        );
    }

    #[test]
    fn an_unreachable_remote_is_not_reported_as_a_rejected_credential() {
        let directory = repository();
        let missing = directory.path().join("no-such-remote.git");

        let status = probe_credentials(directory.path(), &missing.to_string_lossy());

        // Sending someone to re-authenticate because a host was down is a real cost; only an
        // actual refusal is reported as one.
        assert!(
            !status.needs_attention(),
            "an absent remote is not evidence that a credential was refused: {status:?}"
        );
        assert!(!status.is_ready());
    }

    #[test]
    fn a_reachable_local_remote_reports_credentials_as_ready() {
        let directory = repository();
        let remote = directory.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--quiet", "--bare"]);

        let status = probe_credentials(directory.path(), &remote.to_string_lossy());

        assert_eq!(status, CredentialStatus::Ready);
        assert!(
            RepositoryDiagnostics {
                checkout: probe_checkout(directory.path()),
                credentials: status.clone(),
                signing: SigningStatus::Disabled,
            }
            .can_publish()
        );
    }

    #[test]
    fn a_checkout_that_is_gone_blocks_publication_however_healthy_the_remote_looks() {
        let directory = repository();
        let remote = directory.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--quiet", "--bare"]);
        let credentials = probe_credentials(directory.path(), &remote.to_string_lossy());
        assert_eq!(credentials, CredentialStatus::Ready);

        // Credentials and signing are probed against the managed mirror, which outlives the
        // working copy. Without the checkout probe this repository reports itself able to publish
        // while the release worker silently skips it forever, because it cannot read an author.
        let missing = directory.path().join("gone");
        let status = probe_checkout(&missing);
        assert!(
            matches!(status, CheckoutStatus::Missing { .. }),
            "{status:?}"
        );
        assert!(
            !RepositoryDiagnostics {
                checkout: status,
                credentials,
                signing: SigningStatus::Disabled,
            }
            .can_publish()
        );
    }

    #[test]
    fn a_checkout_with_no_identity_cannot_publish_either() {
        // Publication refuses to invent an author, so this blocks a release exactly as firmly as
        // a missing directory does — and saying "ready" here would send someone hunting the wrong
        // problem.
        let directory = repository();
        // Set empty rather than unset: unsetting the local value falls through to the machine's
        // global config, which on a developer's laptop is a real identity — so the test would
        // pass locally for the wrong reason and mean nothing.
        git(directory.path(), &["config", "user.name", ""]);
        git(directory.path(), &["config", "user.email", ""]);
        let status = probe_checkout(directory.path());
        assert!(
            matches!(status, CheckoutStatus::IdentityMissing { .. }),
            "{status:?}"
        );
        assert!(!status.is_ready());
    }

    #[test]
    fn a_healthy_checkout_reports_the_identity_commits_will_carry() {
        let directory = repository();
        match probe_checkout(directory.path()) {
            CheckoutStatus::Ready { author } => {
                assert!(author.contains('<') && author.contains('@'), "{author}");
            }
            other => panic!("expected a ready checkout, got {other:?}"),
        }
    }

    #[test]
    fn a_probe_that_would_wait_for_input_is_killed_and_reported() {
        let directory = repository();

        // Stands in for a tool that blocks on a passphrase prompt. The point is that the probe
        // ends and says so, rather than the daemon stopping invisibly.
        let outcome = run_tool(
            directory.path(),
            "sh",
            &["-c".to_owned(), "read line".to_owned()],
        );

        assert!(outcome.is_err(), "a probe reading stdin must not hang");
    }
}
