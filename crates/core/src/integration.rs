//! External integrations and how their failures are treated.
//!
//! The daemon talks to more than one outside system, and they fail independently. A Git remote
//! being unreachable says nothing about whether a notification endpoint is, and neither says
//! anything about whether local work can proceed. Keeping them apart is what stops one outage from
//! looking like a general failure and stalling everything.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// An external system the daemon depends on, tracked separately from the others.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integration {
    /// Pushing to and fetching from a Git remote.
    Git,
    /// An AI provider used for managed execution.
    AiProvider,
    /// Notification delivery.
    Telegram,
}

impl Integration {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::AiProvider => "ai_provider",
            Self::Telegram => "telegram",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "git" => Some(Self::Git),
            "ai_provider" => Some(Self::AiProvider),
            "telegram" => Some(Self::Telegram),
            _ => None,
        }
    }
}

/// Why an endpoint could not be reached, distinguished so one outage does not look like another.
///
/// The distinction is what decides the response. A transport failure is worth retrying on its own
/// schedule; a rejected credential is not, and retrying it can lock an account or trigger a
/// prompt no one is present to answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivityFault {
    /// The endpoint could not be reached at all. Retrying later is reasonable.
    Unreachable,
    /// The endpoint answered and refused the credentials. Retrying will not fix it.
    Rejected,
    /// The endpoint answered and refused the request itself, such as a protected branch.
    Refused,
    /// The operation ran too long without an answer.
    TimedOut,
}

impl ConnectivityFault {
    /// Reports whether waiting and trying again could plausibly succeed.
    #[must_use]
    pub const fn is_worth_retrying(self) -> bool {
        matches!(self, Self::Unreachable | Self::TimedOut)
    }

    /// Classifies a Git transport failure from the message Git produced.
    ///
    /// Git reports all of these through the same non-zero exit, so the message is the only signal
    /// available. Anything unrecognized is treated as unreachable rather than as a rejection: an
    /// unknown fault that is retried costs a delay, while an unknown fault treated as a permanent
    /// rejection strands work that would have succeeded.
    #[must_use]
    pub fn classify_git(message: &str) -> Self {
        let lowered = message.to_ascii_lowercase();
        const REJECTED: [&str; 6] = [
            "authentication failed",
            "permission denied",
            "invalid username or password",
            "access denied",
            "could not read username",
            "host key verification failed",
        ];
        const REFUSED: [&str; 4] = [
            "protected branch",
            "pre-receive hook declined",
            "refusing to allow",
            "non-fast-forward",
        ];
        const TIMED_OUT: [&str; 3] = ["timed out", "timeout", "operation timed out"];

        if REJECTED.iter().any(|needle| lowered.contains(needle)) {
            return Self::Rejected;
        }
        if REFUSED.iter().any(|needle| lowered.contains(needle)) {
            return Self::Refused;
        }
        if TIMED_OUT.iter().any(|needle| lowered.contains(needle)) {
            return Self::TimedOut;
        }
        Self::Unreachable
    }
}

/// How long to wait before contacting an endpoint again after consecutive failures.
///
/// Doubling with a ceiling, so a long outage costs a bounded number of attempts rather than a
/// steady stream of them. The ceiling matters more than the growth rate: without it a daemon left
/// running through a multi-day outage would keep retrying at whatever its last interval was, and
/// with too small an interval that is indistinguishable from polling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackoffPolicy {
    pub initial: Duration,
    pub ceiling: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(30),
            ceiling: Duration::from_secs(30 * 60),
        }
    }
}

impl BackoffPolicy {
    /// The delay after `consecutive_failures` failures in a row.
    ///
    /// A fault that will not resolve by waiting gets no delay at all, because it gets no retry:
    /// repeatedly presenting a rejected credential is how an account gets locked, or how a
    /// credential prompt appears with nobody present to answer it.
    #[must_use]
    pub fn delay_after(
        &self,
        consecutive_failures: u32,
        fault: ConnectivityFault,
    ) -> Option<Duration> {
        if !fault.is_worth_retrying() || consecutive_failures == 0 {
            return None;
        }
        let doublings = consecutive_failures.saturating_sub(1).min(20);
        let scaled = self
            .initial
            .checked_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .unwrap_or(self.ceiling);
        Some(scaled.min(self.ceiling))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_credential_is_never_scheduled_for_another_attempt() {
        let policy = BackoffPolicy::default();
        for fault in [ConnectivityFault::Rejected, ConnectivityFault::Refused] {
            assert_eq!(
                policy.delay_after(1, fault),
                None,
                "retrying {fault:?} cannot succeed and can lock an account or raise a prompt"
            );
            assert_eq!(policy.delay_after(9, fault), None);
        }
    }

    #[test]
    fn a_transport_outage_backs_off_and_stops_growing_at_the_ceiling() {
        let policy = BackoffPolicy::default();
        let fault = ConnectivityFault::Unreachable;

        assert_eq!(policy.delay_after(1, fault), Some(Duration::from_secs(30)));
        assert_eq!(policy.delay_after(2, fault), Some(Duration::from_secs(60)));
        assert_eq!(policy.delay_after(3, fault), Some(Duration::from_secs(120)));

        // A long outage must not become a steady stream of attempts.
        assert_eq!(policy.delay_after(100, fault), Some(policy.ceiling));
        assert_eq!(policy.delay_after(u32::MAX, fault), Some(policy.ceiling));
    }

    #[test]
    fn integrations_round_trip_through_their_stored_names() {
        for integration in [
            Integration::Git,
            Integration::AiProvider,
            Integration::Telegram,
        ] {
            assert_eq!(Integration::parse(integration.as_str()), Some(integration));
        }
        assert_eq!(Integration::parse("something_else"), None);
    }
}
