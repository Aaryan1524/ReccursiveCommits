//! Domain model and state rules.

/// Version of the in-process domain contract.
pub const DOMAIN_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_contract_starts_at_version_one() {
        assert_eq!(DOMAIN_VERSION, 1);
    }
}
