//! Durable records of the pull requests this service opened.
//!
//! One row per release unit. The row exists so that opening a pull request is claim-before-execute
//! like everything else in the queue: a process that opened one and died before recording it finds
//! the existing one rather than opening a second.
//!
//! Nothing here ever merges. `merged_at_unix_ms` records what GitHub was observed to report, and
//! the only way it becomes non-null is somebody merging the pull request themselves.

use reccursive_core::{PackageId, ReleaseUnitId, RepositoryId, Revision, TargetRef};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// A pull request this service opened for one release unit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PullRequestRecord {
    pub release_unit_id: ReleaseUnitId,
    pub repository_id: RepositoryId,
    pub package_id: PackageId,
    pub package_revision: Revision,
    pub number: u64,
    pub url: String,
    pub head: TargetRef,
    pub base: TargetRef,
    /// `open` or `closed`, as GitHub last reported it.
    pub state: String,
    /// When the pull request was observed to have been merged, by a person.
    pub merged_at_unix_ms: Option<i64>,
    pub observed_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
}

impl Store {
    /// Records a pull request against the unit it delivers.
    ///
    /// Refuses a second row for the same unit rather than replacing the first: two numbers for one
    /// unit means one of the two pull requests is unattended, and quietly forgetting it is worse
    /// than failing here.
    pub fn record_pull_request(&mut self, record: &PullRequestRecord) -> Result<(), StoreError> {
        if record.url.trim().is_empty() {
            return Err(StoreError::InvalidData(
                "a pull request must carry the address a person can open it at".into(),
            ));
        }
        if record.state != "open" && record.state != "closed" {
            return Err(StoreError::InvalidData(format!(
                "unknown pull request state: {}",
                record.state
            )));
        }
        self.connection
            .execute(
                "INSERT INTO github_pull_requests (
                    release_unit_id, repository_id, package_id, package_revision, number, url,
                    head_ref, base_ref, state, merged_at_unix_ms, observed_at_unix_ms,
                    created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    record.release_unit_id.to_string(),
                    record.repository_id.to_string(),
                    record.package_id.to_string(),
                    record.package_revision.get(),
                    record.number,
                    record.url,
                    record.head.as_str(),
                    record.base.as_str(),
                    record.state,
                    record.merged_at_unix_ms,
                    record.observed_at_unix_ms,
                    record.created_at_unix_ms,
                ],
            )
            .map_err(|error| {
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
                    StoreError::Conflict(format!(
                        "a pull request is already recorded for {}",
                        record.release_unit_id
                    ))
                } else {
                    StoreError::Sqlite(error)
                }
            })?;
        Ok(())
    }

    /// The pull request opened for one unit, if there is one.
    pub fn pull_request(
        &self,
        release_unit_id: ReleaseUnitId,
    ) -> Result<Option<PullRequestRecord>, StoreError> {
        self.connection
            .query_row(
                &format!(
                    "SELECT {PULL_REQUEST_COLUMNS} FROM github_pull_requests
                          WHERE release_unit_id = ?1"
                ),
                [release_unit_id.to_string()],
                pull_request_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Every pull request still open, oldest first.
    ///
    /// These are what a maintenance pass asks GitHub about. A closed one is never re-read: its
    /// outcome is already durable, and asking again would be a request that can only cost a rate
    /// limit.
    pub fn open_pull_requests(&self) -> Result<Vec<PullRequestRecord>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {PULL_REQUEST_COLUMNS} FROM github_pull_requests
             WHERE state = 'open' ORDER BY created_at_unix_ms, release_unit_id"
        ))?;
        statement
            .query_map([], pull_request_from_row)?
            .map(|row| row.map_err(StoreError::from))
            .collect()
    }

    /// Records what GitHub now reports for a pull request.
    ///
    /// `merged` only ever moves from false to true, and only from an observation. There is no path
    /// in this codebase that merges a pull request, so there is no path that writes this for any
    /// other reason.
    pub fn observe_pull_request(
        &mut self,
        release_unit_id: ReleaseUnitId,
        state: &str,
        merged: bool,
        now_unix_ms: i64,
    ) -> Result<(), StoreError> {
        if state != "open" && state != "closed" {
            return Err(StoreError::InvalidData(format!(
                "unknown pull request state: {state}"
            )));
        }
        // A merged pull request is closed. Recording it as open would leave the unit waiting on a
        // pull request that has already done its job.
        let stored_state = if merged { "closed" } else { state };
        let changed = self.connection.execute(
            "UPDATE github_pull_requests
             SET state = ?2,
                 merged_at_unix_ms = CASE
                     WHEN ?3 AND merged_at_unix_ms IS NULL THEN ?4
                     ELSE merged_at_unix_ms
                 END,
                 observed_at_unix_ms = ?4
             WHERE release_unit_id = ?1",
            params![
                release_unit_id.to_string(),
                stored_state,
                merged,
                now_unix_ms
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidData(format!(
                "no pull request is recorded for {release_unit_id}"
            )));
        }
        Ok(())
    }
}

const PULL_REQUEST_COLUMNS: &str = "release_unit_id, repository_id, package_id, package_revision, \
     number, url, head_ref, base_ref, state, merged_at_unix_ms, observed_at_unix_ms, \
     created_at_unix_ms";

fn pull_request_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PullRequestRecord> {
    let parse_id = |value: String| -> rusqlite::Result<ReleaseUnitId> {
        value
            .parse()
            .map_err(|error: reccursive_core::IdParseError| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })
    };
    let release_unit_id = parse_id(row.get(0)?)?;
    let repository_id: RepositoryId =
        row.get::<_, String>(1)?
            .parse()
            .map_err(|error: reccursive_core::IdParseError| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })?;
    let package_id: PackageId =
        row.get::<_, String>(2)?
            .parse()
            .map_err(|error: reccursive_core::IdParseError| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })?;
    let package_revision = Revision::new(row.get::<_, u32>(3)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::other(error.to_string())),
        )
    })?;
    let target = |index: usize, value: String| -> rusqlite::Result<TargetRef> {
        TargetRef::new(value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })
    };
    Ok(PullRequestRecord {
        release_unit_id,
        repository_id,
        package_id,
        package_revision,
        number: row.get(4)?,
        url: row.get(5)?,
        head: target(6, row.get(6)?)?,
        base: target(7, row.get(7)?)?,
        state: row.get(8)?,
        merged_at_unix_ms: row.get(9)?,
        observed_at_unix_ms: row.get(10)?,
        created_at_unix_ms: row.get(11)?,
    })
}
