//! Durable per-integration backoff.
//!
//! The daemon depends on several outside systems that fail independently. Tracking their health
//! separately is what keeps one outage from looking like a general failure: an unreachable Git
//! remote must not delay a different repository, and must not delay notification delivery at all.
//!
//! Backoff is durable rather than in-memory because a restart during an outage should resume where
//! it left off. Losing it would turn every restart into an immediate retry, which is how a crash
//! loop becomes a request flood.

use reccursive_core::{BackoffPolicy, ConnectivityFault, Integration};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// The recorded health of one integration within one scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntegrationHealth {
    pub integration: Integration,
    /// What the failure applies to: a repository identifier, or `global` for endpoints that are
    /// not per-repository.
    pub scope: String,
    pub consecutive_failures: u32,
    pub fault: ConnectivityFault,
    pub detail: String,
    /// When another attempt may be made. `None` means waiting will not help.
    pub next_attempt_at_unix_ms: Option<i64>,
    pub updated_at_unix_ms: i64,
}

impl IntegrationHealth {
    /// Reports whether an attempt may be made at this moment.
    #[must_use]
    pub const fn is_ready_at(&self, now_unix_ms: i64) -> bool {
        match self.next_attempt_at_unix_ms {
            Some(at) => at <= now_unix_ms,
            // No scheduled attempt: this fault needs a person, not a timer.
            None => false,
        }
    }
}

/// The scope used for endpoints that are not tied to one repository.
pub const GLOBAL_SCOPE: &str = "global";

const fn fault_column(fault: ConnectivityFault) -> &'static str {
    match fault {
        ConnectivityFault::Unreachable => "unreachable",
        ConnectivityFault::Rejected => "rejected",
        ConnectivityFault::Refused => "refused",
        ConnectivityFault::TimedOut => "timed_out",
    }
}

fn fault_from_column(value: &str) -> Result<ConnectivityFault, StoreError> {
    match value {
        "unreachable" => Ok(ConnectivityFault::Unreachable),
        "rejected" => Ok(ConnectivityFault::Rejected),
        "refused" => Ok(ConnectivityFault::Refused),
        "timed_out" => Ok(ConnectivityFault::TimedOut),
        other => Err(StoreError::InvalidData(format!(
            "stored connectivity fault {other} is not defined"
        ))),
    }
}

impl Store {
    /// Records a failure against one integration and returns when it may be attempted again.
    ///
    /// Consecutive failures accumulate, so the delay grows with a sustained outage rather than
    /// staying at its initial value. A fault that waiting cannot fix schedules no further attempt
    /// at all — repeatedly presenting a rejected credential is how an account gets locked, or how a
    /// credential prompt appears with nobody present to answer it.
    pub fn record_integration_failure(
        &mut self,
        integration: Integration,
        scope: &str,
        fault: ConnectivityFault,
        detail: &str,
        policy: BackoffPolicy,
        now_unix_ms: i64,
    ) -> Result<IntegrationHealth, StoreError> {
        if scope.trim().is_empty() || detail.trim().is_empty() || detail.len() > 1_024 {
            return Err(StoreError::InvalidData(
                "integration failure needs a scope and a short non-empty detail".into(),
            ));
        }
        let previous = self
            .integration_health(integration, scope)?
            .map_or(0, |health| health.consecutive_failures);
        let consecutive_failures = previous.saturating_add(1);
        let next_attempt_at_unix_ms = policy
            .delay_after(consecutive_failures, fault)
            .and_then(|delay| i64::try_from(delay.as_millis()).ok())
            .map(|delay| now_unix_ms.saturating_add(delay));

        self.connection.execute(
            "INSERT INTO integration_backoff (
                integration, scope, consecutive_failures, fault, detail,
                next_attempt_at_unix_ms, updated_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(integration, scope) DO UPDATE SET
                consecutive_failures = excluded.consecutive_failures,
                fault = excluded.fault,
                detail = excluded.detail,
                next_attempt_at_unix_ms = excluded.next_attempt_at_unix_ms,
                updated_at_unix_ms = excluded.updated_at_unix_ms",
            params![
                integration.as_str(),
                scope,
                consecutive_failures,
                fault_column(fault),
                detail,
                next_attempt_at_unix_ms,
                now_unix_ms,
            ],
        )?;
        self.integration_health(integration, scope)?.ok_or_else(|| {
            StoreError::InvalidData("integration health could not be read back".into())
        })
    }

    /// Clears recorded failures after a success, so the next outage starts from the beginning.
    pub fn clear_integration_failure(
        &mut self,
        integration: Integration,
        scope: &str,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM integration_backoff WHERE integration = ?1 AND scope = ?2",
            params![integration.as_str(), scope],
        )?;
        Ok(())
    }

    /// Returns the recorded health of one integration within one scope, if it has failed.
    pub fn integration_health(
        &self,
        integration: Integration,
        scope: &str,
    ) -> Result<Option<IntegrationHealth>, StoreError> {
        let row: Option<(u32, String, String, Option<i64>, i64)> = self
            .connection
            .query_row(
                "SELECT consecutive_failures, fault, detail, next_attempt_at_unix_ms,
                        updated_at_unix_ms
                 FROM integration_backoff WHERE integration = ?1 AND scope = ?2",
                params![integration.as_str(), scope],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((consecutive_failures, fault, detail, next_attempt_at_unix_ms, updated)) = row
        else {
            return Ok(None);
        };
        Ok(Some(IntegrationHealth {
            integration,
            scope: scope.to_owned(),
            consecutive_failures,
            fault: fault_from_column(&fault)?,
            detail,
            next_attempt_at_unix_ms,
            updated_at_unix_ms: updated,
        }))
    }

    /// Reports whether an integration may be contacted for this scope right now.
    ///
    /// An integration with no recorded failure is always ready. This is the question the scheduler
    /// asks before handing out work, and it is asked per scope so a failing repository does not
    /// hold up a healthy one.
    pub fn integration_is_ready(
        &self,
        integration: Integration,
        scope: &str,
        now_unix_ms: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .integration_health(integration, scope)?
            .is_none_or(|health| health.is_ready_at(now_unix_ms)))
    }

    /// Lists every integration currently in a failed state, for diagnostics.
    pub fn failing_integrations(&self) -> Result<Vec<IntegrationHealth>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT integration, scope, consecutive_failures, fault, detail,
                    next_attempt_at_unix_ms, updated_at_unix_ms
             FROM integration_backoff ORDER BY integration, scope",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (integration, scope, failures, fault, detail, next, updated) = row?;
            Ok(IntegrationHealth {
                integration: Integration::parse(&integration).ok_or_else(|| {
                    StoreError::InvalidData(format!("stored integration {integration} is unknown"))
                })?,
                scope,
                consecutive_failures: failures,
                fault: fault_from_column(&fault)?,
                detail,
                next_attempt_at_unix_ms: next,
                updated_at_unix_ms: updated,
            })
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn an_outage_in_one_repository_does_not_hold_up_another() {
        let mut store = store();
        let policy = BackoffPolicy::default();

        store
            .record_integration_failure(
                Integration::Git,
                "repo_a",
                ConnectivityFault::Unreachable,
                "could not reach the remote",
                policy,
                1_000,
            )
            .unwrap();

        assert!(
            !store
                .integration_is_ready(Integration::Git, "repo_a", 1_000)
                .unwrap()
        );
        assert!(
            store
                .integration_is_ready(Integration::Git, "repo_b", 1_000)
                .unwrap(),
            "a healthy repository must not inherit another repository's outage"
        );
        assert!(
            store
                .integration_is_ready(Integration::Telegram, "repo_a", 1_000)
                .unwrap(),
            "a Git outage says nothing about a different integration"
        );
    }

    #[test]
    fn a_sustained_outage_backs_off_instead_of_polling() {
        let mut store = store();
        let policy = BackoffPolicy::default();
        let mut previous_gap = 0;

        for attempt in 1..=5 {
            let health = store
                .record_integration_failure(
                    Integration::Git,
                    "repo_a",
                    ConnectivityFault::Unreachable,
                    "still unreachable",
                    policy,
                    1_000,
                )
                .unwrap();
            assert_eq!(health.consecutive_failures, attempt);
            let gap = health.next_attempt_at_unix_ms.unwrap() - 1_000;
            assert!(
                gap >= previous_gap,
                "each failure must wait at least as long as the last, not retry sooner"
            );
            previous_gap = gap;
        }

        // Five failures in: 30s doubled four times.
        assert_eq!(previous_gap, 480_000);

        // A long outage stops growing at the ceiling rather than becoming a steady stream.
        for _ in 0..20 {
            store
                .record_integration_failure(
                    Integration::Git,
                    "repo_a",
                    ConnectivityFault::Unreachable,
                    "still unreachable",
                    policy,
                    1_000,
                )
                .unwrap();
        }
        let settled = store
            .integration_health(Integration::Git, "repo_a")
            .unwrap()
            .unwrap();
        assert_eq!(
            settled.next_attempt_at_unix_ms.unwrap() - 1_000,
            i64::try_from(policy.ceiling.as_millis()).unwrap()
        );
    }

    #[test]
    fn a_rejected_credential_schedules_no_retry_at_all() {
        let mut store = store();

        let health = store
            .record_integration_failure(
                Integration::Git,
                "repo_a",
                ConnectivityFault::Rejected,
                "authentication failed",
                BackoffPolicy::default(),
                1_000,
            )
            .unwrap();

        assert_eq!(health.next_attempt_at_unix_ms, None);
        // Never ready: this needs a person, and repeatedly presenting the credential is how an
        // account gets locked or a prompt appears with nobody there.
        assert!(!health.is_ready_at(i64::MAX));
        assert!(
            !store
                .integration_is_ready(Integration::Git, "repo_a", i64::MAX)
                .unwrap()
        );
    }

    #[test]
    fn success_clears_the_backoff_so_the_next_outage_starts_fresh() {
        let mut store = store();
        let policy = BackoffPolicy::default();
        for _ in 0..4 {
            store
                .record_integration_failure(
                    Integration::Git,
                    "repo_a",
                    ConnectivityFault::Unreachable,
                    "unreachable",
                    policy,
                    1_000,
                )
                .unwrap();
        }
        store
            .clear_integration_failure(Integration::Git, "repo_a")
            .unwrap();

        assert!(
            store
                .integration_is_ready(Integration::Git, "repo_a", 0)
                .unwrap()
        );
        let fresh = store
            .record_integration_failure(
                Integration::Git,
                "repo_a",
                ConnectivityFault::Unreachable,
                "unreachable again",
                policy,
                2_000,
            )
            .unwrap();
        assert_eq!(fresh.consecutive_failures, 1);
    }

    #[test]
    fn a_restart_resumes_the_backoff_rather_than_retrying_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("queue.sqlite");
        let policy = BackoffPolicy::default();
        {
            let mut store = Store::open(&database).unwrap();
            for _ in 0..3 {
                store
                    .record_integration_failure(
                        Integration::Git,
                        "repo_a",
                        ConnectivityFault::Unreachable,
                        "unreachable",
                        policy,
                        1_000,
                    )
                    .unwrap();
            }
        }

        // A crash and restart during an outage must not reset into an immediate retry.
        let restarted = Store::open(&database).unwrap();
        let health = restarted
            .integration_health(Integration::Git, "repo_a")
            .unwrap()
            .unwrap();
        assert_eq!(health.consecutive_failures, 3);
        assert!(!health.is_ready_at(1_000));
        assert_eq!(restarted.failing_integrations().unwrap().len(), 1);
    }
}
