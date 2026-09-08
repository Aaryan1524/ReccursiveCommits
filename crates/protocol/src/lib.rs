//! Versioned request and response contract shared by local clients and the daemon.

/// Initial local API protocol version.
pub const API_VERSION: u16 = 1;

/// Domain version represented by this protocol crate.
#[must_use]
pub const fn domain_version() -> u16 {
    reccursive_core::DOMAIN_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_links_the_current_domain_contract() {
        assert_eq!(domain_version(), reccursive_core::DOMAIN_VERSION);
    }
}
