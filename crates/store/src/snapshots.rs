use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use reccursive_core::{FeatureId, PackageId, Revision};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Store, StoreError};

/// Durable pointer and authenticated metadata for an immutable snapshot package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub package_id: PackageId,
    pub revision: Revision,
    pub feature_id: FeatureId,
    pub plan_revision: Revision,
    pub path: PathBuf,
    pub base_tree: String,
    pub result_tree: String,
    pub content_hash: String,
    pub manifest: Value,
    pub created_at_unix_ms: i64,
}

/// A durable, operator-visible problem discovered while recovering package storage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecoveryIssue {
    pub path: PathBuf,
    pub kind: String,
    pub message: String,
    pub first_seen_at_unix_ms: i64,
    pub last_seen_at_unix_ms: i64,
}

impl SnapshotRecoveryIssue {
    fn validate(&self) -> Result<(), StoreError> {
        if !self.path.is_absolute()
            || self.kind.trim().is_empty()
            || self.message.trim().is_empty()
            || self.first_seen_at_unix_ms < 0
            || self.last_seen_at_unix_ms < self.first_seen_at_unix_ms
        {
            return Err(StoreError::InvalidData(
                "snapshot recovery issue is invalid".into(),
            ));
        }
        Ok(())
    }
}

impl SnapshotRecord {
    fn validate(&self) -> Result<(), StoreError> {
        if !self.path.is_absolute() {
            return Err(StoreError::InvalidData(
                "snapshot path must be absolute".into(),
            ));
        }
        for (name, value, length) in [
            ("base tree", self.base_tree.as_str(), 40),
            ("result tree", self.result_tree.as_str(), 40),
            ("content hash", self.content_hash.as_str(), 64),
        ] {
            if value.len() != length || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(StoreError::InvalidData(format!(
                    "snapshot {name} is invalid"
                )));
            }
        }
        if !self.manifest.is_object() || self.created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "snapshot metadata is invalid".into(),
            ));
        }
        Ok(())
    }
}

impl Store {
    pub fn record_snapshot(&mut self, record: &SnapshotRecord) -> Result<(), StoreError> {
        record.validate()?;
        self.connection
            .execute(
                "INSERT INTO snapshot_packages (
                    package_id, revision, feature_id, plan_revision, package_path,
                    base_tree, result_tree, content_hash, manifest_json, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    record.package_id.to_string(),
                    record.revision.get(),
                    record.feature_id.to_string(),
                    record.plan_revision.get(),
                    record.path.to_string_lossy(),
                    record.base_tree,
                    record.result_tree,
                    record.content_hash,
                    serde_json::to_string(&record.manifest)?,
                    record.created_at_unix_ms,
                ],
            )
            .map_err(map_snapshot_write_error)?;
        Ok(())
    }

    pub fn snapshot(
        &self,
        package_id: PackageId,
        revision: Revision,
    ) -> Result<Option<SnapshotRecord>, StoreError> {
        let raw: Option<RawSnapshot> = self
            .connection
            .query_row(
                "SELECT package_id, revision, feature_id, plan_revision, package_path,
                            base_tree, result_tree, content_hash, manifest_json, created_at_unix_ms
                     FROM snapshot_packages WHERE package_id = ?1 AND revision = ?2",
                params![package_id.to_string(), revision.get()],
                RawSnapshot::from_row,
            )
            .optional()?;
        raw.map(TryInto::try_into).transpose()
    }

    /// Lists every immutable snapshot record in stable capture order.
    pub fn snapshots(&self) -> Result<Vec<SnapshotRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT package_id, revision, feature_id, plan_revision, package_path,
                    base_tree, result_tree, content_hash, manifest_json, created_at_unix_ms
             FROM snapshot_packages ORDER BY created_at_unix_ms, package_id, revision",
        )?;
        statement
            .query_map([], RawSnapshot::from_row)?
            .map(|row| row.map_err(StoreError::from)?.try_into())
            .collect()
    }

    /// Upserts an unresolved recovery issue without erasing when it was first observed.
    pub fn record_snapshot_recovery_issue(
        &mut self,
        issue: &SnapshotRecoveryIssue,
    ) -> Result<(), StoreError> {
        issue.validate()?;
        self.connection.execute(
            "INSERT INTO snapshot_recovery_issues (
                package_path, kind, message, first_seen_at_unix_ms, last_seen_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(package_path) DO UPDATE SET
                kind = excluded.kind,
                message = excluded.message,
                last_seen_at_unix_ms = excluded.last_seen_at_unix_ms",
            params![
                issue.path.to_string_lossy(),
                issue.kind,
                issue.message,
                issue.first_seen_at_unix_ms,
                issue.last_seen_at_unix_ms,
            ],
        )?;
        Ok(())
    }

    /// Clears an issue only after the matching package path has been verified again.
    pub fn clear_snapshot_recovery_issue(&mut self, path: &Path) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM snapshot_recovery_issues WHERE package_path = ?1",
            [path.to_string_lossy().as_ref()],
        )?;
        Ok(())
    }

    /// Lists unresolved recovery issues newest first.
    pub fn snapshot_recovery_issues(&self) -> Result<Vec<SnapshotRecoveryIssue>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT package_path, kind, message, first_seen_at_unix_ms, last_seen_at_unix_ms
             FROM snapshot_recovery_issues ORDER BY last_seen_at_unix_ms DESC, package_path",
        )?;
        statement
            .query_map([], |row| {
                Ok(SnapshotRecoveryIssue {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    kind: row.get(1)?,
                    message: row.get(2)?,
                    first_seen_at_unix_ms: row.get(3)?,
                    last_seen_at_unix_ms: row.get(4)?,
                })
            })?
            .map(|row| {
                let issue = row?;
                issue.validate()?;
                Ok(issue)
            })
            .collect()
    }
}

struct RawSnapshot {
    package_id: String,
    revision: u32,
    feature_id: String,
    plan_revision: u32,
    path: String,
    base_tree: String,
    result_tree: String,
    content_hash: String,
    manifest: String,
    created_at_unix_ms: i64,
}

impl RawSnapshot {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            package_id: row.get(0)?,
            revision: row.get(1)?,
            feature_id: row.get(2)?,
            plan_revision: row.get(3)?,
            path: row.get(4)?,
            base_tree: row.get(5)?,
            result_tree: row.get(6)?,
            content_hash: row.get(7)?,
            manifest: row.get(8)?,
            created_at_unix_ms: row.get(9)?,
        })
    }
}

impl TryFrom<RawSnapshot> for SnapshotRecord {
    type Error = StoreError;

    fn try_from(raw: RawSnapshot) -> Result<Self, Self::Error> {
        let record = Self {
            package_id: PackageId::from_str(&raw.package_id)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            revision: Revision::new(raw.revision)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            feature_id: FeatureId::from_str(&raw.feature_id)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            plan_revision: Revision::new(raw.plan_revision)
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            path: PathBuf::from(raw.path),
            base_tree: raw.base_tree,
            result_tree: raw.result_tree,
            content_hash: raw.content_hash,
            manifest: serde_json::from_str(&raw.manifest)?,
            created_at_unix_ms: raw.created_at_unix_ms,
        };
        record.validate()?;
        Ok(record)
    }
}

fn map_snapshot_write_error(error: rusqlite::Error) -> StoreError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        StoreError::Conflict(error.to_string())
    } else {
        StoreError::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_issue_preserves_first_observation_and_updates_last_observation() {
        let mut store = Store::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/reccursive-recovery-fixture");
        store
            .record_snapshot_recovery_issue(&SnapshotRecoveryIssue {
                path: path.clone(),
                kind: "incomplete_capture".into(),
                message: "first observation".into(),
                first_seen_at_unix_ms: 10,
                last_seen_at_unix_ms: 10,
            })
            .unwrap();
        store
            .record_snapshot_recovery_issue(&SnapshotRecoveryIssue {
                path,
                kind: "invalid_package".into(),
                message: "latest observation".into(),
                first_seen_at_unix_ms: 20,
                last_seen_at_unix_ms: 30,
            })
            .unwrap();

        let issues = store.snapshot_recovery_issues().unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "invalid_package");
        assert_eq!(issues[0].message, "latest observation");
        assert_eq!(issues[0].first_seen_at_unix_ms, 10);
        assert_eq!(issues[0].last_seen_at_unix_ms, 30);
    }
}
