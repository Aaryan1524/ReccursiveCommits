//! Where a GitHub token lives on this machine, and what it is allowed to touch.
//!
//! One file per repository, owner-only, beside the service's own local credentials. A token is a
//! real secret: it is never written to an event, a log, or the diagnostic export, and it is never
//! returned over the local API — only whether one is present.
//!
//! The product works completely without any of this. A repository that publishes by direct push
//! never has a token stored, and nothing here runs for it.

use std::{
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use crate::{GitHubError, Token};

/// Reasons a token could not be stored or read back.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("no GitHub token is stored for this repository")]
    Missing,
    #[error("the stored GitHub token is unusable: {0}")]
    Unusable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The directory tokens live in, created owner-only the first time one is stored.
#[must_use]
pub fn credentials_root(state_dir: &Path) -> PathBuf {
    state_dir.join("credentials")
}

fn token_path(state_dir: &Path, repository_id: &str) -> PathBuf {
    credentials_root(state_dir).join(format!("github-{repository_id}.token"))
}

/// Stores a token for one repository, replacing any previous one.
///
/// Written to a fresh file created owner-only and then renamed, so a reader never sees a
/// half-written token and the secret never exists at a world-readable moment.
pub fn store(state_dir: &Path, repository_id: &str, token: &str) -> Result<(), TokenError> {
    // Validated before anything is written, so an unusable token is refused rather than stored and
    // then discovered at the moment a publication needed it.
    Token::new(token).map_err(|error: GitHubError| TokenError::Unusable(error.to_string()))?;

    let root = credentials_root(state_dir);
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;

    let destination = token_path(state_dir, repository_id);
    let staging = destination.with_extension("token.partial");
    let _ = fs::remove_file(&staging);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)?;
        file.write_all(token.trim().as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&staging, &destination)?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Reads the token for one repository.
pub fn load(state_dir: &Path, repository_id: &str) -> Result<Token, TokenError> {
    let path = token_path(state_dir, repository_id);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(TokenError::Missing),
        Err(error) => return Err(TokenError::Io(error)),
    };
    Token::new(raw).map_err(|error| TokenError::Unusable(error.to_string()))
}

/// Reports whether a token is stored, without reading it.
///
/// This is what the local API answers with. The token itself never crosses that boundary.
#[must_use]
pub fn is_present(state_dir: &Path, repository_id: &str) -> bool {
    token_path(state_dir, repository_id).exists()
}

/// Removes a stored token. Absent is success: the caller asked for it to be gone.
pub fn forget(state_dir: &Path, repository_id: &str) -> Result<(), TokenError> {
    match fs::remove_file(token_path(state_dir, repository_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(TokenError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_token_is_readable_only_by_its_owner() {
        let directory = tempfile::tempdir().unwrap();
        let repository = "repo_test";
        assert!(!is_present(directory.path(), repository));

        store(directory.path(), repository, "ghp_example_token").unwrap();
        assert!(is_present(directory.path(), repository));

        let mode = fs::metadata(token_path(directory.path(), repository))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a token readable by others is a leaked token");
        let root_mode = fs::metadata(credentials_root(directory.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
    }

    #[test]
    fn storing_a_token_twice_replaces_it_rather_than_failing() {
        // Rotating a token is ordinary. Refusing would mean deleting first, and a window with no
        // token at all.
        let directory = tempfile::tempdir().unwrap();
        let repository = "repo_test";
        store(directory.path(), repository, "ghp_first").unwrap();
        store(directory.path(), repository, "ghp_second").unwrap();
        assert_eq!(
            fs::read_to_string(token_path(directory.path(), repository)).unwrap(),
            "ghp_second"
        );
    }

    #[test]
    fn an_unusable_token_is_refused_before_it_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let repository = "repo_test";
        assert!(matches!(
            store(directory.path(), repository, "  "),
            Err(TokenError::Unusable(_))
        ));
        // Nothing was written, so the failure cannot be mistaken for a stored-but-broken token.
        assert!(!is_present(directory.path(), repository));
    }

    #[test]
    fn a_missing_token_is_named_as_missing_rather_than_as_a_read_failure() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(directory.path(), "repo_test"),
            Err(TokenError::Missing)
        ));
    }

    #[test]
    fn forgetting_a_token_removes_it_and_is_safe_to_repeat() {
        let directory = tempfile::tempdir().unwrap();
        let repository = "repo_test";
        store(directory.path(), repository, "ghp_example").unwrap();
        forget(directory.path(), repository).unwrap();
        assert!(!is_present(directory.path(), repository));
        forget(directory.path(), repository).unwrap();
    }
}
