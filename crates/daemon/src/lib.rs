//! Background service boundary.

mod service;

pub use service::{LocalService, ServiceError, ServicePaths};

/// Stable service identifier used by launchers and diagnostics.
pub const SERVICE_NAME: &str = "reccursive-daemon";

/// API version served by this build.
#[must_use]
pub const fn api_version() -> u16 {
    reccursive_protocol::API_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_uses_the_shared_protocol_version() {
        assert_eq!(api_version(), reccursive_protocol::API_VERSION);
    }
}

pub mod release;
