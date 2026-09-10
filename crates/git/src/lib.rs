//! Single hardened boundary for all Git subprocess execution.

use std::{
    ffi::OsString,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;
use wait_timeout::ChildExt;

pub const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct GitInvocation {
    pub working_directory: PathBuf,
    pub arguments: Vec<OsString>,
    pub timeout: Duration,
}

impl GitInvocation {
    pub fn new(
        working_directory: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
        timeout: Duration,
    ) -> Result<Self, GitError> {
        let value = Self {
            working_directory: working_directory.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
            timeout,
        };
        if !value.working_directory.is_absolute()
            || value.arguments.is_empty()
            || value.timeout.is_zero()
        {
            return Err(GitError::InvalidInvocation);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git invocation requires an absolute directory, arguments, and a nonzero timeout")]
    InvalidInvocation,
    #[error("Git could not start: {0}")]
    Io(#[from] std::io::Error),
    #[error("Git operation timed out after {0:?}")]
    TimedOut(Duration),
    #[error("Git operation was cancelled")]
    Cancelled,
    #[error("Git failed ({status:?}): {message}")]
    Failed {
        status: Option<i32>,
        message: String,
    },
    #[error("managed clone path already exists: {}", .0.display())]
    ManagedPathExists(PathBuf),
}

pub struct GitRunner;
impl GitRunner {
    pub fn run(
        invocation: &GitInvocation,
        cancellation: &CancellationToken,
    ) -> Result<GitOutput, GitError> {
        let mut command = Command::new("git");
        command
            .current_dir(&invocation.working_directory)
            .args(&invocation.arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_reader = thread::spawn(move || read_bounded(stdout));
        let err_reader = thread::spawn(move || read_bounded(stderr));
        let deadline = Instant::now() + invocation.timeout;
        let status = loop {
            if cancellation.cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::TimedOut(invocation.timeout));
            }
            if let Some(status) = child.wait_timeout(remaining.min(Duration::from_millis(50)))? {
                break status;
            }
        };
        let (stdout, out_truncated) = out_reader.join().map_err(|_| GitError::Failed {
            status: None,
            message: "stdout collector panicked".into(),
        })?;
        let (stderr, err_truncated) = err_reader.join().map_err(|_| GitError::Failed {
            status: None,
            message: "stderr collector panicked".into(),
        })?;
        let output = GitOutput {
            stdout,
            stderr,
            truncated: out_truncated || err_truncated,
        };
        if status.success() {
            Ok(output)
        } else {
            Err(GitError::Failed {
                status: status.code(),
                message: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(512)
                    .collect(),
            })
        }
    }
}

/// Daemon-owned mirror used for release work, never a user's checkout.
#[derive(Clone, Debug)]
pub struct ManagedClone {
    pub path: PathBuf,
}

impl ManagedClone {
    pub fn provision(
        remote: &str,
        path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, GitError> {
        let path = path.into();
        if path.exists() {
            return Err(GitError::ManagedPathExists(path));
        }
        let parent = path.parent().ok_or(GitError::InvalidInvocation)?;
        let name = path.file_name().ok_or(GitError::InvalidInvocation)?;
        GitRunner::run(
            &GitInvocation::new(
                parent,
                [
                    OsString::from("clone"),
                    OsString::from("--mirror"),
                    OsString::from(remote),
                    name.to_os_string(),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        Ok(Self { path })
    }

    pub fn fetch_ref(
        &self,
        remote: &str,
        reference: &str,
        timeout: Duration,
    ) -> Result<GitOutput, GitError> {
        GitRunner::run(
            &GitInvocation::new(
                &self.path,
                ["fetch", "--no-tags", remote, reference],
                timeout,
            )?,
            &CancellationToken::default(),
        )
    }

    pub fn create_release_workspace(
        &self,
        destination: impl Into<PathBuf>,
        commit: &str,
        timeout: Duration,
    ) -> Result<PathBuf, GitError> {
        let destination = destination.into();
        if destination.exists() {
            return Err(GitError::ManagedPathExists(destination));
        }
        let parent = destination.parent().ok_or(GitError::InvalidInvocation)?;
        let name = destination.file_name().ok_or(GitError::InvalidInvocation)?;
        GitRunner::run(
            &GitInvocation::new(
                parent,
                [
                    OsString::from("clone"),
                    OsString::from("--no-checkout"),
                    self.path.as_os_str().to_os_string(),
                    name.to_os_string(),
                ],
                timeout,
            )?,
            &CancellationToken::default(),
        )?;
        GitRunner::run(
            &GitInvocation::new(&destination, ["checkout", "--detach", commit], timeout)?,
            &CancellationToken::default(),
        )?;
        Ok(destination)
    }
}

fn read_bounded(mut reader: impl Read) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    let mut truncated = false;
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        let room = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(bytes.len());
        if room < count {
            bytes.extend_from_slice(&chunk[..room]);
            truncated = true;
        } else {
            bytes.extend_from_slice(&chunk[..count]);
        }
    }
    (bytes, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn argument_is_not_interpreted_by_a_shell() {
        let dir = tempdir().unwrap();
        let invocation =
            GitInvocation::new(dir.path(), ["--version; false"], Duration::from_secs(1)).unwrap();
        assert!(matches!(
            GitRunner::run(&invocation, &CancellationToken::default()),
            Err(GitError::Failed { .. })
        ));
    }
    #[test]
    fn cancellation_stops_a_process() {
        let dir = tempdir().unwrap();
        let token = CancellationToken::default();
        token.cancel();
        let invocation =
            GitInvocation::new(dir.path(), ["status"], Duration::from_secs(1)).unwrap();
        assert!(matches!(
            GitRunner::run(&invocation, &token),
            Err(GitError::Cancelled)
        ));
    }
    #[test]
    fn managed_mirror_fetches_and_release_workspace_is_detached() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote.git");
        std::process::Command::new("git")
            .args(["init", "--quiet", source.to_str().unwrap()])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                source.to_str().unwrap(),
                "config",
                "user.name",
                "Fixture",
            ])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                source.to_str().unwrap(),
                "config",
                "user.email",
                "fixture@example.invalid",
            ])
            .status()
            .unwrap();
        std::fs::write(source.join("file"), "ok").unwrap();
        std::process::Command::new("git")
            .args(["-C", source.to_str().unwrap(), "add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-C",
                source.to_str().unwrap(),
                "commit",
                "--quiet",
                "-m",
                "base",
            ])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "clone",
                "--quiet",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        let mirror = ManagedClone::provision(
            remote.to_str().unwrap(),
            root.path().join("mirror.git"),
            Duration::from_secs(5),
        )
        .unwrap();
        mirror
            .fetch_ref("origin", "HEAD", Duration::from_secs(5))
            .unwrap();
        let head = String::from_utf8(
            GitRunner::run(
                &GitInvocation::new(&source, ["rev-parse", "HEAD"], Duration::from_secs(5))
                    .unwrap(),
                &CancellationToken::default(),
            )
            .unwrap()
            .stdout,
        )
        .unwrap();
        let workspace = mirror
            .create_release_workspace(
                root.path().join("release"),
                head.trim(),
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(workspace.join("file").exists());
    }
}
