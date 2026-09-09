use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A nonzero immutable revision number.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(NonZeroU32);

impl Revision {
    /// First revision of a versioned entity.
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    /// Constructs a revision, rejecting zero.
    pub fn new(value: u32) -> Result<Self, RevisionError> {
        NonZeroU32::new(value).map(Self).ok_or(RevisionError::Zero)
    }

    /// Returns the numeric revision.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Returns the following revision or an overflow error.
    pub fn next(self) -> Result<Self, RevisionError> {
        self.get()
            .checked_add(1)
            .and_then(NonZeroU32::new)
            .map(Self)
            .ok_or(RevisionError::Overflow)
    }
}

impl Default for Revision {
    fn default() -> Self {
        Self::FIRST
    }
}

/// Invalid revision operation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RevisionError {
    /// Revisions start at one.
    #[error("revision must be greater than zero")]
    Zero,
    /// No later `u32` revision exists.
    #[error("revision cannot advance beyond {0}", u32::MAX)]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_start_at_one_and_advance() {
        assert_eq!(Revision::FIRST.get(), 1);
        assert_eq!(Revision::FIRST.next().unwrap().get(), 2);
    }

    #[test]
    fn zero_and_overflow_are_rejected() {
        assert_eq!(Revision::new(0), Err(RevisionError::Zero));
        let maximum = Revision::new(u32::MAX).unwrap();
        assert_eq!(maximum.next(), Err(RevisionError::Overflow));
    }
}
