use std::{path::PathBuf, str::FromStr};

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
