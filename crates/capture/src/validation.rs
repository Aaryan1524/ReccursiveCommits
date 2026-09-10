use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Rules applied to the exact Git tree that would become an immutable package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentValidationPolicy {
    pub max_file_bytes: u64,
    pub excluded_path_prefixes: Vec<String>,
    pub generated_path_prefixes: Vec<String>,
    pub generated_suffixes: Vec<String>,
    pub secret_scan_max_bytes: u64,
}

impl Default for ContentValidationPolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
            excluded_path_prefixes: vec![".env".into(), ".env/".into(), ".git/".into()],
            generated_path_prefixes: vec![
                "node_modules/".into(),
                "target/".into(),
                "dist/".into(),
                "build/".into(),
                "coverage/".into(),
            ],
            generated_suffixes: vec![".map".into(), ".min.js".into()],
            secret_scan_max_bytes: 1024 * 1024,
        }
    }
}

/// Stable reason a file cannot be captured.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentRule {
    ExcludedPath,
    GeneratedArtifact,
    FileTooLarge,
    SuspectedSecret,
}

impl ContentRule {
    pub const fn description(self) -> &'static str {
        match self {
            Self::ExcludedPath => "path is excluded by capture policy",
            Self::GeneratedArtifact => "generated artifact is excluded by capture policy",
            Self::FileTooLarge => "file exceeds the capture size limit",
            Self::SuspectedSecret => "file appears to contain a secret",
        }
    }
}

/// Safe diagnostic: it records a rule and repository-relative path, never matched content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContentViolation {
    pub rule: ContentRule,
    pub path: String,
}

impl ContentValidationPolicy {
    pub(crate) fn validate_path(
        &self,
        path: &str,
        size: u64,
        bytes: Option<&[u8]>,
    ) -> Vec<ContentViolation> {
        let mut violations = Vec::new();
        let normalized = Path::new(path).to_string_lossy().replace('\\', "/");
        if self.excluded_path_prefixes.iter().any(|prefix| {
            normalized == prefix.trim_end_matches('/') || normalized.starts_with(prefix)
        }) {
            violations.push(ContentViolation {
                rule: ContentRule::ExcludedPath,
                path: normalized.clone(),
            });
        }
        if self
            .generated_path_prefixes
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
            || self
                .generated_suffixes
                .iter()
                .any(|suffix| normalized.ends_with(suffix))
        {
            violations.push(ContentViolation {
                rule: ContentRule::GeneratedArtifact,
                path: normalized.clone(),
            });
        }
        if size > self.max_file_bytes {
            violations.push(ContentViolation {
                rule: ContentRule::FileTooLarge,
                path: normalized.clone(),
            });
        }
        if let Some(bytes) = bytes.filter(|_| size <= self.secret_scan_max_bytes)
            && contains_secret(bytes)
        {
            violations.push(ContentViolation {
                rule: ContentRule::SuspectedSecret,
                path: normalized,
            });
        }
        violations
    }
}

fn contains_secret(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let patterns = [
        r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----",
        r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
        r"\bgithub_pat_[A-Za-z0-9_]{20,}\b",
        r"\bAKIA[0-9A-Z]{16}\b",
        r"\bsk-[A-Za-z0-9_-]{20,}\b",
    ];
    patterns
        .iter()
        .any(|pattern| Regex::new(pattern).is_ok_and(|regex| regex.is_match(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_block_generated_large_and_secret_content_without_echoing_it() {
        let policy = ContentValidationPolicy::default();
        assert_eq!(
            policy.validate_path("target/debug/app", 1, Some(b"safe"))[0].rule,
            ContentRule::GeneratedArtifact
        );
        assert_eq!(
            policy.validate_path("large.bin", policy.max_file_bytes + 1, None)[0].rule,
            ContentRule::FileTooLarge
        );
        let violations = policy.validate_path(
            "settings.rs",
            30,
            Some(b"let key = \"sk-super-secret-token-abcdef\";"),
        );
        assert_eq!(violations[0].rule, ContentRule::SuspectedSecret);
        assert!(!format!("{violations:?}").contains("super-secret"));
    }
}
