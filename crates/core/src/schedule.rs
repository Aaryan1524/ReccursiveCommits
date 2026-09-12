//! Validated, time-zone-aware scheduling policy primitives.
//!
//! This module deliberately describes *civil* scheduling rules only. P4-T02 turns a validated
//! policy into durable UTC slots; keeping that later concern out of the policy model makes it
//! possible to validate configuration without reading a clock or drawing random values.

use std::collections::BTreeSet;

use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Upper bound that keeps a daily policy useful without permitting accidental release bursts.
const MAX_DAILY_RELEASES: u8 = 24;
const MAX_WINDOWS: usize = 32;

/// A named IANA time zone validated against the system/bundled time-zone database.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IanaTimeZone(String);

impl IanaTimeZone {
    pub fn new(value: impl Into<String>) -> Result<Self, SchedulePolicyError> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 128 || value.contains(char::is_whitespace) {
            return Err(SchedulePolicyError::InvalidTimeZone(value));
        }
        TimeZone::get(&value).map_err(|_| SchedulePolicyError::InvalidTimeZone(value.clone()))?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IanaTimeZone {
    type Error = SchedulePolicyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<IanaTimeZone> for String {
    fn from(value: IanaTimeZone) -> Self {
        value.0
    }
}

/// A weekday in the user's configured civil time zone.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

/// A minute-precise local clock time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "DailyTimeWire", into = "DailyTimeWire")]
pub struct DailyTime {
    pub hour: u8,
    pub minute: u8,
}

impl DailyTime {
    pub fn new(hour: u8, minute: u8) -> Result<Self, SchedulePolicyError> {
        if hour > 23 || minute > 59 {
            return Err(SchedulePolicyError::InvalidDailyTime { hour, minute });
        }
        Ok(Self { hour, minute })
    }

    #[must_use]
    pub const fn minutes_after_midnight(self) -> u16 {
        self.hour as u16 * 60 + self.minute as u16
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DailyTimeWire {
    hour: u8,
    minute: u8,
}

impl TryFrom<DailyTimeWire> for DailyTime {
    type Error = SchedulePolicyError;

    fn try_from(value: DailyTimeWire) -> Result<Self, Self::Error> {
        Self::new(value.hour, value.minute)
    }
}

impl From<DailyTime> for DailyTimeWire {
    fn from(value: DailyTime) -> Self {
        Self {
            hour: value.hour,
            minute: value.minute,
        }
    }
}

/// A permitted local-time interval with an inclusive start and exclusive end.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "DailyWindowWire", into = "DailyWindowWire")]
pub struct DailyWindow {
    pub start: DailyTime,
    pub end: DailyTime,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DailyWindowWire {
    start: DailyTime,
    end: DailyTime,
}

impl TryFrom<DailyWindowWire> for DailyWindow {
    type Error = SchedulePolicyError;

    fn try_from(value: DailyWindowWire) -> Result<Self, Self::Error> {
        Self::new(value.start, value.end)
    }
}

impl From<DailyWindow> for DailyWindowWire {
    fn from(value: DailyWindow) -> Self {
        Self {
            start: value.start,
            end: value.end,
        }
    }
}

impl DailyWindow {
    pub fn new(start: DailyTime, end: DailyTime) -> Result<Self, SchedulePolicyError> {
        if start >= end {
            return Err(SchedulePolicyError::InvalidWindow { start, end });
        }
        Ok(Self { start, end })
    }
}

/// Inclusive lower and upper bounds for releases selected on one eligible day.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "DailyReleaseRangeWire", into = "DailyReleaseRangeWire")]
pub struct DailyReleaseRange {
    pub minimum: u8,
    pub maximum: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DailyReleaseRangeWire {
    minimum: u8,
    maximum: u8,
}

impl TryFrom<DailyReleaseRangeWire> for DailyReleaseRange {
    type Error = SchedulePolicyError;

    fn try_from(value: DailyReleaseRangeWire) -> Result<Self, Self::Error> {
        Self::new(value.minimum, value.maximum)
    }
}

impl From<DailyReleaseRange> for DailyReleaseRangeWire {
    fn from(value: DailyReleaseRange) -> Self {
        Self {
            minimum: value.minimum,
            maximum: value.maximum,
        }
    }
}

impl DailyReleaseRange {
    pub fn new(minimum: u8, maximum: u8) -> Result<Self, SchedulePolicyError> {
        if maximum == 0 || minimum > maximum || maximum > MAX_DAILY_RELEASES {
            return Err(SchedulePolicyError::InvalidDailyReleaseRange { minimum, maximum });
        }
        Ok(Self { minimum, maximum })
    }
}

/// How an overdue slot is handled after an offline period or missed timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "max_releases")]
pub enum MissedWindowBehavior {
    /// Choose a later allowed slot; never backdate a commit or automatically burst work.
    RescheduleForward,
    /// Permit a deliberately bounded catch-up run. P4-T04 controls when this is invoked.
    CatchUp { max_releases: u8 },
}

/// Scheduling settings applied to one repository after resolving any repository override.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "SchedulePolicyWire", into = "SchedulePolicyWire")]
pub struct SchedulePolicy {
    pub timezone: IanaTimeZone,
    pub allowed_days: BTreeSet<Weekday>,
    pub windows: Vec<DailyWindow>,
    pub daily_releases: DailyReleaseRange,
    pub minimum_spacing_minutes: u16,
    pub missed_window_behavior: MissedWindowBehavior,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SchedulePolicyWire {
    timezone: IanaTimeZone,
    allowed_days: BTreeSet<Weekday>,
    windows: Vec<DailyWindow>,
    daily_releases: DailyReleaseRange,
    minimum_spacing_minutes: u16,
    missed_window_behavior: MissedWindowBehavior,
}

impl TryFrom<SchedulePolicyWire> for SchedulePolicy {
    type Error = SchedulePolicyError;

    fn try_from(value: SchedulePolicyWire) -> Result<Self, Self::Error> {
        Self::new(
            value.timezone,
            value.allowed_days,
            value.windows,
            value.daily_releases,
            value.minimum_spacing_minutes,
            value.missed_window_behavior,
        )
    }
}

impl From<SchedulePolicy> for SchedulePolicyWire {
    fn from(value: SchedulePolicy) -> Self {
        Self {
            timezone: value.timezone,
            allowed_days: value.allowed_days,
            windows: value.windows,
            daily_releases: value.daily_releases,
            minimum_spacing_minutes: value.minimum_spacing_minutes,
            missed_window_behavior: value.missed_window_behavior,
        }
    }
}

impl SchedulePolicy {
    pub fn new(
        timezone: IanaTimeZone,
        allowed_days: BTreeSet<Weekday>,
        windows: Vec<DailyWindow>,
        daily_releases: DailyReleaseRange,
        minimum_spacing_minutes: u16,
        missed_window_behavior: MissedWindowBehavior,
    ) -> Result<Self, SchedulePolicyError> {
        if allowed_days.is_empty() {
            return Err(SchedulePolicyError::NoAllowedDays);
        }
        if windows.is_empty() {
            return Err(SchedulePolicyError::NoWindows);
        }
        if windows.len() > MAX_WINDOWS {
            return Err(SchedulePolicyError::TooManyWindows);
        }
        if minimum_spacing_minutes == 0 || minimum_spacing_minutes > 24 * 60 {
            return Err(SchedulePolicyError::InvalidMinimumSpacing(
                minimum_spacing_minutes,
            ));
        }
        let mut windows = windows;
        windows.sort_unstable_by_key(|window| window.start);
        for pair in windows.windows(2) {
            if pair[0].end > pair[1].start {
                return Err(SchedulePolicyError::OverlappingWindows);
            }
        }
        if let MissedWindowBehavior::CatchUp { max_releases } = missed_window_behavior
            && (max_releases == 0 || max_releases > MAX_DAILY_RELEASES)
        {
            return Err(SchedulePolicyError::InvalidCatchUpLimit(max_releases));
        }
        let capacity = slot_capacity(&windows, minimum_spacing_minutes);
        if usize::from(daily_releases.maximum) > capacity {
            return Err(SchedulePolicyError::DailyLimitExceedsWindowCapacity {
                maximum: daily_releases.maximum,
                capacity,
            });
        }
        Ok(Self {
            timezone,
            allowed_days,
            windows,
            daily_releases,
            minimum_spacing_minutes,
            missed_window_behavior,
        })
    }

    /// Resolves a repository-specific override and validates the effective policy as one unit.
    pub fn with_override(
        &self,
        override_policy: &SchedulePolicyOverride,
    ) -> Result<Self, SchedulePolicyError> {
        Self::new(
            override_policy
                .timezone
                .clone()
                .unwrap_or_else(|| self.timezone.clone()),
            override_policy
                .allowed_days
                .clone()
                .unwrap_or_else(|| self.allowed_days.clone()),
            override_policy
                .windows
                .clone()
                .unwrap_or_else(|| self.windows.clone()),
            override_policy
                .daily_releases
                .unwrap_or(self.daily_releases),
            override_policy
                .minimum_spacing_minutes
                .unwrap_or(self.minimum_spacing_minutes),
            override_policy
                .missed_window_behavior
                .unwrap_or(self.missed_window_behavior),
        )
    }
}

/// Fields a repository may replace from its future global scheduling default.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulePolicyOverride {
    pub timezone: Option<IanaTimeZone>,
    pub allowed_days: Option<BTreeSet<Weekday>>,
    pub windows: Option<Vec<DailyWindow>>,
    pub daily_releases: Option<DailyReleaseRange>,
    pub minimum_spacing_minutes: Option<u16>,
    pub missed_window_behavior: Option<MissedWindowBehavior>,
}

fn slot_capacity(windows: &[DailyWindow], spacing: u16) -> usize {
    let spacing = u32::from(spacing);
    let mut next = None;
    let mut count = 0;
    for window in windows {
        let start = u32::from(window.start.minutes_after_midnight());
        let end = u32::from(window.end.minutes_after_midnight());
        let mut candidate = next.unwrap_or(start).max(start);
        while candidate < end {
            count += 1;
            candidate += spacing;
        }
        next = Some(candidate);
    }
    count
}

/// A contradiction in scheduling configuration discovered before any work is queued.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SchedulePolicyError {
    #[error("{0:?} is not a valid IANA time zone")]
    InvalidTimeZone(String),
    #[error("{hour:02}:{minute:02} is not a valid local clock time")]
    InvalidDailyTime { hour: u8, minute: u8 },
    #[error("daily window must end after it starts")]
    InvalidWindow { start: DailyTime, end: DailyTime },
    #[error("at least one permitted weekday is required")]
    NoAllowedDays,
    #[error("at least one permitted time window is required")]
    NoWindows,
    #[error("at most {MAX_WINDOWS} daily windows are permitted")]
    TooManyWindows,
    #[error("daily windows must not overlap")]
    OverlappingWindows,
    #[error("daily releases must be between 1 and {MAX_DAILY_RELEASES}, with minimum <= maximum")]
    InvalidDailyReleaseRange { minimum: u8, maximum: u8 },
    #[error("minimum release spacing must be between 1 and 1440 minutes, got {0}")]
    InvalidMinimumSpacing(u16),
    #[error("daily maximum {maximum} cannot fit in permitted windows; capacity is {capacity}")]
    DailyLimitExceedsWindowCapacity { maximum: u8, capacity: usize },
    #[error("catch-up limit must be between 1 and {MAX_DAILY_RELEASES}, got {0}")]
    InvalidCatchUpLimit(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(hour: u8, minute: u8) -> DailyTime {
        DailyTime::new(hour, minute).unwrap()
    }

    fn weekdays() -> BTreeSet<Weekday> {
        BTreeSet::from([Weekday::Monday, Weekday::Tuesday, Weekday::Wednesday])
    }

    fn policy() -> SchedulePolicy {
        SchedulePolicy::new(
            IanaTimeZone::new("America/New_York").unwrap(),
            weekdays(),
            vec![DailyWindow::new(time(9, 0), time(17, 0)).unwrap()],
            DailyReleaseRange::new(1, 3).unwrap(),
            90,
            MissedWindowBehavior::RescheduleForward,
        )
        .unwrap()
    }

    #[test]
    fn policy_accepts_an_iana_zone_and_resolves_repository_overrides() {
        let effective = policy()
            .with_override(&SchedulePolicyOverride {
                timezone: Some(IanaTimeZone::new("Europe/London").unwrap()),
                daily_releases: Some(DailyReleaseRange::new(2, 2).unwrap()),
                ..SchedulePolicyOverride::default()
            })
            .unwrap();
        assert_eq!(effective.timezone.as_str(), "Europe/London");
        assert_eq!(
            effective.daily_releases,
            DailyReleaseRange::new(2, 2).unwrap()
        );
        assert_eq!(effective.windows, policy().windows);
    }

    #[test]
    fn invalid_or_contradictory_policies_are_refused() {
        assert!(matches!(
            IanaTimeZone::new("Mars/Olympus_Mons"),
            Err(SchedulePolicyError::InvalidTimeZone(_))
        ));
        let overlap = SchedulePolicy::new(
            IanaTimeZone::new("UTC").unwrap(),
            weekdays(),
            vec![
                DailyWindow::new(time(9, 0), time(12, 0)).unwrap(),
                DailyWindow::new(time(11, 0), time(13, 0)).unwrap(),
            ],
            DailyReleaseRange::new(1, 2).unwrap(),
            30,
            MissedWindowBehavior::RescheduleForward,
        );
        assert_eq!(overlap, Err(SchedulePolicyError::OverlappingWindows));

        let impossible = SchedulePolicy::new(
            IanaTimeZone::new("UTC").unwrap(),
            weekdays(),
            vec![DailyWindow::new(time(9, 0), time(10, 0)).unwrap()],
            DailyReleaseRange::new(1, 3).unwrap(),
            30,
            MissedWindowBehavior::RescheduleForward,
        );
        assert_eq!(
            impossible,
            Err(SchedulePolicyError::DailyLimitExceedsWindowCapacity {
                maximum: 3,
                capacity: 2,
            })
        );
    }

    #[test]
    fn invalid_catch_up_bursts_are_refused() {
        let result = SchedulePolicy::new(
            IanaTimeZone::new("UTC").unwrap(),
            weekdays(),
            vec![DailyWindow::new(time(9, 0), time(17, 0)).unwrap()],
            DailyReleaseRange::new(1, 1).unwrap(),
            30,
            MissedWindowBehavior::CatchUp { max_releases: 0 },
        );
        assert_eq!(result, Err(SchedulePolicyError::InvalidCatchUpLimit(0)));
    }

    #[test]
    fn deserialization_cannot_bypass_policy_validation() {
        let invalid_time = serde_json::json!({ "hour": 24, "minute": 0 });
        assert!(serde_json::from_value::<DailyTime>(invalid_time).is_err());

        let invalid_policy = serde_json::json!({
            "timezone": "UTC",
            "allowed_days": ["monday"],
            "windows": [{
                "start": { "hour": 9, "minute": 0 },
                "end": { "hour": 10, "minute": 0 }
            }],
            "daily_releases": { "minimum": 1, "maximum": 3 },
            "minimum_spacing_minutes": 30,
            "missed_window_behavior": { "kind": "reschedule_forward" }
        });
        assert!(serde_json::from_value::<SchedulePolicy>(invalid_policy).is_err());
    }
}
