//! Validated, time-zone-aware scheduling policy primitives.
//!
//! This module deliberately describes *civil* scheduling rules only. P4-T02 turns a validated
//! policy into durable UTC slots; keeping that later concern out of the policy model makes it
//! possible to validate configuration without reading a clock or drawing random values.

use std::collections::BTreeSet;

use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Upper bound that keeps a daily policy useful without permitting accidental release bursts.
const MAX_DAILY_RELEASES: u8 = 24;
const MAX_WINDOWS: usize = 32;
const MAX_SCHEDULING_HORIZON_DAYS: usize = 366;

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

/// A future release instant selected from a policy's civil-time windows.
///
/// The local date is stored alongside the absolute UTC instant so an operator can explain why a
/// slot was chosen even after a daylight-saving transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedSlot {
    pub selected_at_unix_ms: i64,
    pub local_date: String,
    pub timezone: IanaTimeZone,
}

/// Deterministic pseudo-random slot selector.
///
/// The seed is provided by the daemon in production and fixed in tests. This is intentionally not
/// a security primitive: its purpose is a reproducible, varied distribution within a policy's
/// permitted windows, not secrecy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotGenerator {
    state: u64,
}

impl SlotGenerator {
    #[must_use]
    pub const fn seeded(seed: u64) -> Self {
        // XorShift's all-zero state is absorbing, so map it to a fixed non-zero state.
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// Selects `requested_slots` future instants without exceeding a policy's daily maximum or
    /// minimum spacing. Existing slots are retained and count against the day they already occupy.
    ///
    /// A daily minimum is applied where queued work exists; it never creates artificial empty work
    /// simply to fill a calendar. The returned values are sorted chronologically.
    pub fn select(
        &mut self,
        policy: &SchedulePolicy,
        not_before_unix_ms: i64,
        requested_slots: usize,
        existing_slots_unix_ms: &[i64],
    ) -> Result<Vec<PlannedSlot>, SchedulePolicyError> {
        if requested_slots == 0 {
            return Ok(Vec::new());
        }
        if not_before_unix_ms < 0 {
            return Err(SchedulePolicyError::InvalidSchedulingTimestamp(
                not_before_unix_ms,
            ));
        }
        let timezone = policy.timezone.as_str();
        let start = Timestamp::from_millisecond(not_before_unix_ms)
            .map_err(|_| SchedulePolicyError::InvalidSchedulingTimestamp(not_before_unix_ms))?
            .in_tz(timezone)
            .map_err(|_| SchedulePolicyError::InvalidTimeZone(timezone.to_owned()))?;
        let mut date = start.date();
        let spacing_ms = i64::from(policy.minimum_spacing_minutes) * 60_000;
        let mut selected = Vec::with_capacity(requested_slots);
        let mut occupied = existing_slots_unix_ms.to_vec();

        for _ in 0..MAX_SCHEDULING_HORIZON_DAYS {
            if selected.len() == requested_slots {
                break;
            }
            if policy
                .allowed_days
                .contains(&weekday_from_jiff(date.weekday()))
            {
                let existing_today = occupied
                    .iter()
                    .filter(|instant| {
                        local_date(**instant, timezone).is_ok_and(|value| value == date)
                    })
                    .count();
                let remaining_capacity =
                    usize::from(policy.daily_releases.maximum).saturating_sub(existing_today);
                if remaining_capacity > 0 {
                    let remaining = requested_slots - selected.len();
                    let daily_minimum =
                        usize::from(policy.daily_releases.minimum).saturating_sub(existing_today);
                    let daily_maximum = remaining_capacity.min(remaining);
                    let wanted = if remaining <= daily_minimum {
                        remaining
                    } else {
                        let lower = daily_minimum.min(daily_maximum);
                        lower + self.next_index(daily_maximum - lower + 1)
                    };
                    let mut candidates = candidates_for_date(
                        policy,
                        date,
                        not_before_unix_ms,
                        &occupied,
                        spacing_ms,
                    )?;
                    let count = wanted.min(candidates.len());
                    for _ in 0..count {
                        let index = self.next_index(candidates.len());
                        let instant = candidates.swap_remove(index);
                        occupied.push(instant);
                        candidates.retain(|candidate| (candidate - instant).abs() >= spacing_ms);
                        selected.push(PlannedSlot {
                            selected_at_unix_ms: instant,
                            local_date: date.to_string(),
                            timezone: policy.timezone.clone(),
                        });
                    }
                }
            }
            date = date
                .tomorrow()
                .map_err(|_| SchedulePolicyError::SchedulingHorizonExceeded)?;
        }
        if selected.len() != requested_slots {
            return Err(SchedulePolicyError::InsufficientSchedulingCapacity {
                requested: requested_slots,
                selected: selected.len(),
            });
        }
        selected.sort_unstable_by_key(|slot| slot.selected_at_unix_ms);
        Ok(selected)
    }

    fn next_index(&mut self, upper_bound: usize) -> usize {
        debug_assert!(upper_bound > 0);
        // xorshift64*; sufficient here because this is a scheduling preference, not entropy.
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        let value = self.state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (value as usize) % upper_bound
    }
}

fn candidates_for_date(
    policy: &SchedulePolicy,
    date: jiff::civil::Date,
    not_before_unix_ms: i64,
    occupied: &[i64],
    spacing_ms: i64,
) -> Result<Vec<i64>, SchedulePolicyError> {
    let mut candidates = Vec::new();
    for window in &policy.windows {
        // Jiff's compatible conversion handles a daylight-saving gap by selecting the first
        // valid local instant after it, and a fold by selecting the earlier offset.
        let start = date
            .at(window.start.hour as i8, window.start.minute as i8, 0, 0)
            .in_tz(policy.timezone.as_str())
            .map_err(|_| SchedulePolicyError::SchedulingHorizonExceeded)?
            .timestamp()
            .as_millisecond();
        let end = date
            .at(window.end.hour as i8, window.end.minute as i8, 0, 0)
            .in_tz(policy.timezone.as_str())
            .map_err(|_| SchedulePolicyError::SchedulingHorizonExceeded)?
            .timestamp()
            .as_millisecond();
        let first = start.max(not_before_unix_ms).div_euclid(60_000) * 60_000
            + if start.max(not_before_unix_ms).rem_euclid(60_000) == 0 {
                0
            } else {
                60_000
            };
        let mut candidate = first;
        while candidate < end {
            if occupied
                .iter()
                .all(|existing| (candidate - existing).abs() >= spacing_ms)
            {
                candidates.push(candidate);
            }
            candidate += 60_000;
        }
    }
    Ok(candidates)
}

fn local_date(
    instant_unix_ms: i64,
    timezone: &str,
) -> Result<jiff::civil::Date, SchedulePolicyError> {
    Timestamp::from_millisecond(instant_unix_ms)
        .map_err(|_| SchedulePolicyError::InvalidSchedulingTimestamp(instant_unix_ms))?
        .in_tz(timezone)
        .map(|zoned| zoned.date())
        .map_err(|_| SchedulePolicyError::InvalidTimeZone(timezone.to_owned()))
}

fn weekday_from_jiff(weekday: jiff::civil::Weekday) -> Weekday {
    match weekday {
        jiff::civil::Weekday::Monday => Weekday::Monday,
        jiff::civil::Weekday::Tuesday => Weekday::Tuesday,
        jiff::civil::Weekday::Wednesday => Weekday::Wednesday,
        jiff::civil::Weekday::Thursday => Weekday::Thursday,
        jiff::civil::Weekday::Friday => Weekday::Friday,
        jiff::civil::Weekday::Saturday => Weekday::Saturday,
        jiff::civil::Weekday::Sunday => Weekday::Sunday,
    }
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
    #[error("scheduling timestamp {0} is outside the supported range")]
    InvalidSchedulingTimestamp(i64),
    #[error(
        "not enough eligible future schedule capacity: requested {requested}, selected {selected}"
    )]
    InsufficientSchedulingCapacity { requested: usize, selected: usize },
    #[error("the scheduling horizon exceeded {MAX_SCHEDULING_HORIZON_DAYS} days")]
    SchedulingHorizonExceeded,
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

    fn unix_ms(value: &str) -> i64 {
        value.parse::<Timestamp>().unwrap().as_millisecond()
    }

    #[test]
    fn seeded_generation_is_repeatable_and_honors_daily_limits_and_spacing() {
        let not_before = unix_ms("2026-01-05T13:00:00Z"); // Monday 08:00 in New York.
        let mut first = SlotGenerator::seeded(41);
        let mut second = SlotGenerator::seeded(41);
        let selected = first.select(&policy(), not_before, 5, &[]).unwrap();
        assert_eq!(
            selected,
            second.select(&policy(), not_before, 5, &[]).unwrap()
        );
        assert_eq!(selected.len(), 5);
        assert!(selected.windows(2).all(|pair| {
            pair[1].selected_at_unix_ms - pair[0].selected_at_unix_ms >= 90 * 60_000
        }));
        let monday = selected
            .iter()
            .filter(|slot| slot.local_date == "2026-01-05")
            .count();
        assert!((1..=3).contains(&monday));
        assert!(
            selected
                .iter()
                .all(|slot| slot.selected_at_unix_ms >= not_before)
        );
    }

    #[test]
    fn existing_slots_are_never_redrawn_or_crowded() {
        let not_before = unix_ms("2026-01-05T13:00:00Z");
        let existing = unix_ms("2026-01-05T15:00:00Z"); // 10:00 in New York.
        let selected = SlotGenerator::seeded(99)
            .select(&policy(), not_before, 3, &[existing])
            .unwrap();
        assert!(
            selected
                .iter()
                .all(|slot| { (slot.selected_at_unix_ms - existing).abs() >= 90 * 60_000 })
        );
        let monday = selected
            .iter()
            .filter(|slot| slot.local_date == "2026-01-05")
            .count();
        assert!(
            monday <= 2,
            "the existing Monday slot consumes daily capacity"
        );
    }
}
