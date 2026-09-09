use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

const REDACTED: &str = "<redacted>";

/// Masks common credentials in unstructured diagnostic text.
#[must_use]
pub fn redact_text(value: &str) -> String {
    let value = credential_url_regex()
        .replace_all(value, "${scheme}<redacted>@")
        .into_owned();
    let value = bearer_regex()
        .replace_all(&value, "${prefix}<redacted>")
        .into_owned();
    let value = assignment_regex()
        .replace_all(&value, "${name}=<redacted>")
        .into_owned();
    token_regex().replace_all(&value, REDACTED).into_owned()
}

/// Recursively masks sensitive keys and credential patterns in JSON details.
#[must_use]
pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        Value::String(REDACTED.into())
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_json).collect()),
        Value::String(value) => Value::String(redact_text(value)),
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "authorization"
            | "cookie"
            | "password"
            | "passwd"
            | "secret"
            | "token"
            | "access_token"
            | "refresh_token"
            | "api_key"
            | "private_key"
            | "client_secret"
    ) || normalized.ends_with("_password")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_token")
        || normalized.ends_with("_key")
}

fn credential_url_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(?P<scheme>[a-z][a-z0-9+.-]*://)[^/@\s]+@")
            .expect("credential URL regular expression must compile")
    })
}

fn bearer_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(?P<prefix>\bbearer\s+)[a-z0-9._~+/=-]+")
            .expect("bearer-token regular expression must compile")
    })
}

fn assignment_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?P<name>token|password|passwd|secret|api[_-]?key|client[_-]?secret)=[^\s&]+",
        )
        .expect("credential-assignment regular expression must compile")
    })
}

fn token_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"\b(?:gh[pousr]_[A-Za-z0-9_]{8,}|github_pat_[A-Za-z0-9_]{8,}|[0-9]{6,}:[A-Za-z0-9_-]{20,})\b",
        )
        .expect("known-token regular expression must compile")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unstructured_text_masks_urls_bearer_values_and_known_tokens() {
        let input = "fetch https://user:pass@example.test/repo token=plain-secret \
                     Authorization: Bearer abc.def-123 ghp_1234567890abcdef";
        let redacted = redact_text(input);
        assert!(!redacted.contains("user:pass"));
        assert!(!redacted.contains("plain-secret"));
        assert!(!redacted.contains("abc.def-123"));
        assert!(!redacted.contains("ghp_1234567890abcdef"));
        assert!(redacted.contains("https://<redacted>@example.test/repo"));
    }

    #[test]
    fn json_redaction_is_recursive_and_preserves_safe_values() {
        let input = json!({
            "token": "never-store-this",
            "nested": [{
                "remote": "https://user:pass@example.test/repo",
                "safe": "visible",
                "client_secret": { "unexpected": "shape" }
            }]
        });
        let redacted = redact_json(&input);
        let encoded = redacted.to_string();
        assert!(!encoded.contains("never-store-this"));
        assert!(!encoded.contains("user:pass"));
        assert!(!encoded.contains("unexpected"));
        assert!(encoded.contains("visible"));
    }
}
