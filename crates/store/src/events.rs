use std::str::FromStr;

use reccursive_core::{AttemptId, EventId, RepositoryId, RequestId, Revision};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Store, StoreError, redact_json, redact_text};

/// Default maximum number of events retained by one installation.
pub const DEFAULT_EVENT_RETENTION: usize = 10_000;

/// Importance used by event filtering and notification policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    Debug,
    Info,
    Warning,
    Error,
}

impl EventSeverity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            _ => Err(StoreError::InvalidData(format!(
                "unknown event severity {value:?}"
            ))),
        }
    }
}

/// Correlation and ownership fields attached to an event.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventContext {
    pub request_id: Option<RequestId>,
    pub attempt_id: Option<AttemptId>,
    pub repository_id: Option<RepositoryId>,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    pub entity_revision: Option<Revision>,
}

/// Sanitized event ready for durable insertion.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    id: EventId,
    occurred_at_unix_ms: i64,
    context: EventContext,
    kind: String,
    severity: EventSeverity,
    reason_code: Option<String>,
    message: String,
    details: Value,
}

impl NewEvent {
    pub fn new(
        occurred_at_unix_ms: i64,
        context: EventContext,
        kind: impl Into<String>,
        severity: EventSeverity,
        reason_code: Option<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Result<Self, StoreError> {
        let kind = kind.into();
        let message = message.into();
        if occurred_at_unix_ms < 0 {
            return Err(StoreError::InvalidData(
                "event timestamp must not be negative".into(),
            ));
        }
        if kind.trim().is_empty()
            || !kind.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || "._".contains(character)
            })
        {
            return Err(StoreError::InvalidData(
                "event kind must contain lowercase letters, digits, dots, or underscores".into(),
            ));
        }
        if message.trim().is_empty() {
            return Err(StoreError::InvalidData(
                "event message must not be empty".into(),
            ));
        }
        Ok(Self {
            id: EventId::new(),
            occurred_at_unix_ms,
            context,
            kind,
            severity,
            reason_code: reason_code.map(|value| redact_text(&value)),
            message: redact_text(&message),
            details: redact_json(&details),
        })
    }
}

/// Event returned to diagnostics in newest-first order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub sequence: i64,
    pub id: EventId,
    pub occurred_at_unix_ms: i64,
    pub context: EventContext,
    pub kind: String,
    pub severity: EventSeverity,
    pub reason_code: Option<String>,
    pub message: String,
    pub details: Value,
}

impl Store {
    /// Records one sanitized event and prunes older events in the same transaction.
    pub fn record_event(&mut self, event: &NewEvent, retention: usize) -> Result<(), StoreError> {
        if retention == 0 {
            return Err(StoreError::InvalidData(
                "event retention must be greater than zero".into(),
            ));
        }
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO events (
                id, occurred_at_unix_ms, request_id, attempt_id, repository_id,
                entity_type, entity_id, entity_revision, kind, severity,
                reason_code, message, details_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                event.id.to_string(),
                event.occurred_at_unix_ms,
                event.context.request_id.map(|value| value.to_string()),
                event.context.attempt_id.map(|value| value.to_string()),
                event.context.repository_id.map(|value| value.to_string()),
                event.context.entity_type.as_deref(),
                event.context.entity_id.as_deref(),
                event.context.entity_revision.map(Revision::get),
                event.kind.as_str(),
                event.severity.as_str(),
                event.reason_code.as_deref(),
                event.message.as_str(),
                serde_json::to_string(&event.details)?,
            ],
        )?;
        transaction.execute(
            "DELETE FROM events
             WHERE sequence NOT IN (
                 SELECT sequence FROM events ORDER BY sequence DESC LIMIT ?1
             )",
            [i64::try_from(retention).unwrap_or(i64::MAX)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads at most 1,000 newest events for diagnostics.
    pub fn events(&self, limit: usize) -> Result<Vec<StoredEvent>, StoreError> {
        let limit = limit.clamp(1, 1_000);
        let mut statement = self.connection.prepare(
            "SELECT sequence, id, occurred_at_unix_ms, request_id, attempt_id,
                    repository_id, entity_type, entity_id, entity_revision,
                    kind, severity, reason_code, message, details_json
             FROM events ORDER BY sequence DESC LIMIT ?1",
        )?;
        statement
            .query_map([i64::try_from(limit).unwrap_or(1_000)], RawEvent::from_row)?
            .map(|event| event.map_err(StoreError::from).and_then(TryInto::try_into))
            .collect()
    }
}

struct RawEvent {
    sequence: i64,
    id: String,
    occurred_at_unix_ms: i64,
    request_id: Option<String>,
    attempt_id: Option<String>,
    repository_id: Option<String>,
    entity_type: Option<String>,
    entity_id: Option<String>,
    entity_revision: Option<u32>,
    kind: String,
    severity: String,
    reason_code: Option<String>,
    message: String,
    details_json: String,
}

impl RawEvent {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            sequence: row.get(0)?,
            id: row.get(1)?,
            occurred_at_unix_ms: row.get(2)?,
            request_id: row.get(3)?,
            attempt_id: row.get(4)?,
            repository_id: row.get(5)?,
            entity_type: row.get(6)?,
            entity_id: row.get(7)?,
            entity_revision: row.get(8)?,
            kind: row.get(9)?,
            severity: row.get(10)?,
            reason_code: row.get(11)?,
            message: row.get(12)?,
            details_json: row.get(13)?,
        })
    }
}

impl TryFrom<RawEvent> for StoredEvent {
    type Error = StoreError;

    fn try_from(raw: RawEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            sequence: raw.sequence,
            id: EventId::from_str(&raw.id).map_err(invalid_stored)?,
            occurred_at_unix_ms: raw.occurred_at_unix_ms,
            context: EventContext {
                request_id: parse_optional_id(raw.request_id)?,
                attempt_id: parse_optional_id(raw.attempt_id)?,
                repository_id: parse_optional_id(raw.repository_id)?,
                entity_type: raw.entity_type,
                entity_id: raw.entity_id,
                entity_revision: raw
                    .entity_revision
                    .map(Revision::new)
                    .transpose()
                    .map_err(invalid_stored)?,
            },
            kind: raw.kind,
            severity: EventSeverity::parse(&raw.severity)?,
            reason_code: raw.reason_code,
            message: raw.message,
            details: serde_json::from_str(&raw.details_json)?,
        })
    }
}

fn parse_optional_id<T>(value: Option<String>) -> Result<Option<T>, StoreError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .map(|value| value.parse().map_err(invalid_stored))
        .transpose()
}

fn invalid_stored(error: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidData(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(index: i64) -> NewEvent {
        NewEvent::new(
            index,
            EventContext {
                request_id: Some(RequestId::new()),
                ..EventContext::default()
            },
            "worker.failed",
            EventSeverity::Error,
            Some("validation_failed".into()),
            format!("failure {index}"),
            json!({ "index": index }),
        )
        .unwrap()
    }

    #[test]
    fn retention_keeps_only_the_newest_events() {
        let mut store = Store::open_in_memory().unwrap();
        for index in 1..=5 {
            store.record_event(&event(index), 3).unwrap();
        }
        let events = store.events(10).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message, "failure 5");
        assert_eq!(events[2].message, "failure 3");
    }

    #[test]
    fn event_storage_never_persists_supplied_secrets() {
        let mut store = Store::open_in_memory().unwrap();
        let event = NewEvent::new(
            1,
            EventContext::default(),
            "remote.failed",
            EventSeverity::Error,
            Some("token=reason-secret".into()),
            "failed https://user:pass@example.test/repo with ghp_1234567890abcdef",
            json!({
                "authorization": "Bearer secret-value",
                "nested": { "url": "https://user:pass@example.test/repo" }
            }),
        )
        .unwrap();
        store.record_event(&event, DEFAULT_EVENT_RETENTION).unwrap();
        let encoded = serde_json::to_string(&store.events(1).unwrap()).unwrap();
        for secret in [
            "reason-secret",
            "user:pass",
            "ghp_1234567890abcdef",
            "secret-value",
        ] {
            assert!(!encoded.contains(secret), "leaked {secret}");
        }
        assert!(encoded.contains("<redacted>"));
    }
}
