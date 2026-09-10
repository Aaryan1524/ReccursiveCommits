use reccursive_core::{PackageId, RepositoryId, Revision};
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
}
