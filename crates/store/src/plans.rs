use reccursive_core::{FeatureId, FeaturePlan, Revision};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// A validated plan revision and the time it entered durable storage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoredPlan {
    pub plan: FeaturePlan,
    pub created_at_unix_ms: i64,
}

impl Store {
    /// Appends one immutable plan revision after enforcing contiguous history.
    pub fn import_plan(
        &mut self,
        plan: &FeaturePlan,
        created_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        plan.validate()
            .map_err(|error| StoreError::InvalidData(error.to_string()))?;
        if created_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "plan import timestamp must not be negative".into(),
            ));
        }

        let transaction = self.connection.transaction()?;
        let configured_target: Option<String> = transaction
            .query_row(
                "SELECT p.target_ref
                 FROM repositories r
                 JOIN repository_policies p
                   ON p.repository_id = r.id
                  AND p.revision = r.active_policy_revision
                 WHERE r.id = ?1",
                [plan.repository_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let configured_target = configured_target.ok_or_else(|| {
            StoreError::InvalidData(format!("repository {} does not exist", plan.repository_id))
        })?;
        if configured_target != plan.target.as_str() {
            return Err(StoreError::Conflict(format!(
                "plan target {} differs from repository target {configured_target}",
                plan.target.as_str()
            )));
        }
        let existing: Option<(u32, String)> = transaction
            .query_row(
                "SELECT revision, repository_id FROM feature_plans
                 WHERE feature_id = ?1 ORDER BY revision DESC LIMIT 1",
                [plan.feature_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let expected = match existing {
            Some((revision, repository_id)) => {
                if repository_id != plan.repository_id.to_string() {
                    return Err(StoreError::Conflict(
                        "a feature cannot move between repositories".into(),
                    ));
                }
                Revision::new(revision)
                    .and_then(Revision::next)
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?
            }
            None => Revision::FIRST,
        };
        if plan.revision != expected {
            return Err(StoreError::Conflict(format!(
                "feature {} expects revision {}, received {}",
                plan.feature_id,
                expected.get(),
                plan.revision.get()
            )));
        }

        transaction
            .execute(
                "INSERT INTO feature_plans (
                    feature_id, revision, repository_id, target_ref, sealed,
                    document_json, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    plan.feature_id.to_string(),
                    plan.revision.get(),
                    plan.repository_id.to_string(),
                    plan.target.as_str(),
                    plan.sealed,
                    serde_json::to_string(plan)?,
                    created_at_unix_ms,
                ],
            )
            .map_err(map_plan_write_error)?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads an exact plan revision, or the newest revision when omitted.
    pub fn plan(
        &self,
        feature_id: FeatureId,
        revision: Option<Revision>,
    ) -> Result<Option<StoredPlan>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT document_json, created_at_unix_ms FROM feature_plans
             WHERE feature_id = ?1 AND (?2 IS NULL OR revision = ?2)
             ORDER BY revision DESC LIMIT 1",
        )?;
        let raw: Option<(String, i64)> = statement
            .query_row(
                params![feature_id.to_string(), revision.map(Revision::get)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        raw.map(|(document, created_at_unix_ms)| {
            let plan: FeaturePlan = serde_json::from_str(&document)?;
            plan.validate()
                .map_err(|error| StoreError::InvalidData(error.to_string()))?;
            Ok(StoredPlan {
                plan,
                created_at_unix_ms,
            })
        })
        .transpose()
    }

    /// Lists all immutable revisions for a feature in ascending order.
    pub fn plan_history(&self, feature_id: FeatureId) -> Result<Vec<StoredPlan>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT document_json, created_at_unix_ms FROM feature_plans
             WHERE feature_id = ?1 ORDER BY revision",
        )?;
        statement
            .query_map([feature_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .map(|raw| {
                let (document, created_at_unix_ms) = raw?;
                let plan: FeaturePlan = serde_json::from_str(&document)?;
                plan.validate()
                    .map_err(|error| StoreError::InvalidData(error.to_string()))?;
                Ok(StoredPlan {
                    plan,
                    created_at_unix_ms,
                })
            })
            .collect()
    }
}

fn map_plan_write_error(error: rusqlite::Error) -> StoreError {
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
        AcceptanceCheck, PLAN_SCHEMA_VERSION, PlanPhase, PlanTask, PublicationMode, RepositoryId,
        RepositoryPolicy, TargetRef,
    };

    use super::*;
    use crate::RepositoryRegistration;

    fn fixture(store: &mut Store) -> FeaturePlan {
        let repository_id = RepositoryId::new();
        let registration = RepositoryRegistration::new(
            repository_id,
            "/tmp/example",
            "ssh://git@example.invalid/project.git",
            "/tmp/managed/example.git",
            1,
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
        store.enroll_repository(&registration, &policy).unwrap();
        FeaturePlan {
            schema_version: PLAN_SCHEMA_VERSION,
            feature_id: FeatureId::new(),
            revision: Revision::FIRST,
            repository_id,
            goal: "Ship the import path".into(),
            target: TargetRef::new("refs/heads/main").unwrap(),
            sealed: true,
            phases: vec![PlanPhase {
                id: "delivery".into(),
                name: "Delivery".into(),
                tasks: vec![PlanTask {
                    id: reccursive_core::TaskId::new(),
                    name: "Import plan".into(),
                    dependencies: BTreeMap::new(),
                    acceptance_checks: vec![AcceptanceCheck {
                        id: "persisted".into(),
                        description: "Plan survives restart".into(),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn revisions_are_append_only_and_queryable() {
        let mut store = Store::open_in_memory().unwrap();
        let first = fixture(&mut store);
        store.import_plan(&first, 2).unwrap();
        let mut second = first.clone();
        second.revision = first.revision.next().unwrap();
        second.goal = "Ship revised import path".into();
        store.import_plan(&second, 3).unwrap();

        assert_eq!(
            store.plan(first.feature_id, None).unwrap().unwrap().plan,
            second
        );
        let history = store.plan_history(first.feature_id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].plan, first);
    }

    #[test]
    fn skipped_revisions_and_repository_moves_are_rejected() {
        let mut store = Store::open_in_memory().unwrap();
        let first = fixture(&mut store);
        store.import_plan(&first, 2).unwrap();
        let mut skipped = first.clone();
        skipped.revision = Revision::new(3).unwrap();
        assert!(matches!(
            store.import_plan(&skipped, 3),
            Err(StoreError::Conflict(_))
        ));

        let mut moved = first.clone();
        moved.revision = Revision::new(2).unwrap();
        moved.repository_id = RepositoryId::new();
        assert!(matches!(
            store.import_plan(&moved, 3),
            Err(StoreError::InvalidData(_))
        ));

        let mut retargeted = first.clone();
        retargeted.revision = Revision::new(2).unwrap();
        retargeted.target = TargetRef::new("refs/heads/release").unwrap();
        assert!(matches!(
            store.import_plan(&retargeted, 3),
            Err(StoreError::Conflict(_))
        ));
    }
}
