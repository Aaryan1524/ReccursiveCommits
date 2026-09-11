use reccursive_core::{AttemptId, PackageId, RepositoryId, Revision};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// A daemon-owned command definition. Arguments are stored separately and never shell-expanded.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrustedCheck {
    pub repository_id: RepositoryId,
    pub id: String,
    pub command: Vec<String>,
    pub timeout_seconds: u32,
    pub enabled: bool,
}
/// Immutable check evidence attached to one exact snapshot revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationEvidence {
    pub package_id: PackageId,
    pub revision: Revision,
    pub check_id: String,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_summary: String,
    pub executed_at_unix_ms: i64,
}

/// Immutable result of running a trusted check in a reconciled release candidate.
///
/// The input fields make the result useful only for the exact package, base commit, command, and
/// inherited environment that were observed. A changed input invalidates older candidate evidence
/// for the same check instead of overwriting it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateValidationEvidence {
    pub attempt_id: AttemptId,
    pub package_id: PackageId,
    pub revision: Revision,
    pub check_id: String,
    pub base_commit: String,
    pub package_content_hash: String,
    pub command: Vec<String>,
    pub environment_fingerprint: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_summary: String,
    pub executed_at_unix_ms: i64,
    pub invalidated_at_unix_ms: Option<i64>,
    pub invalidated_reason: Option<String>,
}

impl Store {
    pub fn add_trusted_check(&mut self, check: &TrustedCheck) -> Result<(), StoreError> {
        if check.id.trim().is_empty()
            || check.command.is_empty()
            || !(1..=3600).contains(&check.timeout_seconds)
        {
            return Err(StoreError::InvalidData(
                "trusted check configuration is invalid".into(),
            ));
        }
        self.connection.execute("INSERT INTO trusted_checks (repository_id, check_id, command_json, timeout_seconds, enabled) VALUES (?1, ?2, ?3, ?4, ?5)", params![check.repository_id.to_string(), check.id, serde_json::to_string(&check.command)?, check.timeout_seconds, check.enabled])?;
        Ok(())
    }
    pub fn trusted_checks(
        &self,
        repository_id: RepositoryId,
    ) -> Result<Vec<TrustedCheck>, StoreError> {
        let mut statement=self.connection.prepare("SELECT check_id, command_json, timeout_seconds, enabled FROM trusted_checks WHERE repository_id=?1 ORDER BY check_id")?;
        statement
            .query_map([repository_id.to_string()], |r| {
                Ok(TrustedCheck {
                    repository_id,
                    id: r.get(0)?,
                    command: serde_json::from_str(&r.get::<_, String>(1)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    timeout_seconds: r.get(2)?,
                    enabled: r.get(3)?,
                })
            })?
            .map(|r| r.map_err(StoreError::from))
            .collect()
    }
    pub fn record_validation_evidence(
        &mut self,
        evidence: &ValidationEvidence,
    ) -> Result<(), StoreError> {
        if evidence.check_id.trim().is_empty()
            || evidence.command.is_empty()
            || evidence.output_summary.len() > 16_384
            || evidence.executed_at_unix_ms < 0
        {
            return Err(StoreError::InvalidData(
                "validation evidence is invalid".into(),
            ));
        }
        self.connection.execute("INSERT INTO validation_evidence (package_id, package_revision, check_id, command_json, exit_code, timed_out, output_summary, executed_at_unix_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![evidence.package_id.to_string(),evidence.revision.get(),evidence.check_id,serde_json::to_string(&evidence.command)?,evidence.exit_code,evidence.timed_out,evidence.output_summary,evidence.executed_at_unix_ms])?;
        Ok(())
    }

    /// Records a check result for one reconciled release candidate.
    ///
    /// A result is never edited. If its inputs no longer match a new candidate, the old record is
    /// marked stale before the new one is inserted, leaving operators an auditable history.
    pub fn record_candidate_validation_evidence(
        &mut self,
        evidence: &CandidateValidationEvidence,
    ) -> Result<(), StoreError> {
        if evidence.check_id.trim().is_empty()
            || evidence.command.is_empty()
            || !is_hex(&evidence.base_commit, 40)
            || !is_hex(&evidence.package_content_hash, 64)
            || !is_hex(&evidence.environment_fingerprint, 64)
            || evidence.output_summary.len() > 16_384
            || evidence.executed_at_unix_ms < 0
            || evidence
                .invalidated_at_unix_ms
                .is_some_and(|value| value < 0)
            || evidence
                .invalidated_reason
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
        {
            return Err(StoreError::InvalidData(
                "candidate validation evidence is invalid".into(),
            ));
        }
        let command = serde_json::to_string(&evidence.command)?;
        self.connection.execute(
            "UPDATE candidate_validation_evidence
             SET invalidated_at_unix_ms = ?5,
                 invalidated_reason = 'candidate validation inputs changed'
             WHERE package_id = ?1 AND package_revision = ?2 AND check_id = ?3
               AND invalidated_at_unix_ms IS NULL
               AND (base_commit <> ?4 OR package_content_hash <> ?6 OR command_json <> ?7
                    OR environment_fingerprint <> ?8)",
            params![
                evidence.package_id.to_string(),
                evidence.revision.get(),
                evidence.check_id,
                evidence.base_commit,
                evidence.executed_at_unix_ms,
                evidence.package_content_hash,
                command,
                evidence.environment_fingerprint,
            ],
        )?;
        self.connection.execute(
            "INSERT INTO candidate_validation_evidence (
                attempt_id, package_id, package_revision, check_id, base_commit,
                package_content_hash, command_json, environment_fingerprint, exit_code,
                timed_out, output_summary, executed_at_unix_ms, invalidated_at_unix_ms,
                invalidated_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                evidence.attempt_id.to_string(),
                evidence.package_id.to_string(),
                evidence.revision.get(),
                evidence.check_id,
                evidence.base_commit,
                evidence.package_content_hash,
                command,
                evidence.environment_fingerprint,
                evidence.exit_code,
                evidence.timed_out,
                evidence.output_summary,
                evidence.executed_at_unix_ms,
                evidence.invalidated_at_unix_ms,
                evidence.invalidated_reason,
            ],
        )?;
        Ok(())
    }

    pub fn candidate_validation_evidence(
        &self,
        attempt_id: AttemptId,
    ) -> Result<Vec<CandidateValidationEvidence>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT package_id, package_revision, check_id, base_commit, package_content_hash,
                    command_json, environment_fingerprint, exit_code, timed_out, output_summary,
                    executed_at_unix_ms, invalidated_at_unix_ms, invalidated_reason
             FROM candidate_validation_evidence WHERE attempt_id = ?1 ORDER BY check_id",
        )?;
        statement
            .query_map([attempt_id.to_string()], |row| {
                Ok(CandidateValidationEvidence {
                    attempt_id,
                    package_id: row.get::<_, String>(0)?.parse().map_err(invalid_data)?,
                    revision: Revision::new(row.get(1)?).map_err(invalid_data)?,
                    check_id: row.get(2)?,
                    base_commit: row.get(3)?,
                    package_content_hash: row.get(4)?,
                    command: serde_json::from_str(&row.get::<_, String>(5)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    environment_fingerprint: row.get(6)?,
                    exit_code: row.get(7)?,
                    timed_out: row.get(8)?,
                    output_summary: row.get(9)?,
                    executed_at_unix_ms: row.get(10)?,
                    invalidated_at_unix_ms: row.get(11)?,
                    invalidated_reason: row.get(12)?,
                })
            })?
            .map(|row| row.map_err(StoreError::from))
            .collect()
    }
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid_data(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(StoreError::InvalidData(error.to_string())),
    )
}
