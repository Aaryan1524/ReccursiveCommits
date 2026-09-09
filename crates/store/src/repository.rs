use std::str::FromStr;

use reccursive_core::{
    PolicyError, PublicationMode, RepositoryId, RepositoryPolicy, Revision, TargetRef,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// Repository details captured during enrollment before capability authorization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryRegistration {
    pub id: RepositoryId,
    pub checkout_path: String,
    pub canonical_remote: String,
    pub managed_path: String,
    pub created_at_unix_ms: i64,
}

impl RepositoryRegistration {
    pub fn new(
        id: RepositoryId,
        checkout_path: impl Into<String>,
        canonical_remote: impl Into<String>,
        managed_path: impl Into<String>,
        created_at_unix_ms: i64,
    ) -> Result<Self, StoreError> {
        let value = Self {
            id,
            checkout_path: checkout_path.into(),
            canonical_remote: canonical_remote.into(),
            managed_path: managed_path.into(),
            created_at_unix_ms,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), StoreError> {
        for (field, value) in [
            ("checkout_path", self.checkout_path.as_str()),
            ("canonical_remote", self.canonical_remote.as_str()),
            ("managed_path", self.managed_path.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(StoreError::InvalidData(format!(
                    "repository {field} must not be empty"
                )));
            }
        }
        if self.created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "repository creation timestamp must not be negative".into(),
            ));
        }
        Ok(())
    }
}

/// Repository and its currently active policy as returned by the store.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoredRepository {
    pub registration: RepositoryRegistration,
    pub active_policy: RepositoryPolicy,
}

impl Store {
    /// Atomically stores a repository and its first active policy.
    pub fn enroll_repository(
        &mut self,
        registration: &RepositoryRegistration,
        policy: &RepositoryPolicy,
    ) -> Result<(), StoreError> {
        registration.validate()?;
        ensure_policy_owner(registration.id, policy)?;
        let transaction = self.connection.transaction()?;
        transaction
            .execute(
                "INSERT INTO repositories (
                id, checkout_path, canonical_remote, managed_path, created_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    registration.id.to_string(),
                    registration.checkout_path,
                    registration.canonical_remote,
                    registration.managed_path,
                    registration.created_at_unix_ms,
                ],
            )
            .map_err(map_write_error)?;
        insert_policy(&transaction, policy, registration.created_at_unix_ms)?;
        transaction.execute(
            "UPDATE repositories SET active_policy_revision = ?2 WHERE id = ?1",
            params![registration.id.to_string(), policy.revision.get()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Adds and activates a new policy revision in one transaction.
    pub fn activate_policy_revision(
        &mut self,
        policy: &RepositoryPolicy,
        created_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        if created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "policy creation timestamp must not be negative".into(),
            ));
        }
        let transaction = self.connection.transaction()?;
        insert_policy(&transaction, policy, created_at_unix_ms)?;
        let changed = transaction
            .execute(
                "UPDATE repositories SET active_policy_revision = ?2 WHERE id = ?1",
                params![policy.repository_id.to_string(), policy.revision.get()],
            )
            .map_err(map_write_error)?;
        if changed != 1 {
            return Err(StoreError::InvalidData(format!(
                "repository {} does not exist",
                policy.repository_id
            )));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Loads a repository and its active policy.
    pub fn repository(
        &self,
        repository_id: RepositoryId,
    ) -> Result<Option<StoredRepository>, StoreError> {
        let raw = self
            .connection
            .query_row(
                ACTIVE_REPOSITORY_QUERY,
                [repository_id.to_string()],
                RawStoredRepository::from_row,
            )
            .optional()?;
        raw.map(TryInto::try_into).transpose()
    }

    /// Lists repositories in stable identifier order.
    pub fn repositories(&self) -> Result<Vec<StoredRepository>, StoreError> {
        let mut statement = self
            .connection
            .prepare(&format!("{ACTIVE_REPOSITORY_QUERY} ORDER BY r.id"))?;
        statement
            .query_map([Option::<String>::None], RawStoredRepository::from_row)?
            .map(|result| result.map_err(StoreError::from).and_then(TryInto::try_into))
            .collect()
    }
}

const ACTIVE_REPOSITORY_QUERY: &str = "
    SELECT
        r.id,
        r.checkout_path,
        r.canonical_remote,
        r.managed_path,
        r.created_at_unix_ms,
        p.revision,
        p.publication_mode,
        p.target_ref,
        p.development_target_ref
    FROM repositories r
    JOIN repository_policies p
      ON p.repository_id = r.id
     AND p.revision = r.active_policy_revision
    WHERE r.id = COALESCE(?1, r.id)
";

fn insert_policy(
    connection: &rusqlite::Connection,
    policy: &RepositoryPolicy,
    created_at_unix_ms: i64,
) -> Result<(), StoreError> {
    connection
        .execute(
            "INSERT INTO repository_policies (
            repository_id, revision, publication_mode, target_ref,
            development_target_ref, created_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                policy.repository_id.to_string(),
                policy.revision.get(),
                publication_mode_name(policy.publication_mode),
                policy.target.as_str(),
                policy.development_target.as_ref().map(TargetRef::as_str),
                created_at_unix_ms,
            ],
        )
        .map_err(map_write_error)?;
    Ok(())
}

fn ensure_policy_owner(
    registration_id: RepositoryId,
    policy: &RepositoryPolicy,
) -> Result<(), StoreError> {
    if registration_id != policy.repository_id {
        return Err(StoreError::PolicyRepositoryMismatch {
            registration_repository: registration_id,
            policy_repository: policy.repository_id,
        });
    }
    Ok(())
}

const fn publication_mode_name(mode: PublicationMode) -> &'static str {
    match mode {
        PublicationMode::ScheduledCreation => "scheduled_creation",
        PublicationMode::ImmediateAvailability => "immediate_availability",
    }
}

fn parse_publication_mode(value: &str) -> Result<PublicationMode, StoreError> {
    match value {
        "scheduled_creation" => Ok(PublicationMode::ScheduledCreation),
        "immediate_availability" => Ok(PublicationMode::ImmediateAvailability),
        _ => Err(StoreError::InvalidData(format!(
            "unknown publication mode {value:?}"
        ))),
    }
}

struct RawStoredRepository {
    id: String,
    checkout_path: String,
    canonical_remote: String,
    managed_path: String,
    created_at_unix_ms: i64,
    policy_revision: u32,
    publication_mode: String,
    target_ref: String,
    development_target_ref: Option<String>,
}

impl RawStoredRepository {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            checkout_path: row.get(1)?,
            canonical_remote: row.get(2)?,
            managed_path: row.get(3)?,
            created_at_unix_ms: row.get(4)?,
            policy_revision: row.get(5)?,
            publication_mode: row.get(6)?,
            target_ref: row.get(7)?,
            development_target_ref: row.get(8)?,
        })
    }
}

impl TryFrom<RawStoredRepository> for StoredRepository {
    type Error = StoreError;

    fn try_from(raw: RawStoredRepository) -> Result<Self, Self::Error> {
        let id = RepositoryId::from_str(&raw.id)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let registration = RepositoryRegistration::new(
            id,
            raw.checkout_path,
            raw.canonical_remote,
            raw.managed_path,
            raw.created_at_unix_ms,
        )?;
        let revision = Revision::new(raw.policy_revision)
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        let target = TargetRef::new(raw.target_ref).map_err(policy_data_error)?;
        let development_target = raw
            .development_target_ref
            .map(TargetRef::new)
            .transpose()
            .map_err(policy_data_error)?;
        let active_policy = RepositoryPolicy::new(
            id,
            revision,
            parse_publication_mode(&raw.publication_mode)?,
            target,
            development_target,
        )
        .map_err(policy_data_error)?;
        Ok(Self {
            registration,
            active_policy,
        })
    }
}

fn policy_data_error(error: PolicyError) -> StoreError {
    StoreError::InvalidData(error.to_string())
}

fn map_write_error(error: rusqlite::Error) -> StoreError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        StoreError::Conflict(error.to_string())
    } else {
        StoreError::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture() -> (RepositoryRegistration, RepositoryPolicy) {
        let repository_id = RepositoryId::new();
        let registration = RepositoryRegistration::new(
            repository_id,
            "/tmp/example",
            "ssh://git@example.invalid/project.git",
            "/tmp/managed/example.git",
            1_780_000_000_000,
        )
        .unwrap();
        let policy = RepositoryPolicy::new(
            repository_id,
            Revision::FIRST,
            PublicationMode::ScheduledCreation,
            TargetRef::new("refs/heads/main").unwrap(),
            None,
        )
        .unwrap();
        (registration, policy)
    }

    #[test]
    fn restart_preserves_repository_and_active_policy() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.sqlite");
        let (registration, policy) = fixture();
        {
            let mut store = Store::open(&database).unwrap();
            store.enroll_repository(&registration, &policy).unwrap();
        }

        let store = Store::open(&database).unwrap();
        assert_eq!(
            store.repository(registration.id).unwrap(),
            Some(StoredRepository {
                registration,
                active_policy: policy,
            })
        );
    }

    #[test]
    fn policy_activation_is_versioned_and_persistent() {
        let mut store = Store::open_in_memory().unwrap();
        let (registration, policy) = fixture();
        store.enroll_repository(&registration, &policy).unwrap();
        let next = RepositoryPolicy::new(
            registration.id,
            Revision::new(2).unwrap(),
            PublicationMode::ImmediateAvailability,
            TargetRef::new("refs/heads/main").unwrap(),
            Some(TargetRef::new("refs/heads/automation/work").unwrap()),
        )
        .unwrap();
        store
            .activate_policy_revision(&next, registration.created_at_unix_ms + 1)
            .unwrap();
        assert_eq!(
            store
                .repository(registration.id)
                .unwrap()
                .unwrap()
                .active_policy,
            next
        );
    }

    #[test]
    fn mismatched_policy_rolls_back_enrollment() {
        let mut store = Store::open_in_memory().unwrap();
        let (registration, policy) = fixture();
        let mismatched = RepositoryPolicy::new(
            RepositoryId::new(),
            policy.revision,
            policy.publication_mode,
            policy.target.clone(),
            None,
        )
        .unwrap();
        assert!(matches!(
            store.enroll_repository(&registration, &mismatched),
            Err(StoreError::PolicyRepositoryMismatch { .. })
        ));
        assert!(store.repositories().unwrap().is_empty());
    }

    #[test]
    fn verified_backup_restores_to_a_new_database() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.sqlite");
        let backup = directory.path().join("backup.sqlite");
        let restored_database = directory.path().join("restored.sqlite");
        let (registration, policy) = fixture();

        let mut store = Store::open(&database).unwrap();
        store.enroll_repository(&registration, &policy).unwrap();
        store.create_backup(&backup).unwrap();
        drop(store);

        Store::restore_backup(&backup, &restored_database).unwrap();
        let restored = Store::open(&restored_database).unwrap();
        assert_eq!(
            restored.repository(registration.id).unwrap().unwrap(),
            StoredRepository {
                registration,
                active_policy: policy,
            }
        );
    }

    #[test]
    fn backup_and_restore_never_overwrite_destinations() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.sqlite");
        let destination = directory.path().join("exists.sqlite");
        std::fs::write(&destination, "keep me").unwrap();
        let store = Store::open(&database).unwrap();
        assert!(matches!(
            store.create_backup(&destination),
            Err(StoreError::BackupDestinationExists { .. })
        ));
        assert!(matches!(
            Store::restore_backup(&database, &destination),
            Err(StoreError::RestoreDestinationExists { .. })
        ));
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "keep me");
    }
}
