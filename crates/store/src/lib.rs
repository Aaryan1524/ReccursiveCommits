//! Durable local persistence boundary.

mod attempts;
mod checks;
mod events;
mod idempotency;
mod integrations;
mod migrations;
mod plans;
mod pull_requests;
mod redaction;
mod repository;
mod schedules;
mod snapshots;
mod states;
mod tasks;
mod units;
mod workspaces;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub use attempts::{AttemptLease, NewReleaseAttempt, ReleaseAttempt, TaskPublication};
pub use checks::{CandidateValidationEvidence, TrustedCheck, ValidationEvidence};
pub use events::{DEFAULT_EVENT_RETENTION, EventContext, EventSeverity, NewEvent, StoredEvent};
pub use idempotency::IdempotencyClaim;
pub use integrations::{GLOBAL_SCOPE, IntegrationHealth};
pub use migrations::STORAGE_SCHEMA_VERSION;
pub use plans::StoredPlan;
pub use pull_requests::PullRequestRecord;
pub use redaction::{redact_json, redact_text};
pub use repository::{RepositoryRegistration, StoredRepository, StoredSchedulePolicy};
use rusqlite::{Connection, OpenFlags};
pub use schedules::BlockingPrerequisite;
pub use schedules::{
    NewScheduleSlot, RepositoryPause, ScheduleRecalculation, ScheduleSlot, WithdrawnSlot,
};
pub use snapshots::{SnapshotRecord, SnapshotRecoveryIssue};
pub use tasks::{TaskDependency, TaskRecord};
use thiserror::Error;
pub use units::{LifecycleOutcome, ReleaseUnitRecord};
use uuid::Uuid;
pub use workspaces::WorkspaceRecord;

/// SQLite owner for queue and repository state.
pub struct Store {
    connection: Connection,
}

impl Store {
    /// Opens or creates a database and applies all committed migrations atomically.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        let store = Self::initialize(connection)?;
        // The database holds every repository path, plan, captured change and event this
        // installation knows about. SQLite creates it with the process umask, which is usually
        // world-readable, and the owner-only state directory above it is the only thing that has
        // been protecting it — a directory someone could later move it out of. The write-ahead
        // log and shared-memory files carry the same content, so they are restricted too.
        restrict_to_owner(path)?;
        Ok(store)
    }

    /// Opens an isolated in-memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(mut connection: Connection) -> Result<Self, StoreError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        migrations::apply(&mut connection)?;
        backfill_remote_identities(&mut connection)?;
        Self::verify_integrity(&connection)?;
        Ok(Self { connection })
    }

    /// Re-applies owner-only permissions to the database and its sidecar files.
    ///
    /// Called again after opening because SQLite recreates `-wal` and `-shm` as it pleases, so a
    /// permission set once at creation does not stay set.
    pub fn restrict_permissions(path: impl AsRef<Path>) -> Result<(), StoreError> {
        restrict_to_owner(path.as_ref())
    }

    /// Returns the schema version recorded in the open database.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Writes a consistent backup without overwriting an existing destination.
    pub fn create_backup(&self, destination: impl AsRef<Path>) -> Result<(), StoreError> {
        let destination = destination.as_ref();
        if destination.exists() {
            return Err(StoreError::BackupDestinationExists {
                path: destination.to_path_buf(),
            });
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        self.connection
            .execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
        Self::verify_file(destination)
    }

    /// Restores a verified backup into a new path without replacing existing data.
    pub fn restore_backup(
        backup: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<(), StoreError> {
        let backup = backup.as_ref();
        let destination = destination.as_ref();
        if destination.exists() {
            return Err(StoreError::RestoreDestinationExists {
                path: destination.to_path_buf(),
            });
        }
        Self::verify_file(backup)?;
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".reccursive-restore-{}.sqlite", Uuid::new_v4()));
        fs::copy(backup, &temporary)?;

        let restore_result = (|| {
            Self::verify_file(&temporary)?;
            let restored = Self::open(&temporary)?;
            drop(restored);
            fs::rename(&temporary, destination)?;
            Ok(())
        })();
        if restore_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        restore_result
    }

    fn verify_file(path: &Path) -> Result<(), StoreError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        migrations::reject_future_schema(&connection)?;
        Self::verify_integrity(&connection)
    }

    fn verify_integrity(connection: &Connection) -> Result<(), StoreError> {
        let result: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result != "ok" {
            return Err(StoreError::Integrity(result));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// Derives the remote identity for any row still carrying the empty default.
///
/// Deriving an identity means parsing a URL, which SQL cannot do, so this runs immediately after
/// migration and before anything reads these columns. It is idempotent: rows that already have an
/// identity are left alone, so it costs nothing on an already-migrated database.
fn backfill_remote_identities(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction()?;
    let pending: Vec<(String, String)> = {
        let mut statement = transaction
            .prepare("SELECT id, canonical_remote FROM repositories WHERE remote_identity = ''")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, address) in pending {
        transaction.execute(
            "UPDATE repositories SET remote_identity = ?2 WHERE id = ?1",
            rusqlite::params![id, reccursive_core::RemoteIdentity::of(&address).as_str()],
        )?;
    }

    let attempts: Vec<(String, String)> = {
        let mut statement = transaction.prepare(
            "SELECT attempt_id, target_remote FROM release_attempts
             WHERE target_remote_identity = ''",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (attempt_id, address) in attempts {
        transaction.execute(
            "UPDATE release_attempts SET target_remote_identity = ?2 WHERE attempt_id = ?1",
            rusqlite::params![
                attempt_id,
                reccursive_core::RemoteIdentity::of(&address).as_str()
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Storage, migration, and recovery errors.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "database schema {found} is newer than supported schema {supported}; upgrade the application"
    )]
    FutureSchema { found: u32, supported: u32 },
    #[error("database integrity check failed: {0}")]
    Integrity(String),
    #[error("repository policy belongs to {policy_repository}, not {registration_repository}")]
    PolicyRepositoryMismatch {
        registration_repository: reccursive_core::RepositoryId,
        policy_repository: reccursive_core::RepositoryId,
    },
    #[error("stored value is invalid: {0}")]
    InvalidData(String),
    #[error("stored record conflicts with existing state: {0}")]
    Conflict(String),
    #[error("backup destination already exists: {}", path.display())]
    BackupDestinationExists { path: PathBuf },
    #[error("restore destination already exists: {}", path.display())]
    RestoreDestinationExists { path: PathBuf },
}

/// Restricts a database file and its write-ahead sidecars to their owner.
///
/// A missing sidecar is not an error: SQLite creates `-wal` and `-shm` only in WAL mode and
/// removes them on a clean close, so their absence is the ordinary case rather than a fault.
fn restrict_to_owner(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    for candidate in [
        path.to_path_buf(),
        append_suffix(path, "-wal"),
        append_suffix(path, "-shm"),
    ] {
        match fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::from(error)),
        }
    }
    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_starts_on_latest_schema_with_foreign_keys() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), STORAGE_SCHEMA_VERSION);
        let enabled: bool = store
            .connection()
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert!(enabled);
    }

    #[test]
    fn the_database_and_its_write_ahead_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let mut store = Store::open(&path).unwrap();

        // Write something, so the write-ahead log actually exists. SQLite creates it lazily, and
        // checking permissions before any write would be checking a file that is not there yet.
        store
            .record_event(
                &crate::NewEvent::new(
                    1,
                    crate::EventContext::default(),
                    "test.event",
                    crate::EventSeverity::Info,
                    None,
                    "a write to force the write-ahead log into existence",
                    serde_json::Value::Null,
                )
                .unwrap(),
                10,
            )
            .unwrap();

        // The database holds every repository path, plan and captured change this installation
        // knows about; the sidecars hold the same content mid-flight. SQLite creates all three
        // with the process umask, which is usually world-readable.
        for suffix in ["", "-wal", "-shm"] {
            let candidate = append_suffix(&path, suffix);
            if !candidate.exists() {
                continue;
            }
            let mode = fs::metadata(&candidate).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "{} is readable by others (mode {mode:o})",
                candidate.display()
            );
        }
    }
}
