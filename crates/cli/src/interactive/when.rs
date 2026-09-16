//! Choosing when the work ships.
//!
//! The person names a date and a clock time; the repository's schedule policy names the zone. That
//! combination is turned into an exact instant with `jiff`, so a wall-clock time means the same
//! reading on both sides of a daylight-saving change. The instant is then checked by the daemon
//! against the same policy the scheduler draws from — this module never decides what is allowed.

use inquire::Text;
use reccursive_protocol::{Command, ReadyWorkGroup, ReleaseTimeVerdict, ResponseData};

use crate::{CliFailure, EXIT_ACTION_REQUIRED, Session, send};

pub struct Chosen {
    pub instant_unix_ms: i64,
}

/// How many times to re-ask before giving up, so a wrong keyboard cannot loop forever.
const MAX_ATTEMPTS: usize = 12;

/// Asks for a date and a time in the repository's own zone, until one the repository allows.
///
/// The refusal arrives here rather than after the review screen, which is where it used to: being
/// told on Saturday evening that Saturday is not a publishing day, only after choosing what to
/// ship and confirming it, is the wrong end of the conversation.
///
/// The verdict comes from the daemon. This module does not know the scheduling rules and must not
/// learn them — a second implementation of the policy would eventually disagree with the one that
/// actually decides.
pub fn choose(session: &Session, group: &ReadyWorkGroup) -> Result<Chosen, CliFailure> {
    choose_for(session, group.repository_id, &group.timezone)
}

/// The same conversation, for work that has no ready-work group behind it.
pub fn choose_for(
    session: &Session,
    repository_id: reccursive_protocol::RepositoryId,
    timezone: &str,
) -> Result<Chosen, CliFailure> {
    let zone = timezone.to_owned();
    let mut date: Option<String> = None;

    for _ in 0..MAX_ATTEMPTS {
        // A rejected weekday means the date is wrong; a rejected hour means the time is. Keeping
        // the good half saves retyping the part that was already right.
        let chosen_date = match date.take() {
            Some(existing) => existing,
            None => prompt(
                "Date (YYYY-MM-DD)",
                &format!("the day this should ship, in {zone}"),
            )?,
        };
        let time = prompt("Time (HH:MM, 24-hour)", &format!("local time in {zone}"))?;

        let instant_unix_ms = match to_instant(&chosen_date, &time, &zone) {
            Ok(instant) => instant,
            Err(failure) => {
                // Malformed input never reached the daemon, so it is explained here and both
                // fields are asked again.
                println!("\n{}\n", failure.message);
                continue;
            }
        };

        match validate(session, repository_id, instant_unix_ms)? {
            ReleaseTimeVerdict::Allowed => return Ok(Chosen { instant_unix_ms }),
            ReleaseTimeVerdict::Refused { reason, message } => {
                println!("\n{message}\n");
                if !reason.asks_for_a_new_date() {
                    date = Some(chosen_date);
                }
            }
        }
    }
    Err(CliFailure::new(
        EXIT_ACTION_REQUIRED,
        "no_valid_time",
        "No release time was accepted. Nothing was scheduled.",
    ))
}

/// Asks the daemon whether an instant is currently a time this repository may publish at.
///
/// Advisory: it reserves nothing, and scheduling checks again. A refusal after this one is a real
/// answer about a queue that changed, not a contradiction.
fn validate(
    session: &Session,
    repository_id: reccursive_protocol::RepositoryId,
    requested_at_unix_ms: i64,
) -> Result<ReleaseTimeVerdict, CliFailure> {
    match send(
        session,
        Command::ValidateReleaseTime {
            repository_id,
            requested_at_unix_ms,
        },
    )? {
        ResponseData::ReleaseTimeValidated { verdict } => Ok(verdict),
        _ => Err(CliFailure::new(
            EXIT_ACTION_REQUIRED,
            "unexpected_response",
            "the service did not answer whether that time is allowed",
        )),
    }
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
