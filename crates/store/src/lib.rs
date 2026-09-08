//! Durable local persistence boundary.

/// First storage schema version reserved by the application.
pub const STORAGE_SCHEMA_VERSION: u16 = 1;

/// Reports the domain contract this storage crate was compiled against.
#[must_use]
pub const fn domain_version() -> u16 {
    reccursive_core::DOMAIN_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_links_the_current_domain_contract() {
        assert_eq!(domain_version(), reccursive_core::DOMAIN_VERSION);
    }
}
