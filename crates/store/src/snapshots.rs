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
    /// The unit this one builds on in the same workspace; `None` for the first unit.
    pub parent_package_id: Option<PackageId>,
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
        // snapshot_packages is keyed on (package_id, revision), so the parent link cannot be a SQL
        // foreign key. A cycle is always invalid and is refused here. Whether the parent exists is
        // deliberately not checked at write time: crash recovery re-registers packages in
        // filesystem order, so a child can legitimately be written before its parent. Dangling
        // links are reported by `dangling_package_parents` once recovery has finished.
        if record.parent_package_id == Some(record.package_id) {
            return Err(StoreError::InvalidData(
                "a package cannot be its own parent".into(),
            ));
        }
        self.connection
            .execute(
                "INSERT INTO snapshot_packages (
                    package_id, revision, feature_id, plan_revision, package_path,
                    base_tree, result_tree, content_hash, manifest_json, created_at_unix_ms,
                    parent_package_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                    record.parent_package_id.map(|id| id.to_string()),
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
                            base_tree, result_tree, content_hash, manifest_json, created_at_unix_ms,
                            parent_package_id
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
                    base_tree, result_tree, content_hash, manifest_json, created_at_unix_ms,
                    parent_package_id
             FROM snapshot_packages ORDER BY created_at_unix_ms, package_id, revision",
        )?;
        statement
            .query_map([], RawSnapshot::from_row)?
            .map(|row| row.map_err(StoreError::from)?.try_into())
            .collect()
    }

    /// Returns the head of one workspace's unit chain: the package nothing else was built on.
    ///
    /// This is the parent of the next unit captured from that workspace. The head is defined by
    /// the chain rather than by a timestamp on purpose — two units captured inside the same
    /// millisecond would otherwise order arbitrarily and silently fork the chain. The timestamp
    /// only breaks ties between several heads, which can exist just after a broken recovery.
    pub fn latest_workspace_snapshot(
        &self,
        feature_id: FeatureId,
        plan_revision: Revision,
    ) -> Result<Option<SnapshotRecord>, StoreError> {
        let raw: Option<RawSnapshot> = self
            .connection
            .query_row(
                "SELECT head.package_id, head.revision, head.feature_id, head.plan_revision,
                        head.package_path, head.base_tree, head.result_tree, head.content_hash,
                        head.manifest_json, head.created_at_unix_ms, head.parent_package_id
                 FROM snapshot_packages AS head
                 WHERE head.feature_id = ?1 AND head.plan_revision = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM snapshot_packages AS child
                       WHERE child.feature_id = head.feature_id
                         AND child.plan_revision = head.plan_revision
                         AND child.parent_package_id = head.package_id
                   )
                 ORDER BY head.created_at_unix_ms DESC, head.package_id DESC LIMIT 1",
                params![feature_id.to_string(), plan_revision.get()],
                RawSnapshot::from_row,
            )
            .optional()?;
        raw.map(TryInto::try_into).transpose()
    }

    /// Lists packages whose recorded parent unit has no stored row.
    ///
    /// Run after crash recovery, when every package that is going to be re-registered has been.
    /// A link that is still dangling means the chain a unit was built on is missing, so the unit
    /// cannot be placed in release order.
    pub fn dangling_package_parents(&self) -> Result<Vec<(PackageId, PackageId)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT child.package_id, child.parent_package_id
             FROM snapshot_packages AS child
             WHERE child.parent_package_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM snapshot_packages AS parent
                   WHERE parent.package_id = child.parent_package_id
               )
             ORDER BY child.package_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (child, parent) = row?;
            Ok((
                PackageId::from_str(&child)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                PackageId::from_str(&parent)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?,
            ))
        })
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
    parent_package_id: Option<String>,
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
            parent_package_id: row.get(10)?,
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
            parent_package_id: raw
                .parent_package_id
                .map(|id| PackageId::from_str(&id))
                .transpose()
                .map_err(|error| StoreError::InvalidData(error.to_string()))?,
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
    use std::collections::BTreeMap;

    use reccursive_core::{
        AcceptanceCheck, FeaturePlan, PLAN_SCHEMA_VERSION, PlanPhase, PlanTask, PublicationMode,
        RepositoryId, RepositoryPolicy, TargetRef, TaskId,
    };
    use serde_json::json;

    use super::*;
    use crate::{RepositoryRegistration, WorkspaceRecord};

    fn object_id(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    /// Builds the enrollment → plan → workspace chain a snapshot row depends on.
    fn workspace_fixture(store: &mut Store) -> FeatureId {
        let repository_id = RepositoryId::new();
        let target = TargetRef::new("refs/heads/main").unwrap();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/example",
                    "ssh://git@example.invalid/project.git",
                    "/tmp/managed/example.git",
                    1,
                )
                .unwrap(),
                &RepositoryPolicy::new(
                    repository_id,
                    Revision::FIRST,
                    PublicationMode::ScheduledCreation,
                    target.clone(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let feature_id = FeatureId::new();
        store
            .import_plan(
                &FeaturePlan {
                    schema_version: PLAN_SCHEMA_VERSION,
                    feature_id,
                    revision: Revision::FIRST,
                    repository_id,
                    goal: "Capture ordered units".into(),
                    target,
                    sealed: true,
                    phases: vec![PlanPhase {
                        id: "delivery".into(),
                        name: "Delivery".into(),
                        tasks: vec![PlanTask {
                            id: TaskId::new(),
                            name: "Build".into(),
                            dependencies: BTreeMap::new(),
                            acceptance_checks: vec![AcceptanceCheck {
                                id: "c1".into(),
                                description: "it works".into(),
                            }],
                        }],
                    }],
                },
                1,
            )
            .unwrap();
        store
            .record_workspace(&WorkspaceRecord {
                feature_id,
                revision: Revision::FIRST,
                path: "/tmp/managed/workspace".into(),
                base_commit: object_id('a'),
                prerequisites: json!([]),
                created_at_unix_ms: 1,
            })
            .unwrap();
        feature_id
    }

    fn unit(feature_id: FeatureId, name: char, parent: Option<PackageId>) -> SnapshotRecord {
        SnapshotRecord {
            package_id: PackageId::new(),
            revision: Revision::FIRST,
            feature_id,
            plan_revision: Revision::FIRST,
            path: PathBuf::from(format!("/tmp/managed/package-{name}")),
            base_tree: object_id(name),
            result_tree: object_id(name),
            content_hash: std::iter::repeat_n(name, 64).collect(),
            parent_package_id: parent,
            manifest: json!({ "paths": [] }),
            created_at_unix_ms: 1,
        }
    }

    #[test]
    fn schema_nine_drops_the_unused_entity_tables() {
        let store = Store::open_in_memory().unwrap();
        for table in [
            "features",
            "tasks",
            "task_dependencies",
            "packages",
            "package_tasks",
        ] {
            let present: bool = store
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master
                     WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!present, "{table} was never used and must not survive v9");
        }
    }

    #[test]
    fn unit_chain_is_queryable_and_a_package_cannot_parent_itself() {
        let mut store = Store::open_in_memory().unwrap();
        let feature_id = workspace_fixture(&mut store);

        let first = unit(feature_id, 'b', None);
        store.record_snapshot(&first).unwrap();
        let second = unit(feature_id, 'c', Some(first.package_id));
        store.record_snapshot(&second).unwrap();

        // Both units share a capture millisecond, so only the chain can identify the head.
        assert_eq!(first.created_at_unix_ms, second.created_at_unix_ms);
        let latest = store
            .latest_workspace_snapshot(feature_id, Revision::FIRST)
            .unwrap()
            .unwrap();
        assert_eq!(latest.package_id, second.package_id);
        assert_eq!(latest.parent_package_id, Some(first.package_id));

        let reloaded = store
            .snapshot(second.package_id, Revision::FIRST)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.parent_package_id, Some(first.package_id));

        let mut cyclic = unit(feature_id, 'd', None);
        cyclic.parent_package_id = Some(cyclic.package_id);
        assert!(
            matches!(
                store.record_snapshot(&cyclic),
                Err(StoreError::InvalidData(_))
            ),
            "a package must not be its own parent"
        );
    }

    #[test]
    fn a_parent_that_never_recovers_is_reported_as_a_broken_chain() {
        let mut store = Store::open_in_memory().unwrap();
        let feature_id = workspace_fixture(&mut store);

        // Recovery re-registers packages in filesystem order, so a child may legitimately be
        // written before its parent: the write itself must not fail.
        let orphan = unit(feature_id, 'b', Some(PackageId::new()));
        store.record_snapshot(&orphan).unwrap();

        let dangling = store.dangling_package_parents().unwrap();
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].0, orphan.package_id);
        assert_eq!(Some(dangling[0].1), orphan.parent_package_id);

        // Once the parent is stored, the chain is whole and nothing is reported.
        let mut parent = unit(feature_id, 'c', None);
        parent.package_id = orphan.parent_package_id.unwrap();
        store.record_snapshot(&parent).unwrap();
        assert!(store.dangling_package_parents().unwrap().is_empty());
    }

    #[test]
    fn workspace_base_advances_so_the_next_unit_starts_where_the_last_one_ended() {
        let mut store = Store::open_in_memory().unwrap();
        let feature_id = workspace_fixture(&mut store);
        assert_eq!(
            store
                .workspace(feature_id, Revision::FIRST)
                .unwrap()
                .unwrap()
                .base_commit,
            object_id('a')
        );

        store
            .advance_workspace_base(feature_id, Revision::FIRST, &object_id('e'))
            .unwrap();
        assert_eq!(
            store
                .workspace(feature_id, Revision::FIRST)
                .unwrap()
                .unwrap()
                .base_commit,
            object_id('e')
        );

        assert!(matches!(
            store.advance_workspace_base(feature_id, Revision::FIRST, "not-an-object-id"),
            Err(StoreError::InvalidData(_))
        ));
        assert!(matches!(
            store.advance_workspace_base(FeatureId::new(), Revision::FIRST, &object_id('f')),
            Err(StoreError::InvalidData(_))
        ));
    }

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
