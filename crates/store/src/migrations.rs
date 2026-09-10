use rusqlite::{Connection, TransactionBehavior};

use crate::StoreError;

/// Latest schema understood by this build.
pub const STORAGE_SCHEMA_VERSION: u32 = 7;

struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
        CREATE TABLE repositories (
            id TEXT PRIMARY KEY,
            checkout_path TEXT NOT NULL CHECK (length(checkout_path) > 0),
            canonical_remote TEXT NOT NULL CHECK (length(canonical_remote) > 0),
            managed_path TEXT NOT NULL UNIQUE CHECK (length(managed_path) > 0),
            created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
            active_policy_revision INTEGER CHECK (active_policy_revision > 0),
            UNIQUE (canonical_remote, checkout_path),
            FOREIGN KEY (id, active_policy_revision)
                REFERENCES repository_policies(repository_id, revision)
                DEFERRABLE INITIALLY DEFERRED
        );

        CREATE TABLE repository_policies (
            repository_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            publication_mode TEXT NOT NULL
                CHECK (publication_mode IN ('scheduled_creation', 'immediate_availability')),
            target_ref TEXT NOT NULL CHECK (target_ref LIKE 'refs/heads/%'),
            development_target_ref TEXT,
            created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
            PRIMARY KEY (repository_id, revision),
            FOREIGN KEY (repository_id) REFERENCES repositories(id) ON DELETE CASCADE,
            CHECK (
                (publication_mode = 'scheduled_creation' AND development_target_ref IS NULL)
                OR
                (publication_mode = 'immediate_availability'
                    AND development_target_ref IS NOT NULL
                    AND development_target_ref <> target_ref)
            )
        );

        CREATE TABLE features (
            id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            repository_id TEXT NOT NULL,
            goal TEXT NOT NULL CHECK (length(trim(goal)) > 0),
            sealed INTEGER NOT NULL CHECK (sealed IN (0, 1)),
            created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
            PRIMARY KEY (id, revision),
            FOREIGN KEY (repository_id) REFERENCES repositories(id) ON DELETE CASCADE
        );

        CREATE TABLE tasks (
            id TEXT PRIMARY KEY,
            feature_id TEXT NOT NULL,
            feature_revision INTEGER NOT NULL CHECK (feature_revision > 0),
            name TEXT NOT NULL CHECK (length(trim(name)) > 0),
            status TEXT NOT NULL CHECK (status IN (
                'planned', 'building', 'captured', 'validated', 'queued', 'scheduled',
                'reconciling', 'verifying', 'commit_prepared', 'push_pending',
                'remote_confirmed', 'published', 'blocked', 'cancelled', 'superseded'
            )),
            reason_code TEXT,
            reason_message TEXT,
            blocked_from TEXT,
            FOREIGN KEY (feature_id, feature_revision)
                REFERENCES features(id, revision) ON DELETE CASCADE,
            CHECK (
                (status IN ('blocked', 'cancelled', 'superseded')
                    AND reason_code IS NOT NULL AND reason_message IS NOT NULL)
                OR
                (status NOT IN ('blocked', 'cancelled', 'superseded')
                    AND reason_code IS NULL AND reason_message IS NULL)
            ),
            CHECK (
                (status = 'blocked' AND blocked_from IS NOT NULL)
                OR (status <> 'blocked' AND blocked_from IS NULL)
            )
        );

        CREATE TABLE task_dependencies (
            task_id TEXT NOT NULL,
            dependency_id TEXT NOT NULL,
            required_milestone TEXT NOT NULL CHECK (required_milestone IN (
                'captured', 'development_available', 'target_published'
            )),
            PRIMARY KEY (task_id, dependency_id),
            FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
            FOREIGN KEY (dependency_id) REFERENCES tasks(id) ON DELETE RESTRICT,
            CHECK (task_id <> dependency_id)
        );

        CREATE TABLE packages (
            id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            repository_id TEXT NOT NULL,
            policy_revision INTEGER NOT NULL CHECK (policy_revision > 0),
            required_milestone TEXT NOT NULL CHECK (required_milestone IN (
                'captured', 'development_available', 'target_published'
            )),
            base_tree TEXT,
            result_tree TEXT,
            content_hash TEXT,
            captured_at_unix_ms INTEGER CHECK (captured_at_unix_ms >= 0),
            PRIMARY KEY (id, revision),
            FOREIGN KEY (repository_id, policy_revision)
                REFERENCES repository_policies(repository_id, revision)
        );

        CREATE TABLE package_tasks (
            package_id TEXT NOT NULL,
            package_revision INTEGER NOT NULL CHECK (package_revision > 0),
            task_id TEXT NOT NULL,
            PRIMARY KEY (package_id, package_revision, task_id),
            FOREIGN KEY (package_id, package_revision)
                REFERENCES packages(id, revision) ON DELETE CASCADE,
            FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE RESTRICT
        );

        CREATE INDEX idx_features_repository ON features(repository_id);
        CREATE INDEX idx_tasks_feature ON tasks(feature_id, feature_revision);
        CREATE INDEX idx_packages_repository ON packages(repository_id);
        "#,
    },
    Migration {
        version: 2,
        sql: r#"
        CREATE TABLE events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            occurred_at_unix_ms INTEGER NOT NULL CHECK (occurred_at_unix_ms >= 0),
            request_id TEXT,
            attempt_id TEXT,
            repository_id TEXT,
            entity_type TEXT,
            entity_id TEXT,
            entity_revision INTEGER CHECK (entity_revision > 0),
            kind TEXT NOT NULL CHECK (length(trim(kind)) > 0),
            severity TEXT NOT NULL CHECK (severity IN ('debug', 'info', 'warning', 'error')),
            reason_code TEXT,
            message TEXT NOT NULL CHECK (length(trim(message)) > 0),
            details_json TEXT NOT NULL CHECK (json_valid(details_json)),
            FOREIGN KEY (repository_id) REFERENCES repositories(id) ON DELETE SET NULL
        );

        CREATE INDEX idx_events_request ON events(request_id, sequence DESC);
        CREATE INDEX idx_events_repository ON events(repository_id, sequence DESC);
        CREATE INDEX idx_events_kind ON events(kind, sequence DESC);
        "#,
    },
    Migration {
        version: 3,
        sql: r#"
        CREATE TABLE feature_plans (
            feature_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            repository_id TEXT NOT NULL,
            target_ref TEXT NOT NULL CHECK (target_ref LIKE 'refs/heads/%'),
            sealed INTEGER NOT NULL CHECK (sealed IN (0, 1)),
            document_json TEXT NOT NULL CHECK (json_valid(document_json)),
            created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
            PRIMARY KEY (feature_id, revision),
            FOREIGN KEY (repository_id) REFERENCES repositories(id) ON DELETE CASCADE
        );

        CREATE INDEX idx_feature_plans_repository
            ON feature_plans(repository_id, feature_id, revision DESC);
        "#,
    },
    Migration {
        version: 4,
        sql: r#"
        CREATE TABLE build_workspaces (
            feature_id TEXT NOT NULL,
            plan_revision INTEGER NOT NULL CHECK (plan_revision > 0),
            workspace_path TEXT NOT NULL UNIQUE CHECK (length(workspace_path) > 0),
            base_commit TEXT NOT NULL CHECK (length(base_commit) = 40),
            prerequisites_json TEXT NOT NULL CHECK (json_valid(prerequisites_json)),
            created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
            PRIMARY KEY (feature_id, plan_revision),
            FOREIGN KEY (feature_id, plan_revision)
                REFERENCES feature_plans(feature_id, revision) ON DELETE RESTRICT
        );
        "#,
    },
    Migration {
        version: 5,
        sql: r#"
        CREATE TABLE snapshot_packages (
            package_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            feature_id TEXT NOT NULL,
            plan_revision INTEGER NOT NULL CHECK (plan_revision > 0),
            package_path TEXT NOT NULL UNIQUE CHECK (length(package_path) > 0),
            base_tree TEXT NOT NULL CHECK (length(base_tree) = 40),
            result_tree TEXT NOT NULL CHECK (length(result_tree) = 40),
            content_hash TEXT NOT NULL CHECK (length(content_hash) = 64),
            manifest_json TEXT NOT NULL CHECK (json_valid(manifest_json)),
            created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
            PRIMARY KEY (package_id, revision),
            FOREIGN KEY (feature_id, plan_revision)
                REFERENCES build_workspaces(feature_id, plan_revision) ON DELETE RESTRICT
        );

        CREATE INDEX idx_snapshot_packages_feature
            ON snapshot_packages(feature_id, plan_revision, created_at_unix_ms);
        "#,
    },
    Migration {
        version: 6,
        sql: r#"
        CREATE TABLE snapshot_recovery_issues (
            package_path TEXT PRIMARY KEY CHECK (length(package_path) > 0),
            kind TEXT NOT NULL CHECK (length(trim(kind)) > 0),
            message TEXT NOT NULL CHECK (length(trim(message)) > 0),
            first_seen_at_unix_ms INTEGER NOT NULL CHECK (first_seen_at_unix_ms >= 0),
            last_seen_at_unix_ms INTEGER NOT NULL CHECK (last_seen_at_unix_ms >= 0)
        );

        CREATE INDEX idx_snapshot_recovery_issues_last_seen
            ON snapshot_recovery_issues(last_seen_at_unix_ms DESC);
        "#,
    },
    Migration {
        version: 7,
        sql: r#"
        CREATE TABLE trusted_checks (
            repository_id TEXT NOT NULL,
            check_id TEXT NOT NULL,
            command_json TEXT NOT NULL CHECK (json_valid(command_json)),
            timeout_seconds INTEGER NOT NULL CHECK (timeout_seconds BETWEEN 1 AND 3600),
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
            PRIMARY KEY (repository_id, check_id),
            FOREIGN KEY (repository_id) REFERENCES repositories(id) ON DELETE CASCADE
        );
        CREATE TABLE validation_evidence (
            package_id TEXT NOT NULL,
            package_revision INTEGER NOT NULL CHECK (package_revision > 0),
            check_id TEXT NOT NULL,
            command_json TEXT NOT NULL CHECK (json_valid(command_json)),
            exit_code INTEGER,
            timed_out INTEGER NOT NULL CHECK (timed_out IN (0, 1)),
            output_summary TEXT NOT NULL,
            executed_at_unix_ms INTEGER NOT NULL CHECK (executed_at_unix_ms >= 0),
            PRIMARY KEY (package_id, package_revision, check_id),
            FOREIGN KEY (package_id, package_revision)
                REFERENCES snapshot_packages(package_id, revision) ON DELETE CASCADE
        );
    "#,
    },
];

pub(crate) fn apply(connection: &mut Connection) -> Result<(), StoreError> {
    let current = reject_future_schema(connection)?;
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
    {
        apply_one(connection, migration.version, migration.sql)?;
    }
    Ok(())
}

pub(crate) fn reject_future_schema(connection: &Connection) -> Result<u32, StoreError> {
    let found: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > STORAGE_SCHEMA_VERSION {
        return Err(StoreError::FutureSchema {
            found,
            supported: STORAGE_SCHEMA_VERSION,
        });
    }
    Ok(found)
}

fn apply_one(connection: &mut Connection, version: u32, sql: &str) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(sql)?;
    transaction.pragma_update(None, "user_version", version)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_schema_is_rejected() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "user_version", STORAGE_SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            apply(&mut connection),
            Err(StoreError::FutureSchema { .. })
        ));
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        let error = apply_one(
            &mut connection,
            1,
            "CREATE TABLE should_rollback (id INTEGER); THIS IS INVALID SQL;",
        );
        assert!(error.is_err());

        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
        let table_count: u32 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'should_rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }

    #[test]
    fn version_one_databases_upgrade_without_losing_existing_tables() {
        let mut connection = Connection::open_in_memory().unwrap();
        apply_one(&mut connection, MIGRATIONS[0].version, MIGRATIONS[0].sql).unwrap();
        assert_eq!(reject_future_schema(&connection).unwrap(), 1);

        apply(&mut connection).unwrap();

        assert_eq!(
            reject_future_schema(&connection).unwrap(),
            STORAGE_SCHEMA_VERSION
        );
        let tables: u32 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('repositories', 'events')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }
}
