//! Stable identity for a Git remote.
//!
//! A remote URL is an *address*: it says how to reach a repository, and one repository has several
//! valid addresses. `git@github.com:example/project.git` and `https://github.com/example/project`
//! are the same place. Anything that must answer "is this the same repository?" — refusing a
//! second publisher, holding the release lease for a branch — has to compare identity, not the
//! address the user happened to type.
//!
//! The address is kept and used verbatim for fetching and pushing. This type exists only alongside
//! it, for comparison.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A normalized identity derived from a remote address.
///
/// Two addresses that reach the same repository produce the same identity. The original address is
/// never modified or replaced by this.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteIdentity(String);

impl RemoteIdentity {
    /// Derives the identity of a remote address.
    ///
    /// Deliberately *not* normalized away: path case, and any non-default port. Path case matters
    /// because not every host is case-insensitive, and wrongly merging two distinct repositories is
    /// far worse than failing to notice a duplicate. A different port is a different endpoint.
    #[must_use]
    pub fn of(address: &str) -> Self {
        let trimmed = address.trim();
        let (host, path) = split_host_and_path(trimmed);
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);

        match host {
            // A host-based remote: the host is case-insensitive, the path is not.
            Some(host) => Self(format!("{}/{}", host.to_ascii_lowercase(), path)),
            // A local path remote. Kept as-is apart from the `.git` suffix and trailing slashes,
            // because a local filesystem path is already unambiguous.
            None => Self(format!("/{path}")),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Splits an address into its host (if any) and repository path.
///
/// Handles the three shapes Git accepts: a URL with a scheme, scp-like `user@host:path`, and a
/// plain filesystem path.
fn split_host_and_path(address: &str) -> (Option<&str>, &str) {
    if let Some(rest) = address.split_once("://").map(|(_, rest)| rest) {
        // scheme://[user@]host[:port]/path
        let rest = rest.split_once('@').map_or(rest, |(_, after)| after);
        return match rest.split_once('/') {
            Some((authority, path)) => (Some(authority), path),
            None => (Some(rest), ""),
        };
    }

    // scp-like syntax has a colon that is not part of a scheme: user@host:path
    if let Some((authority, path)) = address.split_once(':')
        && !address.starts_with('/')
        && !path.starts_with('/')
    {
        let authority = authority
            .split_once('@')
            .map_or(authority, |(_, after)| after);
        return (Some(authority), path);
    }

    (None, address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_address_for_one_repository_shares_an_identity() {
        let expected = RemoteIdentity::of("git@github.com:example/project.git");
        for address in [
            "git@github.com:example/project.git",
            "git@github.com:example/project",
            "https://github.com/example/project.git",
            "https://github.com/example/project",
            "https://github.com/example/project/",
            "ssh://git@github.com/example/project.git",
            "git://github.com/example/project.git",
            "https://GitHub.com/example/project.git",
        ] {
            assert_eq!(
                RemoteIdentity::of(address),
                expected,
                "{address} should reach the same repository"
            );
        }
        assert_eq!(expected.as_str(), "github.com/example/project");
    }

    #[test]
    fn genuinely_different_repositories_keep_different_identities() {
        let base = RemoteIdentity::of("https://github.com/example/project.git");
        for address in [
            "https://github.com/example/other.git",
            "https://gitlab.com/example/project.git",
            "https://github.com/different/project.git",
            // Path case is preserved: not every host folds case, and wrongly merging two real
            // repositories is worse than missing a duplicate.
            "https://github.com/Example/Project.git",
            // A different port is a different endpoint.
            "ssh://git@github.com:2222/example/project.git",
        ] {
            assert_ne!(
                RemoteIdentity::of(address),
                base,
                "{address} is a different repository"
            );
        }
    }

    #[test]
    fn local_path_remotes_are_identified_by_their_path() {
        let expected = RemoteIdentity::of("/srv/git/project.git");
        assert_eq!(RemoteIdentity::of("/srv/git/project"), expected);
        assert_eq!(RemoteIdentity::of("/srv/git/project/"), expected);
        assert_ne!(RemoteIdentity::of("/srv/git/other"), expected);
    }

    #[test]
    fn credentials_and_user_names_do_not_change_which_repository_it_is() {
        let expected = RemoteIdentity::of("https://github.com/example/project.git");
        assert_eq!(
            RemoteIdentity::of("https://someone@github.com/example/project.git"),
            expected
        );
        assert_eq!(
            RemoteIdentity::of("ssh://git@github.com/example/project.git"),
            expected
        );
    }
}
