//! Optional GitHub adapter for the pull-request publication strategy.
//!
//! Two things shape this crate, and both are deliberate.
//!
//! It is optional. No part of scheduling, capture, verification, or direct-push publication may
//! depend on it, so the pure-Git path stays whole for GitLab, Bitbucket and self-hosted users. A
//! repository that never chooses the pull-request strategy never reaches this code.
//!
//! It speaks HTTP by driving `curl` as a process, the same way `reccursive-git` drives `git`: no
//! shell, an explicit timeout, and arguments passed as a list. That keeps TLS, proxies and the
//! system trust store out of this codebase entirely, and it means the token can be handed over on
//! stdin rather than on a command line where every process on the machine could read it.
//!
//! What this crate cannot do is merge. There is no merge call here and there is not meant to be
//! one: opening a pull request is automation, merging it is authority over what lands on a main
//! branch, and the product does not take that.

pub mod storage;

use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use serde::Deserialize;
use thiserror::Error;
use wait_timeout::ChildExt;

/// Where the API lives and how to reach it.
///
/// `unix_socket` exists so a test can point a real `curl` at a fake endpoint with no network and
/// no credentials. Production leaves it `None` and talks to `https://api.github.com` over TLS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub base_url: String,
    pub unix_socket: Option<std::path::PathBuf>,
}

impl Endpoint {
    /// The public API.
    #[must_use]
    pub fn github() -> Self {
        Self {
            base_url: "https://api.github.com".to_owned(),
            unix_socket: None,
        }
    }
}

/// The `owner/repository` a pull request belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySlug {
    pub owner: String,
    pub repository: String,
}

impl RepositorySlug {
    /// Reads `owner/repo` out of a remote URL, or reports that it is not a GitHub remote.
    ///
    /// Both forms GitHub hands out are accepted — `https://github.com/o/r.git` and
    /// `git@github.com:o/r.git` — because a user pastes whichever one they were given. Anything
    /// else returns `None` rather than a guess: a wrong slug would open a pull request against
    /// someone else's repository.
    #[must_use]
    pub fn from_remote(remote: &str) -> Option<Self> {
        let remainder = remote
            .strip_prefix("https://github.com/")
            .or_else(|| remote.strip_prefix("http://github.com/"))
            .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
            .or_else(|| remote.strip_prefix("git@github.com:"))?;
        let remainder = remainder.strip_suffix(".git").unwrap_or(remainder);
        let remainder = remainder.trim_end_matches('/');
        let (owner, repository) = remainder.split_once('/')?;
        if owner.is_empty() || repository.is_empty() || repository.contains('/') {
            return None;
        }
        Some(Self {
            owner: owner.to_owned(),
            repository: repository.to_owned(),
        })
    }
}

/// One pull request, as much of it as this product has any business knowing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    /// `open` or `closed`, verbatim from the API.
    pub state: String,
    /// Whether it has been merged. Only ever observed, never caused.
    pub merged: bool,
}

/// What went wrong, in the only categories that change what a caller should do.
#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("the GitHub token was refused")]
    Unauthorized,
    #[error("the GitHub token is missing a permission this needs: {detail}")]
    Forbidden { detail: String },
    #[error("GitHub has no such repository, branch, or pull request")]
    NotFound,
    /// A pull request already exists for this head branch. Carries nothing, because the number has
    /// to be looked up rather than guessed.
    #[error("a pull request already exists for that branch")]
    AlreadyExists,
    #[error("GitHub asked to be called again later")]
    RateLimited,
    #[error("GitHub returned {status}: {detail}")]
    Unexpected { status: u16, detail: String },
    #[error("GitHub could not be reached: {0}")]
    Transport(String),
    #[error("GitHub's answer could not be read: {0}")]
    Malformed(String),
    #[error("the request to GitHub ran longer than {}s", .0.as_secs())]
    TimedOut(Duration),
}

impl GitHubError {
    /// Reports whether waiting and trying again could plausibly succeed.
    ///
    /// A refused token is never retried: presenting a rejected credential repeatedly is how an
    /// account gets locked, and nobody is present to answer a prompt.
    #[must_use]
    pub const fn is_worth_retrying(&self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::Transport(_) | Self::TimedOut(_)
        )
    }
}

/// A GitHub token. Prints as a placeholder so it cannot reach a log by accident.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Rejects a token that could not be sent as a header, rather than producing a broken request.
    pub fn new(value: impl Into<String>) -> Result<Self, GitHubError> {
        let value = value.into().trim().to_owned();
        if value.is_empty() {
            return Err(GitHubError::Malformed("the token is empty".to_owned()));
        }
        if value
            .bytes()
            .any(|byte| !(0x21..=0x7e).contains(&byte) || byte == b'"')
        {
            return Err(GitHubError::Malformed(
                "the token contains characters that cannot be sent in a header".to_owned(),
            ));
        }
        Ok(Self(value))
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Token(redacted)")
    }
}

/// Talks to one GitHub repository on behalf of one enrolled repository.
pub struct GitHubClient {
    endpoint: Endpoint,
    token: Token,
    timeout: Duration,
}

impl GitHubClient {
    #[must_use]
    pub const fn new(endpoint: Endpoint, token: Token, timeout: Duration) -> Self {
        Self {
            endpoint,
            token,
            timeout,
        }
    }

    /// Opens a pull request from `head` into `base`.
    ///
    /// `AlreadyExists` is returned rather than treated as success, because the caller has to learn
    /// the existing number before it can record anything — and recording the wrong number is worse
    /// than failing.
    pub fn create_pull_request(
        &self,
        slug: &RepositorySlug,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest, GitHubError> {
        let payload = serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
            "maintainer_can_modify": true,
        })
        .to_string();
        let path = format!("/repos/{}/{}/pulls", slug.owner, slug.repository);
        let response = self.send("POST", &path, Some(&payload))?;
        parse_pull_request(&response.body)
    }

    /// Reads one pull request back, which is the only way this product learns it was merged.
    pub fn pull_request(
        &self,
        slug: &RepositorySlug,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        let path = format!("/repos/{}/{}/pulls/{number}", slug.owner, slug.repository);
        let response = self.send("GET", &path, None)?;
        parse_pull_request(&response.body)
    }

    /// Finds an open pull request already opened for `head`, if one exists.
    ///
    /// This is what makes creation idempotent across a crash: a process that opened a pull request
    /// and died before recording its number finds it here instead of opening a second one.
    pub fn find_open_pull_request(
        &self,
        slug: &RepositorySlug,
        owner: &str,
        head: &str,
    ) -> Result<Option<PullRequest>, GitHubError> {
        let path = format!(
            "/repos/{}/{}/pulls?state=open&head={owner}:{head}",
            slug.owner, slug.repository
        );
        let response = self.send("GET", &path, None)?;
        let listed: Vec<RawPullRequest> = serde_json::from_str(&response.body)
            .map_err(|error| GitHubError::Malformed(error.to_string()))?;
        Ok(listed.into_iter().next().map(Into::into))
    }

    /// Runs one request and classifies its outcome.
    fn send(&self, method: &str, path: &str, body: Option<&str>) -> Result<Response, GitHubError> {
        // The status is asked for separately from the body so a non-2xx answer can be classified
        // without parsing a payload that may not be JSON at all.
        let mut arguments: Vec<String> = vec![
            "--silent".into(),
            "--show-error".into(),
            "--config".into(),
            "-".into(),
            "--request".into(),
            method.to_owned(),
            "--write-out".into(),
            "\n%{http_code}".into(),
        ];
        if let Some(socket) = &self.endpoint.unix_socket {
            arguments.push("--unix-socket".into());
            arguments.push(socket.to_string_lossy().into_owned());
        }
        if let Some(payload) = body {
            arguments.push("--data-binary".into());
            arguments.push(payload.to_owned());
        }
        arguments.push(format!("{}{path}", self.endpoint.base_url));

        let mut child = Command::new("curl")
            .args(&arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| GitHubError::Transport(error.to_string()))?;

        // The token goes here and nowhere else: not in `argv`, where any process could read it
        // from `ps`, and not in a file, which would have to be created, chmodded and removed.
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| GitHubError::Transport("curl refused stdin".to_owned()))?;
            let config = format!(
                "header = \"Authorization: Bearer {}\"\n\
                 header = \"Accept: application/vnd.github+json\"\n\
                 header = \"X-GitHub-Api-Version: 2022-11-28\"\n\
                 header = \"Content-Type: application/json\"\n\
                 header = \"User-Agent: reccursive\"\n",
                self.token.0
            );
            stdin
                .write_all(config.as_bytes())
                .map_err(|error| GitHubError::Transport(error.to_string()))?;
        }

        let status = match child
            .wait_timeout(self.timeout)
            .map_err(|error| GitHubError::Transport(error.to_string()))?
        {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitHubError::TimedOut(self.timeout));
            }
        };
        let output = child
            .wait_with_output()
            .map_err(|error| GitHubError::Transport(error.to_string()))?;
        if !status.success() {
            return Err(GitHubError::Transport(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }

        let combined = String::from_utf8(output.stdout)
            .map_err(|_| GitHubError::Malformed("the answer was not text".to_owned()))?;
        let (body, code) = combined
            .rsplit_once('\n')
            .ok_or_else(|| GitHubError::Malformed("no status code was reported".to_owned()))?;
        let code: u16 = code
            .trim()
            .parse()
            .map_err(|_| GitHubError::Malformed(format!("unreadable status code: {code}")))?;
        classify(code, body)
    }
}

struct Response {
    body: String,
}

/// Turns a status code into the category that decides what the caller does next.
fn classify(code: u16, body: &str) -> Result<Response, GitHubError> {
    match code {
        200..=299 => Ok(Response {
            body: body.to_owned(),
        }),
        401 => Err(GitHubError::Unauthorized),
        // GitHub uses 403 both for a token missing a scope and for a secondary rate limit, and
        // they need opposite handling: one waits, the other cannot be retried at all.
        403 if body.to_ascii_lowercase().contains("rate limit") => Err(GitHubError::RateLimited),
        403 => Err(GitHubError::Forbidden {
            detail: message(body),
        }),
        404 => Err(GitHubError::NotFound),
        // 422 covers every validation failure, so the message is the only thing separating "a pull
        // request already exists" from a genuinely malformed request.
        422 if body.to_ascii_lowercase().contains("already exists") => {
            Err(GitHubError::AlreadyExists)
        }
        429 => Err(GitHubError::RateLimited),
        other => Err(GitHubError::Unexpected {
            status: other,
            detail: message(body),
        }),
    }
}

/// Pulls GitHub's own explanation out of an error body, falling back to the body itself.
fn message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        message: Option<String>,
    }
    serde_json::from_str::<ErrorBody>(body)
        .ok()
        .and_then(|parsed| parsed.message)
        .unwrap_or_else(|| body.trim().chars().take(200).collect())
}

#[derive(Deserialize)]
struct RawPullRequest {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merged_at: Option<String>,
}

impl From<RawPullRequest> for PullRequest {
    fn from(raw: RawPullRequest) -> Self {
        Self {
            number: raw.number,
            url: raw.html_url,
            state: raw.state,
            // `merged` is present when a pull request is read directly and absent when it comes
            // from a list, where `merged_at` carries the same fact. Taking either means a listed
            // pull request is not mistakenly reported as unmerged.
            merged: raw.merged || raw.merged_at.is_some(),
        }
    }
}

fn parse_pull_request(body: &str) -> Result<PullRequest, GitHubError> {
    serde_json::from_str::<RawPullRequest>(body)
        .map(Into::into)
        .map_err(|error| GitHubError::Malformed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_read_from_the_remote_forms_github_hands_out() {
        for remote in [
            "https://github.com/Aaryan1524/ReccursiveCommits.git",
            "https://github.com/Aaryan1524/ReccursiveCommits",
            "git@github.com:Aaryan1524/ReccursiveCommits.git",
            "ssh://git@github.com/Aaryan1524/ReccursiveCommits.git",
        ] {
            assert_eq!(
                RepositorySlug::from_remote(remote),
                Some(RepositorySlug {
                    owner: "Aaryan1524".into(),
                    repository: "ReccursiveCommits".into(),
                }),
                "{remote}"
            );
        }
    }

    #[test]
    fn a_remote_that_is_not_github_is_refused_rather_than_guessed_at() {
        // Guessing here would open a pull request against whatever repository the guess named.
        for remote in [
            "https://gitlab.com/owner/project.git",
            "git@bitbucket.org:owner/project.git",
            "/srv/git/bare.git",
            "https://github.com/",
            "https://github.com/only-an-owner",
            "https://notgithub.com/owner/project.git",
        ] {
            assert_eq!(RepositorySlug::from_remote(remote), None, "{remote}");
        }
    }

    #[test]
    fn a_token_that_could_not_be_sent_is_refused_before_any_request() {
        assert!(Token::new("ghp_valid_token_value").is_ok());
        for bad in ["", "   ", "has space", "has\"quote", "has\nnewline"] {
            assert!(Token::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_token_never_prints_itself() {
        let token = Token::new("ghp_secret_value").unwrap();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("ghp_secret_value"), "{rendered}");
        assert_eq!(rendered, "Token(redacted)");
    }

    #[test]
    fn statuses_are_classified_into_the_actions_they_imply() {
        assert!(classify(201, "{}").is_ok());
        assert!(matches!(
            classify(401, "{}"),
            Err(GitHubError::Unauthorized)
        ));
        assert!(matches!(
            classify(403, r#"{"message":"Resource not accessible by personal access token"}"#),
            Err(GitHubError::Forbidden { detail }) if detail.contains("not accessible")
        ));
        // A secondary rate limit also arrives as 403 and must wait rather than stop forever.
        assert!(matches!(
            classify(
                403,
                r#"{"message":"You have exceeded a secondary rate limit"}"#
            ),
            Err(GitHubError::RateLimited)
        ));
        assert!(matches!(classify(404, "{}"), Err(GitHubError::NotFound)));
        assert!(matches!(
            classify(
                422,
                r#"{"errors":[{"message":"A pull request already exists for o:branch."}]}"#
            ),
            Err(GitHubError::AlreadyExists)
        ));
        // A different 422 is a real validation failure and must not be mistaken for the above.
        assert!(matches!(
            classify(
                422,
                r#"{"message":"Validation Failed","errors":[{"field":"base"}]}"#
            ),
            Err(GitHubError::Unexpected { status: 422, .. })
        ));
        assert!(matches!(classify(429, "{}"), Err(GitHubError::RateLimited)));
        assert!(matches!(
            classify(500, "server exploded"),
            Err(GitHubError::Unexpected { status: 500, .. })
        ));
    }

    #[test]
    fn only_transient_failures_are_worth_retrying() {
        // A refused credential is never retried: repeating it is how an account gets locked.
        assert!(!GitHubError::Unauthorized.is_worth_retrying());
        assert!(
            !GitHubError::Forbidden {
                detail: String::new()
            }
            .is_worth_retrying()
        );
        assert!(!GitHubError::NotFound.is_worth_retrying());
        assert!(!GitHubError::AlreadyExists.is_worth_retrying());
        assert!(GitHubError::RateLimited.is_worth_retrying());
        assert!(GitHubError::Transport(String::new()).is_worth_retrying());
        assert!(GitHubError::TimedOut(Duration::from_secs(1)).is_worth_retrying());
    }

    #[test]
    fn a_listed_pull_request_that_was_merged_is_not_reported_as_open_work() {
        // Lists carry `merged_at` and omit `merged`; reading only `merged` would call a merged
        // pull request unmerged and wait for it forever.
        let listed: RawPullRequest = serde_json::from_str(
            r#"{"number":7,"html_url":"https://github.com/o/r/pull/7","state":"closed","merged_at":"2026-09-14T00:00:00Z"}"#,
        )
        .unwrap();
        let pull_request: PullRequest = listed.into();
        assert!(pull_request.merged);
        assert_eq!(pull_request.number, 7);
    }
}

/// Exercises the adapter against a stand-in endpoint over a Unix socket.
///
/// These run the real `curl` against a real HTTP server, so what is tested is the whole path a
/// request actually takes — argument construction, the token on stdin, status classification and
/// parsing — rather than a mock of it. Skipped when `python3` is unavailable.
#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use std::{io::Read, process::Child, time::Instant};

    struct Fake {
        child: Child,
        socket: std::path::PathBuf,
        _directory: tempfile::TempDir,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start() -> Option<Fake> {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/scenarios/support/fake_github.py");
        if !script.exists() || Command::new("python3").arg("--version").output().is_err() {
            return None;
        }
        let directory = tempfile::tempdir().ok()?;
        let socket = directory.path().join("github.sock");
        let child = Command::new("python3")
            .arg(&script)
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(Fake {
            child,
            socket,
            _directory: directory,
        })
    }

    fn client(fake: &Fake, token: &str) -> GitHubClient {
        GitHubClient::new(
            Endpoint {
                base_url: "http://localhost".to_owned(),
                unix_socket: Some(fake.socket.clone()),
            },
            Token::new(token).unwrap(),
            Duration::from_secs(20),
        )
    }

    fn slug() -> RepositorySlug {
        RepositorySlug {
            owner: "owner".into(),
            repository: "project".into(),
        }
    }

    fn control(fake: &Fake, path: &str) {
        let status = Command::new("curl")
            .args([
                "--silent",
                "--output",
                "/dev/null",
                "--unix-socket",
                &fake.socket.to_string_lossy(),
                "--request",
                "POST",
                &format!("http://localhost{path}"),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_pull_request_is_opened_read_back_and_never_opened_twice() {
        let Some(fake) = start() else { return };
        let client = client(&fake, "test-token");

        let opened = client
            .create_pull_request(&slug(), "work", "main", "Add the change", "body")
            .unwrap();
        assert_eq!(opened.number, 1);
        assert_eq!(opened.state, "open");
        assert!(!opened.merged);

        // The idempotency case that matters: a crash after creating but before recording must not
        // open a second pull request. The endpoint refuses, and the existing one is findable.
        assert!(matches!(
            client.create_pull_request(&slug(), "work", "main", "Add the change", "body"),
            Err(GitHubError::AlreadyExists)
        ));
        let found = client
            .find_open_pull_request(&slug(), "owner", "work")
            .unwrap()
            .expect("the existing pull request is findable");
        assert_eq!(found.number, opened.number);

        // Merging is something a person does. The product only ever observes that it happened.
        assert!(!client.pull_request(&slug(), opened.number).unwrap().merged);
        control(&fake, &format!("/control/merge/{}", opened.number));
        let merged = client.pull_request(&slug(), opened.number).unwrap();
        assert!(merged.merged);
        assert_eq!(merged.state, "closed");
    }

    #[test]
    fn a_refused_token_and_a_missing_scope_are_told_apart() {
        let Some(fake) = start() else { return };
        // Both are permanent, but one asks the user for a different token and the other for a
        // different scope on the same token.
        assert!(matches!(
            client(&fake, "wrong-token").create_pull_request(&slug(), "a", "main", "t", "b"),
            Err(GitHubError::Unauthorized)
        ));
        assert!(matches!(
            client(&fake, "unscoped-token").create_pull_request(&slug(), "a", "main", "t", "b"),
            Err(GitHubError::Forbidden { .. })
        ));
    }

    #[test]
    fn the_token_never_reaches_the_process_table() {
        let Some(fake) = start() else { return };
        // The whole reason the token goes in on stdin. A token in `argv` is readable by every
        // process on the machine, which is not a trade this product makes for convenience.
        let client = client(&fake, "ghp-unmistakable-secret");
        let handle = std::thread::spawn(move || {
            let mut sightings = 0;
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(output) = Command::new("ps")
                    .args(["-ww", "-o", "args=", "-A"])
                    .output()
                {
                    let mut text = String::new();
                    let _ = output.stdout.as_slice().read_to_string(&mut text);
                    // Only processes that *are* curl are the claim under test. Matching any line
                    // mentioning curl instead reports the shell that invoked this test with the
                    // literal in its own command line — a true sighting of the wrong thing.
                    sightings += text
                        .lines()
                        .filter(|line| {
                            line.split_whitespace()
                                .next()
                                .is_some_and(|argv0| argv0.rsplit('/').next() == Some("curl"))
                        })
                        .filter(|line| line.contains("ghp-unmistakable-secret"))
                        .count();
                }
            }
            sightings
        });
        for _ in 0..8 {
            let _ = client.create_pull_request(&slug(), "scan", "main", "t", "b");
        }
        assert_eq!(
            handle.join().unwrap(),
            0,
            "the token appeared in a curl command line"
        );
    }

    #[test]
    fn a_missing_pull_request_is_not_confused_with_an_empty_one() {
        let Some(fake) = start() else { return };
        let client = client(&fake, "test-token");
        assert!(matches!(
            client.pull_request(&slug(), 4242),
            Err(GitHubError::NotFound)
        ));
        assert!(
            client
                .find_open_pull_request(&slug(), "owner", "never-pushed")
                .unwrap()
                .is_none()
        );
    }
}
