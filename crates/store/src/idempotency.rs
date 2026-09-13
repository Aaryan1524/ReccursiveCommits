//! Durable record of client-declared request intents.
//!
//! An agent that issues a request and loses the connection before the answer arrives cannot tell a
//! request that never arrived from one that arrived, acted, and answered. Retrying is the only move
//! it has, and without a record here that retry captures a second package, enrolls a second
//! repository, or publishes twice.
//!
//! The record is a *claim* taken before the command runs, not a cache written after it succeeds.
//! The window a retry has to be dangerous in is precisely the window where the first attempt is
//! still running, so that is the window the claim has to cover.

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError};

/// What a claim attempt found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotencyClaim {
    /// This key is new. The caller owns it and must settle or release it.
    Claimed,
    /// The same request already finished. Its recorded outcome is replayed verbatim.
    Settled { outcome: String },
    /// The same request is running right now, elsewhere. Nothing is replayed, because there is no
    /// answer yet.
    InProgress,
    /// The key was used before for a genuinely different request.
    Mismatched { command: String },
}

impl Store {
    /// Takes the key for one command, or reports what the key already refers to.
    ///
    /// The insert and the read are one immediate transaction: two connections cannot both see an
    /// absent row and both proceed.
    pub fn claim_idempotency_key(
        &mut self,
        key: &str,
        command: &str,
        fingerprint: &str,
        now_unix_ms: i64,
    ) -> Result<IdempotencyClaim, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO idempotent_requests
                 (idempotency_key, command, fingerprint, status, outcome,
                  claimed_at_unix_ms, settled_at_unix_ms)
             VALUES (?1, ?2, ?3, 'in_progress', NULL, ?4, NULL)",
            params![key, command, fingerprint, now_unix_ms],
        )?;
        if inserted == 1 {
            transaction.commit()?;
            return Ok(IdempotencyClaim::Claimed);
        }

        let existing: (String, String, String, Option<String>) = transaction
            .query_row(
                "SELECT command, fingerprint, status, outcome
                 FROM idempotent_requests WHERE idempotency_key = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidData(format!(
                    "idempotency key {key} was neither inserted nor found"
                ))
            })?;
        transaction.commit()?;

        let (existing_command, existing_fingerprint, status, outcome) = existing;
        if existing_fingerprint != fingerprint {
            return Ok(IdempotencyClaim::Mismatched {
                command: existing_command,
            });
        }
        match (status.as_str(), outcome) {
            ("settled", Some(outcome)) => Ok(IdempotencyClaim::Settled { outcome }),
            ("in_progress", _) => Ok(IdempotencyClaim::InProgress),
            (other, _) => Err(StoreError::InvalidData(format!(
                "idempotency key {key} is in unrecognized state {other}"
            ))),
        }
    }

    /// Records the outcome a later retry of this key will be given.
    pub fn settle_idempotency_key(
        &mut self,
        key: &str,
        outcome: &str,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let updated = self.connection.execute(
            "UPDATE idempotent_requests
                SET status = 'settled', outcome = ?2, settled_at_unix_ms = ?3
              WHERE idempotency_key = ?1 AND status = 'in_progress'",
            params![key, outcome, now_unix_ms],
        )?;
        if updated == 0 {
            return Err(StoreError::Conflict(format!(
                "idempotency key {key} was not held when its outcome was recorded"
            )));
        }
        Ok(())
    }

    /// Gives the key back, so a later request may use it for a real retry.
    ///
    /// Only correct for a failure that provably left nothing behind. A failure that may have
    /// reached a remote is settled instead: releasing it would invite a second push.
    pub fn release_idempotency_key(&mut self, key: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM idempotent_requests WHERE idempotency_key = ?1 AND status = 'in_progress'",
            params![key],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn a_first_request_takes_the_key_and_a_retry_replays_its_outcome() {
        let mut store = store();
        assert_eq!(
            store
                .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 1_000)
                .unwrap(),
            IdempotencyClaim::Claimed
        );
        store
            .settle_idempotency_key("agent-1", r#"{"Ok":"package_1"}"#, 1_200)
            .unwrap();
        assert_eq!(
            store
                .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 9_000)
                .unwrap(),
            IdempotencyClaim::Settled {
                outcome: r#"{"Ok":"package_1"}"#.to_owned()
            }
        );
    }

    #[test]
    fn a_retry_arriving_while_the_first_attempt_runs_is_told_to_wait() {
        let mut store = store();
        store
            .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 1_000)
            .unwrap();
        assert_eq!(
            store
                .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 1_010)
                .unwrap(),
            IdempotencyClaim::InProgress
        );
    }

    #[test]
    fn reusing_a_key_for_a_different_request_is_reported_rather_than_answered() {
        let mut store = store();
        store
            .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 1_000)
            .unwrap();
        assert_eq!(
            store
                .claim_idempotency_key("agent-1", "enroll_repository", &"b".repeat(64), 1_010)
                .unwrap(),
            IdempotencyClaim::Mismatched {
                command: "capture_package".to_owned()
            }
        );
    }

    #[test]
    fn a_released_key_is_available_for_a_genuine_retry() {
        let mut store = store();
        store
            .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 1_000)
            .unwrap();
        store.release_idempotency_key("agent-1").unwrap();
        assert_eq!(
            store
                .claim_idempotency_key("agent-1", "capture_package", &"a".repeat(64), 1_020)
                .unwrap(),
            IdempotencyClaim::Claimed
        );
    }

    #[test]
    fn a_settled_key_cannot_be_released_out_from_under_a_replay() {
        let mut store = store();
        store
            .claim_idempotency_key("agent-1", "release_package", &"a".repeat(64), 1_000)
            .unwrap();
        store
            .settle_idempotency_key("agent-1", r#"{"Err":"push failed"}"#, 1_100)
            .unwrap();
        store.release_idempotency_key("agent-1").unwrap();
        assert_eq!(
            store
                .claim_idempotency_key("agent-1", "release_package", &"a".repeat(64), 1_200)
                .unwrap(),
            IdempotencyClaim::Settled {
                outcome: r#"{"Err":"push failed"}"#.to_owned()
            }
        );
    }
}
