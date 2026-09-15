//! User-session service installation.
//!
//! The service runs in the user's own login session, not as a system daemon. That is deliberate:
//! publishing uses the user's existing Git credentials and SSH agent, and a root-owned daemon
//! would either lose access to them or need a privileged copy of them.
//!
//! Installation is described as data first and applied second, so the same description can be
//! rendered for inspection, written to disk, and torn down without the caller reverse-engineering
//! what was done.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Where an installed service's definition lives and what it will run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceInstallation {
    /// Reverse-DNS launchd label, also the name used to start, stop, and remove the service.
    pub label: String,
    /// The `launchd` property list describing the service.
    pub plist_path: PathBuf,
    /// The daemon binary the service runs.
    pub program: PathBuf,
    /// The state directory the daemon is given.
    pub state_dir: PathBuf,
    /// Where the service's own output is written.
    pub log_path: PathBuf,
}

impl ServiceInstallation {
    /// Describes an installation without touching the filesystem.
    pub fn describe(
        program: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        home: &Path,
    ) -> Self {
        let state_dir = state_dir.into();
        let label = format!("com.reccursive.{}", reccursive_protocol::SERVICE_NAME);
        Self {
            plist_path: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{label}.plist")),
            log_path: state_dir.join("service.log"),
            program: program.into(),
            state_dir,
            label,
        }
    }

    /// Renders the launchd property list for this installation.
    ///
    /// `RunAtLoad` starts the service when the agent is loaded, and `KeepAlive` restarts it if it
    /// exits, so a crash and a closed terminal both leave publishing running.
    ///
    /// What that does *not* buy is surviving a logout. This is a launchd **agent** in the user's
    /// own `~/Library/LaunchAgents`, with no `LimitLoadToSessionType`, so it belongs to the login
    /// session and is unloaded with it; `KeepAlive` restarts a process that exits, it does not
    /// outlive the session that owns the process. A shutdown ends it the same way, and it comes
    /// back at the next login rather than at the moment a release was due.
    ///
    /// That is deliberate, not an oversight. An agent runs as the user, which is what lets
    /// publishing use the Git credentials and SSH agent they already have; a `LaunchDaemon` would
    /// survive logout and then have no credentials to publish with.
    ///
    /// It also does not set a `StartInterval`: the daemon reconciles durable deadlines when it
    /// starts and when it wakes, rather than depending on a timer that does not fire while the
    /// machine is asleep.
    #[must_use]
    pub fn plist(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>--state-dir</string>
        <string>{state_dir}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
            label = xml_escape(&self.label),
            program = xml_escape(&self.program.to_string_lossy()),
            state_dir = xml_escape(&self.state_dir.to_string_lossy()),
            log = xml_escape(&self.log_path.to_string_lossy()),
        )
    }

    /// Writes the property list and asks launchd to load it.
    ///
    /// Installing twice is not an error: the definition is rewritten and reloaded, which is what
    /// an upgrade needs.
    pub fn install(&self) -> Result<(), InstallError> {
        if !self.program.exists() {
            return Err(InstallError::MissingProgram(self.program.clone()));
        }
        if let Some(parent) = self.plist_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&self.state_dir)?;
        fs::write(&self.plist_path, self.plist())?;

        // Replace any previous definition first, so an upgrade does not leave the old binary
        // running under the same label.
        let _ = self.bootout();
        launchctl(&[
            "bootstrap",
            &gui_domain(),
            &self.plist_path.to_string_lossy(),
        ])?;
        Ok(())
    }

    /// Stops the service and removes its definition, leaving queued work on disk.
    ///
    /// The state directory is never deleted here. Uninstalling the service is not the same
    /// decision as discarding captured work that has not been published.
    pub fn uninstall(&self) -> Result<(), InstallError> {
        let _ = self.bootout();
        if self.plist_path.exists() {
            fs::remove_file(&self.plist_path)?;
        }
        Ok(())
    }

    /// Reports whether launchd currently knows about this service.
    pub fn is_loaded(&self) -> bool {
        Command::new("launchctl")
            .args(["print", &format!("{}/{}", gui_domain(), self.label)])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn bootout(&self) -> Result<(), InstallError> {
        launchctl(&["bootout", &format!("{}/{}", gui_domain(), self.label)])
    }
}

/// The current user's launchd GUI domain.
fn gui_domain() -> String {
    // SAFETY-free equivalent of getuid(): the effective user is the one whose session this is.
    let uid = std::env::var("UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or_else(current_uid);
    format!("gui/{uid}")
}

fn current_uid() -> u32 {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn launchctl(arguments: &[&str]) -> Result<(), InstallError> {
    let output = Command::new("launchctl")
        .args(arguments)
        .output()
        .map_err(|error| InstallError::LaunchctlUnavailable(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(InstallError::Launchctl {
        command: arguments.join(" "),
        detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Escapes the five XML entities, so an unusual path cannot break the property list.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("service program {0} does not exist")]
    MissingProgram(PathBuf),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("launchctl is unavailable: {0}")]
    LaunchctlUnavailable(String),
    #[error("launchctl {command} failed: {detail}")]
    Launchctl { command: String, detail: String },
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn the_property_list_runs_the_daemon_against_its_own_state_directory() {
        let home = Path::new("/Users/example");
        let installation = ServiceInstallation::describe(
            "/usr/local/bin/reccursive-daemon",
            "/Users/example/.local/state/reccursive",
            home,
        );
        let plist = installation.plist();

        assert!(plist.contains("<string>com.reccursive.reccursive-daemon</string>"));
        assert!(plist.contains("<string>/usr/local/bin/reccursive-daemon</string>"));
        assert!(plist.contains("<string>--state-dir</string>"));
        assert!(plist.contains("<string>/Users/example/.local/state/reccursive</string>"));
        // Restarts after a crash and keeps running when the terminal closes. Not a logout: an
        // agent is unloaded with the session that owns it.
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n    <true/>"));
        // No timer: wake and start reconcile durable deadlines instead.
        assert!(!plist.contains("StartInterval"));
        assert_eq!(
            installation.plist_path,
            home.join("Library/LaunchAgents/com.reccursive.reccursive-daemon.plist")
        );
    }

    #[test]
    fn an_unusual_path_cannot_break_out_of_the_property_list() {
        let home = Path::new("/Users/example");
        let installation =
            ServiceInstallation::describe("/tmp/a&b/daemon", "/tmp/<state>/\"dir\"", home);
        let plist = installation.plist();

        assert!(plist.contains("/tmp/a&amp;b/daemon"));
        assert!(plist.contains("&lt;state&gt;"));
        assert!(plist.contains("&quot;dir&quot;"));
        // The raw characters never appear inside an element's text.
        assert!(!plist.contains("/tmp/a&b/daemon"));
        assert!(!plist.contains("<state>"));
    }

    #[test]
    fn installing_a_missing_program_is_refused_before_anything_is_written() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let installation = ServiceInstallation::describe(
            directory.path().join("not-built"),
            directory.path().join("state"),
            &home,
        );

        let outcome = installation.install();

        assert!(matches!(outcome, Err(InstallError::MissingProgram(_))));
        assert!(
            !installation.plist_path.exists(),
            "a refused installation must not leave a definition behind"
        );
    }

    #[test]
    fn uninstalling_removes_the_definition_and_keeps_queued_work() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let state_dir = directory.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let queued = state_dir.join("packages");
        fs::create_dir_all(&queued).unwrap();
        fs::write(queued.join("package.json"), "captured work").unwrap();

        let installation =
            ServiceInstallation::describe(directory.path().join("daemon"), &state_dir, &home);
        fs::create_dir_all(installation.plist_path.parent().unwrap()).unwrap();
        fs::write(&installation.plist_path, installation.plist()).unwrap();

        // launchctl may not be present or may refuse in a test environment; removing the
        // definition must still happen, and captured work must survive regardless.
        let _ = installation.uninstall();

        assert!(!installation.plist_path.exists());
        assert!(
            queued.join("package.json").exists(),
            "uninstalling the service must never discard captured work"
        );
    }
}
