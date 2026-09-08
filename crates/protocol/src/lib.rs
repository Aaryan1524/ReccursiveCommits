//! Versioned request and response contract shared by local clients and the daemon.

pub mod transport;

use std::fmt;

pub use reccursive_core::RequestId;
use serde::{Deserialize, Serialize};
pub use transport::{LocalClient, TransportError};

/// Initial local API protocol version.
pub const API_VERSION: u16 = 1;

/// Maximum encoded request or response size accepted by the local transport.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Domain version represented by this protocol crate.
#[must_use]
pub const fn domain_version() -> u16 {
    reccursive_core::DOMAIN_VERSION
}

/// Installation-scoped credential. Debug output is always redacted.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthToken(String);

impl AuthToken {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolValidationError> {
        let value = value.into();
        if value.len() < 32 || value.len() > 256 || value.chars().any(char::is_whitespace) {
            return Err(ProtocolValidationError::InvalidAuthToken);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for AuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthToken(<redacted>)")
    }
}

/// One versioned command sent to the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub api_version: u16,
    pub request_id: RequestId,
    pub auth_token: AuthToken,
    pub command: Command,
}

impl RequestEnvelope {
    #[must_use]
    pub fn new(auth_token: AuthToken, command: Command) -> Self {
        Self {
            api_version: API_VERSION,
            request_id: RequestId::new(),
            auth_token,
            command,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.api_version != API_VERSION {
            return Err(ProtocolValidationError::UnsupportedVersion {
                received: self.api_version,
                supported: API_VERSION,
            });
        }
        Ok(())
    }
}

/// Commands supported by the Phase 1 service boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Command {
    Ping,
}

/// Correlated daemon response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub api_version: u16,
    pub request_id: RequestId,
    pub result: Result<ResponseData, ApiError>,
}

impl ResponseEnvelope {
    #[must_use]
    pub const fn success(request_id: RequestId, data: ResponseData) -> Self {
        Self {
            api_version: API_VERSION,
            request_id,
            result: Ok(data),
        }
    }

    #[must_use]
    pub const fn failure(request_id: RequestId, error: ApiError) -> Self {
        Self {
            api_version: API_VERSION,
            request_id,
            result: Err(error),
        }
    }
}

/// Successful API payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ResponseData {
    Pong {
        service_version: String,
        schema_version: u32,
    },
}

/// Stable machine-readable API failure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    #[must_use]
    pub fn new(code: ApiErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

/// Error codes remain stable when human wording improves.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    Unauthorized,
    Conflict,
    TemporarilyUnavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProtocolValidationError {
    #[error("authentication token must be 32-256 non-whitespace characters")]
    InvalidAuthToken,
    #[error("API version {received} is unsupported; this service supports version {supported}")]
    UnsupportedVersion { received: u16, supported: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_links_the_current_domain_contract() {
        assert_eq!(domain_version(), reccursive_core::DOMAIN_VERSION);
    }

    #[test]
    fn authentication_token_is_redacted_from_debug_output() {
        let token = AuthToken::new("a".repeat(32)).unwrap();
        assert_eq!(format!("{token:?}"), "AuthToken(<redacted>)");
    }

    #[test]
    fn request_validation_reports_version_negotiation() {
        let mut request =
            RequestEnvelope::new(AuthToken::new("a".repeat(32)).unwrap(), Command::Ping);
        request.api_version += 1;
        assert_eq!(
            request.validate(),
            Err(ProtocolValidationError::UnsupportedVersion {
                received: API_VERSION + 1,
                supported: API_VERSION,
            })
        );
    }
}
