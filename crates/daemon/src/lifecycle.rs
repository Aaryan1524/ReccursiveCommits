//! Start, wake, and connectivity transitions.
//!
//! A scheduled release is a durable deadline, not a timer. A timer does not fire while the machine
//! is asleep, and a process that trusted one would wake believing nothing was due. Everything here
//! is built on that: the daemon reconciles what is *actually* overdue whenever it starts and
//! whenever it notices time has passed, and never depends on having been running to notice.

use std::time::{Duration, Instant};

/// Detects that wall-clock time has advanced far more than the process has been running.
///
/// Two clocks are read together: a monotonic one that stops while the machine is asleep, and the
/// wall clock that does not. When the wall clock has moved much further than the monotonic clock,
/// the difference is time the process was not running through — a sleep, a suspend, or a clock
/// correction. All three mean the same thing here: deadlines may have passed unobserved, so
/// whatever is overdue has to be reconciled.
#[derive(Debug)]
pub struct WakeDetector {
    last_monotonic: Instant,
    last_wall_clock_ms: i64,
    tolerance: Duration,
}

/// What a clock reading revealed about time that passed while the process was not observing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeTransition {
    /// Time advanced about as much as the process ran. Nothing was missed.
    Continuous,
    /// The wall clock advanced well beyond the running time: the machine slept, was suspended, or
    /// the clock was corrected forward. Durable deadlines must be reconciled.
    Resumed { unobserved: Duration },
    /// The wall clock moved backwards. Nothing is newly overdue, but anything computed from the
    /// previous reading is suspect, so deadlines are re-examined rather than trusted.
    ClockMovedBackwards { by: Duration },
}

impl WakeDetector {
    /// Starts detection from the current moment.
    ///
    /// `tolerance` is how much drift between the two clocks is treated as ordinary. Scheduling
    /// resolution is minutes, so this does not need to be tight, and a value that is too tight
    /// would report a wake every time the process was merely descheduled.
    #[must_use]
    pub fn started_at(monotonic: Instant, wall_clock_ms: i64, tolerance: Duration) -> Self {
        Self {
            last_monotonic: monotonic,
            last_wall_clock_ms: wall_clock_ms,
            tolerance,
        }
    }

    /// Records a new reading of both clocks and reports what happened between them.
    pub fn observe(&mut self, monotonic: Instant, wall_clock_ms: i64) -> TimeTransition {
        let ran_for = monotonic.saturating_duration_since(self.last_monotonic);
        let wall_delta_ms = wall_clock_ms - self.last_wall_clock_ms;
        self.last_monotonic = monotonic;
        self.last_wall_clock_ms = wall_clock_ms;

        if wall_delta_ms < 0 {
            return TimeTransition::ClockMovedBackwards {
                by: Duration::from_millis(wall_delta_ms.unsigned_abs()),
            };
        }
        let wall = Duration::from_millis(wall_delta_ms.unsigned_abs());
        let unobserved = wall.saturating_sub(ran_for);
        if unobserved > self.tolerance {
            TimeTransition::Resumed { unobserved }
        } else {
            TimeTransition::Continuous
        }
    }
}

/// Why an endpoint could not be reached, distinguished so one outage does not look like another.
///
/// The distinction is what decides the response. A transport failure is worth retrying on its own
/// schedule; a rejected credential is not, and retrying it can lock an account or trigger a
/// prompt no one is present to answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    #[test]
    fn ordinary_running_time_is_not_mistaken_for_a_wake() {
        let start = Instant::now();
        let mut detector = WakeDetector::started_at(start, 1_000_000, Duration::from_secs(30));

        // Both clocks advanced together: the process was simply running.
        let transition = detector.observe(start + Duration::from_secs(60), 1_000_000 + 60_000);

        assert_eq!(transition, TimeTransition::Continuous);
    }

    #[test]
    fn a_sleep_is_detected_from_the_gap_between_the_two_clocks() {
        let start = Instant::now();
        let mut detector = WakeDetector::started_at(start, 1_000_000, Duration::from_secs(30));

        // The machine slept for three hours: the monotonic clock barely moved, the wall clock did.
        // This is exactly the case a timer would have missed entirely.
        let transition = detector.observe(
            start + Duration::from_secs(2),
            1_000_000 + i64::try_from(180 * MINUTE).unwrap(),
        );

        match transition {
            TimeTransition::Resumed { unobserved } => {
                assert!(unobserved >= Duration::from_secs(3 * 60 * 60 - 5));
            }
            other => panic!("a three-hour sleep must be reported as a resume, got {other:?}"),
        }
    }

    #[test]
    fn a_backwards_clock_is_reported_rather_than_read_as_a_wake() {
        let start = Instant::now();
        let mut detector = WakeDetector::started_at(start, 1_000_000, Duration::from_secs(30));

        let transition = detector.observe(start + Duration::from_secs(10), 1_000_000 - 5_000);

        assert_eq!(
            transition,
            TimeTransition::ClockMovedBackwards {
                by: Duration::from_millis(5_000)
            }
        );
    }

    #[test]
    fn each_observation_measures_from_the_previous_one() {
        let start = Instant::now();
        let mut detector = WakeDetector::started_at(start, 1_000_000, Duration::from_secs(30));

        // A sleep, then ordinary running. The second reading must not re-report the first gap.
        detector.observe(start + Duration::from_secs(1), 1_000_000 + 3_600_000);
        let second = detector.observe(start + Duration::from_secs(61), 1_000_000 + 3_660_000);

        assert_eq!(second, TimeTransition::Continuous);
    }

    #[test]
    fn a_rejected_credential_is_never_confused_with_an_unreachable_host() {
        for message in [
            "fatal: Authentication failed for 'https://example.invalid/repo.git'",
            "git@github.com: Permission denied (publickey).",
            "remote: Invalid username or password",
            "fatal: could not read Username for 'https://example.invalid'",
            "Host key verification failed.",
        ] {
            let fault = ConnectivityFault::classify_git(message);
            assert_eq!(fault, ConnectivityFault::Rejected, "{message}");
            assert!(
                !fault.is_worth_retrying(),
                "retrying a rejected credential can lock an account or prompt with nobody present"
            );
        }
    }

    #[test]
    fn a_refused_push_is_distinguished_from_a_credential_problem() {
        for message in [
            "remote: error: GH006: Protected branch update failed",
            "remote: error: pre-receive hook declined",
            "! [rejected] main -> main (non-fast-forward)",
        ] {
            let fault = ConnectivityFault::classify_git(message);
            assert_eq!(fault, ConnectivityFault::Refused, "{message}");
            assert!(!fault.is_worth_retrying());
        }
    }

    #[test]
    fn a_transport_problem_is_retryable_and_an_unknown_one_is_treated_as_transport() {
        assert_eq!(
            ConnectivityFault::classify_git(
                "ssh: connect to host example.invalid port 22: \
                 Connection refused"
            ),
            ConnectivityFault::Unreachable
        );
        assert_eq!(
            ConnectivityFault::classify_git("fatal: unable to access ...: Operation timed out"),
            ConnectivityFault::TimedOut
        );
        // An unfamiliar failure is retried rather than treated as permanent: a needless delay is
        // recoverable, stranding work that would have succeeded is not.
        let unknown = ConnectivityFault::classify_git("fatal: something entirely new happened");
        assert_eq!(unknown, ConnectivityFault::Unreachable);
        assert!(unknown.is_worth_retrying());
    }
}
