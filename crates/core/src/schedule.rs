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
// Internally tagged rather than adjacently tagged: with `content = "max_releases"` the catch-up
// variant serialized as {"kind":"catch_up","max_releases":{"max_releases":5}}, nesting a field
// inside a container of the same name. Tagging internally gives the shape a person would write.
#[serde(rename_all = "snake_case", tag = "kind")]
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

impl SchedulePolicy {
    /// Reports whether one exact instant is a release time this policy permits.
    ///
    /// The counterpart to `SlotGenerator::select`: that one *chooses* a time, this one *checks*
    /// one a person chose. Both have to agree, so the rules are expressed once here and the
    /// generator's own candidate construction is the only other place that knows them.
    ///
    /// Written as a predicate rather than as "find the nearest allowed time and use that",
    /// deliberately. Silently moving a requested release is worse than refusing it: somebody who
    /// asked for 10:30 and got 14:05 has been told a release is scheduled and not told when.
    ///
    /// All civil-time arithmetic goes through the policy's own zone, so a window is the wall-clock
    /// window a person means on the day in question, including across a daylight-saving change.
    pub fn validate_requested_slot(
        &self,
        requested_unix_ms: i64,
        not_before_unix_ms: i64,
        existing_slots_unix_ms: &[i64],
    ) -> Result<PlannedSlot, SchedulePolicyError> {
        if requested_unix_ms < 0 {
            return Err(SchedulePolicyError::InvalidSchedulingTimestamp(
                requested_unix_ms,
            ));
        }
        let timezone = self.timezone.as_str();
        let requested = Timestamp::from_millisecond(requested_unix_ms)
            .map_err(|_| SchedulePolicyError::InvalidSchedulingTimestamp(requested_unix_ms))?
            .in_tz(timezone)
            .map_err(|_| SchedulePolicyError::InvalidTimeZone(timezone.to_owned()))?;
        let requested_local = requested.strftime("%Y-%m-%d %H:%M").to_string();

        if requested_unix_ms < not_before_unix_ms {
            return Err(SchedulePolicyError::RequestedTimeInPast { requested_local });
        }

        let date = requested.date();
        let weekday = weekday_from_jiff(date.weekday());
        if !self.allowed_days.contains(&weekday) {
            return Err(SchedulePolicyError::RequestedDayNotAllowed { weekday });
        }

        // Asked of the same candidate construction the generator uses, so a time this accepts is
        // one the generator could itself have produced. `not_before` is the requested instant so
        // the whole window is offered and the minute alignment is the generator's own.
        let spacing_ms = i64::from(self.minimum_spacing_minutes) * 60_000;
        let window_minutes = candidates_for_date(self, date, requested_unix_ms, &[], spacing_ms)?;
        if !window_minutes.contains(&requested_unix_ms) {
            return Err(SchedulePolicyError::RequestedTimeOutsideWindows { requested_local });
        }

        // Spacing and the daily maximum are about what is already scheduled, so they are checked
        // against the live slots rather than against the window shape.
        if let Some(conflict) = existing_slots_unix_ms
            .iter()
            .copied()
            .filter(|existing| *existing != requested_unix_ms)
            .min_by_key(|existing| (existing - requested_unix_ms).abs())
            && (conflict - requested_unix_ms).abs() < spacing_ms
        {
            return Err(SchedulePolicyError::RequestedTimeTooClose {
                requested_local,
                required_minutes: self.minimum_spacing_minutes,
                actual_minutes: (conflict - requested_unix_ms).abs() / 60_000,
            });
        }

        let existing_today = existing_slots_unix_ms
            .iter()
            .filter(|instant| {
                **instant != requested_unix_ms
                    && local_date(**instant, timezone).is_ok_and(|value| value == date)
            })
            .count();
        if existing_today >= usize::from(self.daily_releases.maximum) {
            return Err(SchedulePolicyError::RequestedDayIsFull {
                local_date: date.to_string(),
                maximum: self.daily_releases.maximum,
            });
        }

        Ok(PlannedSlot {
            selected_at_unix_ms: requested_unix_ms,
            local_date: date.to_string(),
            timezone: self.timezone.clone(),
        })
    }

    /// The publishing windows that apply on the day an instant falls in, as local clock times.
    ///
    /// Exists so a refusal can say what *is* allowed instead of only what is not. Empty when the
    /// weekday itself is not permitted.
    #[must_use]
    pub fn windows_on(&self, instant_unix_ms: i64) -> Vec<DailyWindow> {
        let Ok(date) = local_date(instant_unix_ms, self.timezone.as_str()) else {
            return Vec::new();
        };
        if !self
            .allowed_days
            .contains(&weekday_from_jiff(date.weekday()))
        {
            return Vec::new();
        }
        self.windows.clone()
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
    #[error("{requested_local} has already passed")]
    RequestedTimeInPast { requested_local: String },
    #[error("{weekday:?} is not a day this repository publishes on")]
    RequestedDayNotAllowed { weekday: Weekday },
    #[error("{requested_local} is outside this repository's publishing hours")]
    RequestedTimeOutsideWindows { requested_local: String },
    #[error(
        "{requested_local} is within {actual_minutes} minutes of another scheduled release; \
         this repository requires at least {required_minutes} minutes between releases"
    )]
    RequestedTimeTooClose {
        requested_local: String,
        required_minutes: u16,
        actual_minutes: i64,
    },
    #[error("{local_date} already has the {maximum} releases this repository allows in a day")]
    RequestedDayIsFull { local_date: String, maximum: u8 },
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

    /// Monday-to-Friday business hours, which is what the requested-time tests are about. The
    /// shared `policy()` helper permits only Monday to Wednesday, and reusing it here would have
    /// made every weekday assertion below mean something other than it says.
    fn business_policy() -> SchedulePolicy {
        SchedulePolicy::new(
            IanaTimeZone::new("America/New_York").unwrap(),
            BTreeSet::from([
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
            ]),
            vec![DailyWindow::new(time(9, 0), time(17, 0)).unwrap()],
            DailyReleaseRange::new(1, 3).unwrap(),
            90,
            MissedWindowBehavior::RescheduleForward,
        )
        .unwrap()
    }

    /// Builds an instant from a civil time in the policy's zone, the way a person naming a date
    /// and a clock time means it.
    fn at_local(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> i64 {
        jiff::civil::date(year, month, day)
            .at(hour, minute, 0, 0)
            .in_tz("America/New_York")
            .unwrap()
            .timestamp()
            .as_millisecond()
    }

    #[test]
    fn a_requested_time_inside_a_window_on_an_allowed_day_is_accepted_exactly() {
        // 2026-09-18 is a Friday. The whole point is that the instant comes back unchanged: a
        // scheduler that accepted 10:30 and stored 10:31 would be lying to the person who asked.
        let requested = at_local(2026, 9, 18, 10, 30);
        let slot = business_policy()
            .validate_requested_slot(requested, at_local(2026, 9, 17, 9, 0), &[])
            .unwrap();
        assert_eq!(slot.selected_at_unix_ms, requested);
        assert_eq!(slot.local_date, "2026-09-18");
        assert_eq!(slot.timezone.as_str(), "America/New_York");
    }

    #[test]
    fn a_requested_time_outside_the_window_is_refused_on_both_sides() {
        let now = at_local(2026, 9, 17, 9, 0);
        for (hour, minute) in [(8, 59), (22, 30), (17, 0), (0, 0)] {
            let requested = at_local(2026, 9, 18, hour, minute);
            assert!(
                matches!(
                    business_policy().validate_requested_slot(requested, now, &[]),
                    Err(SchedulePolicyError::RequestedTimeOutsideWindows { .. })
                ),
                "{hour:02}:{minute:02} was not refused as outside the window"
            );
        }
    }

    #[test]
    fn a_requested_time_on_a_day_the_repository_does_not_publish_is_refused() {
        // 2026-09-19 is a Saturday, and the policy permits weekdays only.
        let requested = at_local(2026, 9, 19, 10, 30);
        assert!(matches!(
            business_policy().validate_requested_slot(requested, at_local(2026, 9, 17, 9, 0), &[]),
            Err(SchedulePolicyError::RequestedDayNotAllowed {
                weekday: Weekday::Saturday
            })
        ));
    }

    #[test]
    fn a_requested_time_in_the_past_is_refused_rather_than_moved() {
        let requested = at_local(2026, 9, 18, 10, 30);
        assert!(matches!(
            business_policy().validate_requested_slot(requested, at_local(2026, 9, 18, 11, 0), &[]),
            Err(SchedulePolicyError::RequestedTimeInPast { .. })
        ));
    }

    #[test]
    fn a_requested_time_too_close_to_an_existing_release_is_refused_and_says_by_how_much() {
        // The policy requires 90 minutes of spacing.
        let existing = at_local(2026, 9, 18, 10, 0);
        let requested = at_local(2026, 9, 18, 11, 0);
        match business_policy().validate_requested_slot(
            requested,
            at_local(2026, 9, 17, 9, 0),
            &[existing],
        ) {
            Err(error @ SchedulePolicyError::RequestedTimeTooClose { .. }) => {
                let SchedulePolicyError::RequestedTimeTooClose {
                    required_minutes,
                    actual_minutes,
                    ..
                } = &error
                else {
                    unreachable!("matched above")
                };
                assert_eq!(*required_minutes, 90);
                assert_eq!(*actual_minutes, 60);
                // The rendered message is what a person reads, and it used to stop at the bare
                // number: "this repository requires 90". A sentence that ends mid-unit is a
                // sentence nobody can act on.
                let rendered = error.to_string();
                assert!(
                    rendered.ends_with("minutes between releases"),
                    "spacing refusal reads: {rendered}"
                );
                assert!(rendered.contains("at least 90 minutes"), "{rendered}");
            }
            other => panic!("expected a spacing refusal, got {other:?}"),
        }
        // Exactly the required spacing is allowed: the rule is "at least", not "more than".
        let far_enough = at_local(2026, 9, 18, 11, 30);
        assert!(
            business_policy()
                .validate_requested_slot(far_enough, at_local(2026, 9, 17, 9, 0), &[existing])
                .is_ok()
        );
    }

    #[test]
    fn a_day_already_at_its_release_maximum_refuses_another() {
        // Three is this policy's daily maximum, and these are spaced far enough apart to make the
        // daily limit the only reason a fourth is refused.
        let existing = [
            at_local(2026, 9, 18, 9, 0),
            at_local(2026, 9, 18, 12, 0),
            at_local(2026, 9, 18, 15, 0),
        ];
        let requested = at_local(2026, 9, 18, 16, 45);
        assert!(matches!(
            business_policy().validate_requested_slot(
                requested,
                at_local(2026, 9, 17, 9, 0),
                &existing
            ),
            Err(SchedulePolicyError::RequestedDayIsFull { maximum: 3, .. })
        ));
    }

    #[test]
    fn requesting_the_time_a_unit_already_holds_is_not_a_conflict_with_itself() {
        // Re-validating an existing selection must not read that selection as a competitor, or
        // rescheduling a unit to the time it already has would refuse itself.
        let requested = at_local(2026, 9, 18, 10, 30);
        assert!(
            business_policy()
                .validate_requested_slot(requested, at_local(2026, 9, 17, 9, 0), &[requested])
                .is_ok()
        );
    }

    #[test]
    fn a_requested_time_is_the_wall_clock_time_across_a_daylight_saving_change() {
        // America/New_York leaves daylight saving on 2026-11-01. A window of 09:00-17:00 means
        // those clock readings on both sides of the change, not a fixed offset from UTC.
        let before = at_local(2026, 10, 30, 10, 30);
        let after = at_local(2026, 11, 2, 10, 30);
        let now = at_local(2026, 10, 29, 9, 0);
        assert!(
            business_policy()
                .validate_requested_slot(before, now, &[])
                .is_ok()
        );
        assert!(
            business_policy()
                .validate_requested_slot(after, now, &[])
                .is_ok()
        );
        // The clocks went back in between, so the same wall-clock reading three days later is
        // three days *and one hour* apart in absolute terms. Asserting that is what distinguishes
        // real civil-time handling from arithmetic on a fixed UTC offset, which would have put
        // one of these two outside the window.
        assert_eq!(after - before, 3 * 86_400_000 + 3_600_000);
    }

    #[test]
    fn a_repository_override_changes_what_a_requested_time_is_checked_against() {
        // Overrides refine a shared policy. A requested time has to be judged by the effective
        // policy, not the base one, or a repository's own narrower hours would not apply.
        let effective = business_policy()
            .with_override(&SchedulePolicyOverride {
                windows: Some(vec![DailyWindow::new(time(13, 0), time(14, 0)).unwrap()]),
                // A one-hour window cannot hold three releases 90 minutes apart, and the policy
                // refuses to construct a combination that cannot be satisfied.
                daily_releases: Some(DailyReleaseRange::new(1, 1).unwrap()),
                ..SchedulePolicyOverride::default()
            })
            .unwrap();
        let now = at_local(2026, 9, 17, 9, 0);
        assert!(matches!(
            effective.validate_requested_slot(at_local(2026, 9, 18, 10, 30), now, &[]),
            Err(SchedulePolicyError::RequestedTimeOutsideWindows { .. })
        ));
        assert!(
            effective
                .validate_requested_slot(at_local(2026, 9, 18, 13, 30), now, &[])
                .is_ok()
        );
    }

    #[test]
    fn windows_on_reports_what_is_allowed_so_a_refusal_can_say_so() {
        let friday = at_local(2026, 9, 18, 10, 30);
        let saturday = at_local(2026, 9, 19, 10, 30);
        let open = business_policy().windows_on(friday);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].start, time(9, 0));
        assert_eq!(open[0].end, time(17, 0));
        assert!(business_policy().windows_on(saturday).is_empty());
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

    /// Converts a local civil time in a zone to a UTC millisecond instant, for test setup.
    fn local(zone: &str, year: i16, month: i8, day: i8, hour: i8, minute: i8) -> i64 {
        jiff::civil::date(year, month, day)
            .at(hour, minute, 0, 0)
            .in_tz(zone)
            .unwrap()
            .timestamp()
            .as_millisecond()
    }

    fn local_clock(zone: &str, instant: i64) -> (i8, i8) {
        let zoned = Timestamp::from_millisecond(instant)
            .unwrap()
            .in_tz(zone)
            .unwrap();
        (zoned.hour(), zoned.minute())
    }

    fn all_days() -> BTreeSet<Weekday> {
        BTreeSet::from([
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ])
    }

    fn policy_in(zone: &str, windows: Vec<DailyWindow>, spacing: u16) -> SchedulePolicy {
        SchedulePolicy::new(
            IanaTimeZone::new(zone).unwrap(),
            all_days(),
            windows,
            DailyReleaseRange::new(1, 1).unwrap(),
            spacing,
            MissedWindowBehavior::RescheduleForward,
        )
        .unwrap()
    }

    fn window(start: (u8, u8), end: (u8, u8)) -> DailyWindow {
        DailyWindow::new(
            DailyTime::new(start.0, start.1).unwrap(),
            DailyTime::new(end.0, end.1).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn a_spring_forward_gap_never_selects_a_local_time_that_does_not_exist() {
        // US eastern time jumps 02:00 -> 03:00 on 2027-03-14; 02:30 does not occur that day.
        let zone = "America/New_York";
        let policy = policy_in(zone, vec![window((1, 0), (4, 0))], 30);
        let start = local(zone, 2027, 3, 14, 0, 0);

        let selected = SlotGenerator::seeded(9)
            .select(&policy, start, 1, &[])
            .unwrap();
        let instant = selected[0].selected_at_unix_ms;
        let (hour, _) = local_clock(zone, instant);

        assert!(
            hour != 2,
            "02:00-02:59 does not exist on this date; selected local hour was {hour}"
        );
        // Whatever was chosen is a real instant inside the window's actual span.
        assert!(instant >= local(zone, 2027, 3, 14, 1, 0));
        assert!(instant < local(zone, 2027, 3, 14, 4, 0));
    }

    #[test]
    fn a_fall_back_fold_selects_one_unambiguous_instant() {
        // 01:00-01:59 occurs twice on 2027-11-07 in US eastern time.
        let zone = "America/New_York";
        let policy = policy_in(zone, vec![window((0, 30), (3, 0))], 30);
        let start = local(zone, 2027, 11, 7, 0, 0);

        let selected = SlotGenerator::seeded(4)
            .select(&policy, start, 1, &[])
            .unwrap();
        let instant = selected[0].selected_at_unix_ms;

        // The instant is real and inside the day's window however the fold is resolved.
        assert!(instant >= start);
        assert!(instant < local(zone, 2027, 11, 7, 3, 0));
        // Round-tripping it back to civil time must not land outside the policy's window.
        let (hour, minute) = local_clock(zone, instant);
        let minutes = i32::from(hour) * 60 + i32::from(minute);
        assert!(
            (30..=180).contains(&minutes),
            "selected {hour:02}:{minute:02} is outside the configured window"
        );
    }

    #[test]
    fn a_window_ending_at_midnight_never_spills_into_the_next_day() {
        let zone = "UTC";
        let policy = policy_in(zone, vec![window((23, 0), (23, 59))], 30);
        let start = local(zone, 2027, 6, 1, 22, 0);

        let selected = SlotGenerator::seeded(2)
            .select(&policy, start, 3, &[])
            .unwrap();
        for slot in &selected {
            let (hour, minute) = local_clock(zone, slot.selected_at_unix_ms);
            assert_eq!(hour, 23, "a 23:00-23:59 window must stay within its hour");
            assert!(minute < 59);
        }
    }

    #[test]
    fn a_clock_that_jumps_backwards_still_only_selects_future_instants() {
        let zone = "UTC";
        let policy = policy_in(zone, vec![window((0, 0), (23, 59))], 60);
        let later = local(zone, 2027, 6, 10, 12, 0);
        let earlier = local(zone, 2027, 6, 10, 9, 0);

        // A slot was already chosen against the later reading of the clock.
        let ahead = SlotGenerator::seeded(6)
            .select(&policy, later, 1, &[])
            .unwrap()[0]
            .selected_at_unix_ms;

        // The clock is corrected backwards; a new selection must still be in the future of the
        // moment it is made, and must not crowd the slot already chosen.
        let after_jump = SlotGenerator::seeded(6)
            .select(&policy, earlier, 1, &[ahead])
            .unwrap()[0]
            .selected_at_unix_ms;

        assert!(after_jump >= earlier, "a selection must never be backdated");
        assert!(
            (after_jump - ahead).abs() >= 60 * 60_000,
            "a corrected clock must not crowd an existing selection"
        );
    }

    #[test]
    fn a_policy_whose_windows_are_all_in_the_past_today_moves_to_the_next_day() {
        let zone = "UTC";
        let policy = policy_in(zone, vec![window((9, 0), (10, 0))], 30);
        // Asked at 23:00, long after today's only window closed.
        let start = local(zone, 2027, 6, 1, 23, 0);

        let selected = SlotGenerator::seeded(1)
            .select(&policy, start, 1, &[])
            .unwrap();
        let instant = selected[0].selected_at_unix_ms;

        assert!(instant > start);
        assert!(
            instant >= local(zone, 2027, 6, 2, 9, 0),
            "a closed window must roll forward to the next eligible day, not backfill today"
        );
        let (hour, _) = local_clock(zone, instant);
        assert_eq!(hour, 9);
    }

    #[test]
    fn a_day_with_no_remaining_capacity_reports_it_rather_than_overfilling() {
        let zone = "UTC";
        // One hour of window at 45-minute spacing holds a single slot per day.
        let policy = SchedulePolicy::new(
            IanaTimeZone::new(zone).unwrap(),
            BTreeSet::from([Weekday::Tuesday]),
            vec![window((9, 0), (10, 0))],
            DailyReleaseRange::new(1, 1).unwrap(),
            45,
            MissedWindowBehavior::RescheduleForward,
        )
        .unwrap();
        let start = local(zone, 2027, 6, 1, 0, 0);

        // Well beyond the horizon's capacity for a single weekday with one slot each.
        let outcome = SlotGenerator::seeded(3).select(&policy, start, 500, &[]);
        assert!(
            matches!(
                outcome,
                Err(SchedulePolicyError::InsufficientSchedulingCapacity { .. })
            ),
            "exceeding real capacity must be reported, not silently truncated: {outcome:?}"
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
