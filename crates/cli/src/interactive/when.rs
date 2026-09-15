//! Choosing when the work ships.
//!
//! The person names a date and a clock time; the repository's schedule policy names the zone. That
//! combination is turned into an exact instant with `jiff`, so a wall-clock time means the same
//! reading on both sides of a daylight-saving change. The instant is then checked by the daemon
//! against the same policy the scheduler draws from — this module never decides what is allowed.

use inquire::Text;
use reccursive_protocol::ReadyWorkGroup;

use crate::{CliFailure, EXIT_ACTION_REQUIRED};

pub struct Chosen {
    pub instant_unix_ms: i64,
}

/// Asks for a date and a time in the repository's own zone.
pub fn choose(group: &ReadyWorkGroup) -> Result<Chosen, CliFailure> {
    let zone = group.timezone.clone();
    let date = prompt(
        "Date (YYYY-MM-DD)",
        &format!("the day this should ship, in {zone}"),
    )?;
    let time = prompt("Time (HH:MM, 24-hour)", &format!("local time in {zone}"))?;
    let instant_unix_ms = to_instant(&date, &time, &zone)?;
    Ok(Chosen { instant_unix_ms })
}

fn prompt(label: &str, help: &str) -> Result<String, CliFailure> {
    match Text::new(label).with_help_message(help).prompt() {
        Ok(value) => Ok(value.trim().to_owned()),
        Err(inquire::InquireError::OperationCanceled)
        | Err(inquire::InquireError::OperationInterrupted) => Err(CliFailure::new(
            crate::EXIT_SUCCESS,
            "cancelled",
            "Nothing was scheduled.",
        )),
        Err(error) => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "prompt_failed",
            error.to_string(),
        )),
    }
}

/// Reads a civil date and time in a named zone into the instant they denote.
///
/// Public so the conversion can be tested without a terminal: this is the one piece of the wizard
/// where being wrong means scheduling a release at the wrong moment.
pub fn to_instant(date: &str, time: &str, zone: &str) -> Result<i64, CliFailure> {
    let invalid = |detail: String| CliFailure::new(EXIT_ACTION_REQUIRED, "invalid_time", detail);

    let date: jiff::civil::Date = date.parse().map_err(|_| {
        invalid(format!(
            "{date:?} is not a date. Use YYYY-MM-DD, like 2026-09-18."
        ))
    })?;
    let (hour, minute) = parse_clock(time)?;
    let civil = date.at(hour, minute, 0, 0).in_tz(zone).map_err(|error| {
        invalid(format!(
            "{date} {time} is not a valid time in {zone}: {error}"
        ))
    })?;
    Ok(civil.timestamp().as_millisecond())
}

/// Accepts `14:30` and `2:30 PM`, because people write both.
fn parse_clock(value: &str) -> Result<(i8, i8), CliFailure> {
    let invalid = || {
        CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "invalid_time",
            format!("{value:?} is not a time. Use HH:MM, like 10:30 or 14:00."),
        )
    };
    let lowered = value.trim().to_ascii_lowercase();
    let (clock, shift) = match lowered.strip_suffix("pm").map(str::trim) {
        Some(clock) => (clock.to_owned(), 12),
        None => (
            lowered
                .strip_suffix("am")
                .map(str::trim)
                .unwrap_or(&lowered)
                .to_owned(),
            0,
        ),
    };
    let (hour, minute) = clock.split_once(':').ok_or_else(invalid)?;
    let hour: i8 = hour.trim().parse().map_err(|_| invalid())?;
    let minute: i8 = minute.trim().parse().map_err(|_| invalid())?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return Err(invalid());
    }
    // 12 AM is midnight and 12 PM is noon, which the naive "add twelve" rule gets backwards.
    let hour = match (shift, hour) {
        (12, 12) => 12,
        (12, other) => other + 12,
        (0, 12) if lowered.ends_with("am") => 0,
        (_, other) => other,
    };
    if hour > 23 {
        return Err(invalid());
    }
    Ok((hour, minute))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_civil_time_becomes_the_instant_that_wall_clock_reading_denotes() {
        // 2026-09-18 10:30 in New York is 14:30 UTC, because September is daylight saving time.
        let instant = to_instant("2026-09-18", "10:30", "America/New_York").unwrap();
        let rendered = jiff::Timestamp::from_millisecond(instant)
            .unwrap()
            .in_tz("UTC")
            .unwrap()
            .strftime("%Y-%m-%d %H:%M")
            .to_string();
        assert_eq!(rendered, "2026-09-18 14:30");
    }

    #[test]
    fn the_same_wall_clock_reading_survives_a_daylight_saving_change() {
        // Standard time, so the same reading is an hour later in absolute terms than it would be
        // in summer. Getting this wrong ships an hour early or late half the year.
        let instant = to_instant("2026-11-18", "10:30", "America/New_York").unwrap();
        let rendered = jiff::Timestamp::from_millisecond(instant)
            .unwrap()
            .in_tz("UTC")
            .unwrap()
            .strftime("%Y-%m-%d %H:%M")
            .to_string();
        assert_eq!(rendered, "2026-11-18 15:30");
    }

    #[test]
    fn both_the_ways_people_write_a_time_are_accepted() {
        let zone = "America/New_York";
        assert_eq!(
            to_instant("2026-09-18", "14:00", zone).unwrap(),
            to_instant("2026-09-18", "2:00 PM", zone).unwrap()
        );
        assert_eq!(
            to_instant("2026-09-18", "09:30", zone).unwrap(),
            to_instant("2026-09-18", "9:30 am", zone).unwrap()
        );
        // Noon and midnight are where "add twelve" goes wrong.
        assert_eq!(
            to_instant("2026-09-18", "12:00", zone).unwrap(),
            to_instant("2026-09-18", "12:00 PM", zone).unwrap()
        );
        assert_eq!(
            to_instant("2026-09-18", "00:00", zone).unwrap(),
            to_instant("2026-09-18", "12:00 AM", zone).unwrap()
        );
    }

    #[test]
    fn nonsense_is_refused_with_an_example_rather_than_a_parser_error() {
        for (date, time) in [
            ("18-09-2026", "10:30"),
            ("2026-09-18", "half past ten"),
            ("2026-09-18", "25:00"),
            ("2026-09-18", "10:99"),
            ("", "10:30"),
        ] {
            let failure = to_instant(date, time, "America/New_York").unwrap_err();
            assert_eq!(failure.code, "invalid_time");
            assert!(
                failure.message.contains("like"),
                "no example offered: {}",
                failure.message
            );
        }
    }
}
