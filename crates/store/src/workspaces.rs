use std::{path::PathBuf, str::FromStr};

use reccursive_core::{FeatureId, Revision};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Store, StoreError};

/// Durable record for a daemon-owned build workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub feature_id: FeatureId,
    pub revision: Revision,
    pub path: PathBuf,
    pub base_commit: String,
    pub prerequisites: Value,
    pub created_at_unix_ms: i64,
}

impl WorkspaceRecord {
    pub fn validate(&self) -> Result<(), StoreError> {
        if !self.path.is_absolute() {
            return Err(StoreError::InvalidData(
                "owned workspace path must be absolute".into(),
            ));
        }
        if self.base_commit.len() != 40
            || !self
                .base_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(StoreError::InvalidData(
                "workspace base commit must be a full hexadecimal object ID".into(),
            ));
        }
        if !self.prerequisites.is_array() {
            return Err(StoreError::InvalidData(
                "workspace prerequisites must be a JSON array".into(),
            ));
        }
        if self.created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "workspace creation timestamp must not be negative".into(),
            ));
        }
        Ok(())
    }
}

impl Store {
    /// Persists ownership only after workspace construction succeeds.
    pub fn record_workspace(&mut self, record: &WorkspaceRecord) -> Result<(), StoreError> {
        record.validate()?;
        self.connection
            .execute(
                "INSERT INTO build_workspaces (
                    feature_id, plan_revision, workspace_path, base_commit,
                    prerequisites_json, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.feature_id.to_string(),
                    record.revision.get(),
                    record.path.to_string_lossy(),
                    record.base_commit,
                    serde_json::to_string(&record.prerequisites)?,
                    record.created_at_unix_ms,
                ],
            )
            .map_err(map_workspace_write_error)?;
        Ok(())
    }

    /// Loads the owned workspace for an exact plan revision.
    pub fn workspace(
        &self,
        feature_id: FeatureId,
        revision: Revision,
    ) -> Result<Option<WorkspaceRecord>, StoreError> {
        let raw: Option<(String, u32, String, String, String, i64)> = self
            .connection
            .query_row(
                "SELECT feature_id, plan_revision, workspace_path, base_commit,
                        prerequisites_json, created_at_unix_ms
                 FROM build_workspaces WHERE feature_id = ?1 AND plan_revision = ?2",
                params![feature_id.to_string(), revision.get()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(feature_id, revision, path, base_commit, prerequisites, created_at_unix_ms)| {
                let record = WorkspaceRecord {
                    feature_id: FeatureId::from_str(&feature_id)
                        .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                    revision: Revision::new(revision)
                        .map_err(|error| StoreError::InvalidData(error.to_string()))?,
                    path: PathBuf::from(path),
                    base_commit,
                    prerequisites: serde_json::from_str(&prerequisites)?,
                    created_at_unix_ms,
                };
                record.validate()?;
                Ok(record)
            },
        )
        .transpose()
    }
}

fn map_workspace_write_error(error: rusqlite::Error) -> StoreError {
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
    use crate::RepositoryRegistration;

    #[test]
    fn workspace_ownership_round_trips_and_is_unique() {
        let mut store = Store::open_in_memory().unwrap();
        let repository_id = RepositoryId::new();
        store
            .enroll_repository(
                &RepositoryRegistration::new(
                    repository_id,
                    "/tmp/source",
                    "ssh://git@example.invalid/repo.git",
                    "/tmp/managed.git",
                    1,
                )
                .unwrap(),
                &RepositoryPolicy::new(
                    repository_id,
                    Revision::FIRST,
                    PublicationMode::ScheduledCreation,
                    TargetRef::new("refs/heads/main").unwrap(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let plan = FeaturePlan {
            schema_version: PLAN_SCHEMA_VERSION,
            feature_id: FeatureId::new(),
            revision: Revision::FIRST,
            repository_id,
            goal: "Create an owned workspace".into(),
            target: TargetRef::new("refs/heads/main").unwrap(),
            sealed: true,
            phases: vec![PlanPhase {
                id: "capture".into(),
                name: "Capture".into(),
                tasks: vec![PlanTask {
                    id: TaskId::new(),
                    name: "Create workspace".into(),
                    dependencies: BTreeMap::new(),
                    acceptance_checks: vec![AcceptanceCheck {
                        id: "isolated".into(),
                        description: "Source remains unchanged".into(),
                    }],
                }],
            }],
        };
        store.import_plan(&plan, 2).unwrap();
        let record = WorkspaceRecord {
            feature_id: plan.feature_id,
            revision: plan.revision,
            path: PathBuf::from("/tmp/workspace"),
            base_commit: "a".repeat(40),
            prerequisites: json!([{ "path": "src/lib.rs", "state": "present" }]),
            created_at_unix_ms: 3,
        };
        store.record_workspace(&record).unwrap();
        assert_eq!(
            store.workspace(plan.feature_id, plan.revision).unwrap(),
            Some(record.clone())
        );
        assert!(matches!(
            store.record_workspace(&record),
            Err(StoreError::Conflict(_))
        ));
    }
}
