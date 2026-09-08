use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

/// Error returned when a typed identifier has the wrong prefix or UUID payload.
#[derive(Debug, Clone, Eq, Error, PartialEq)]
#[error("invalid {kind} ID; expected {prefix}_<uuid>")]
pub struct IdParseError {
    kind: &'static str,
    prefix: &'static str,
}

impl IdParseError {
    const fn new(kind: &'static str, prefix: &'static str) -> Self {
        Self { kind, prefix }
    }
}

macro_rules! typed_id {
    ($name:ident, $kind:literal, $prefix:literal) => {
        #[doc = concat!("Stable identifier for a ", $kind, ".")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a random identifier with a type-specific display prefix.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wraps an existing UUID.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!($prefix, "_{}"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let Some(uuid) = value.strip_prefix(concat!($prefix, "_")) else {
                    return Err(IdParseError::new($kind, $prefix));
                };
                Uuid::parse_str(uuid)
                    .map(Self)
                    .map_err(|_| IdParseError::new($kind, $prefix))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }
    };
}

typed_id!(RepositoryId, "repository", "repo");
typed_id!(FeatureId, "feature", "feature");
typed_id!(TaskId, "task", "task");
typed_id!(ReleaseUnitId, "release unit", "unit");
typed_id!(PackageId, "package", "package");
typed_id!(RequestId, "request", "request");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_round_trip_with_their_type_prefix() {
        let id = RepositoryId::new();
        assert_eq!(id.to_string().parse::<RepositoryId>(), Ok(id));
        assert!(id.to_string().starts_with("repo_"));
    }

    #[test]
    fn identifiers_reject_another_entity_prefix() {
        let task = TaskId::new();
        let error = task.to_string().parse::<RepositoryId>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid repository ID; expected repo_<uuid>"
        );
    }

    #[test]
    fn generated_identifiers_are_distinct() {
        assert_ne!(TaskId::new(), TaskId::new());
    }
}
