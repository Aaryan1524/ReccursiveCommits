//! Durable local persistence boundary.

mod events;
mod migrations;
mod redaction;
mod repository;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub use events::{DEFAULT_EVENT_RETENTION, EventContext, EventSeverity, NewEvent, StoredEvent};
pub use migrations::STORAGE_SCHEMA_VERSION;
pub use redaction::{redact_json, redact_text};
pub use repository::{RepositoryRegistration, StoredRepository};
use rusqlite::{Connection, OpenFlags};
use thiserror::Error;
use uuid::Uuid;

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
        Self::initialize(connection)
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
        Self::verify_integrity(&connection)?;
        Ok(Self { connection })
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
}
