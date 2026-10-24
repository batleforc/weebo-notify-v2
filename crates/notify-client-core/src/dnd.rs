//! The quick do-not-disturb presets the tray, TUI and CLI share (§7.4, §9.2, §10bis.3ter):
//! one place turns a label into the `until` instant the preference stores.

use chrono::{DateTime, NaiveTime, TimeZone as _, Utc};
use chrono_tz::Tz;

use crate::ClientError;

/// This machine's clock, via `time` (the workspace's chrono has no `clock` feature).
///
/// # Errors
///
/// [`ClientError::Invalid`] if the clock reads before 1970 or out of `i64` range.
pub fn now_utc() -> Result<DateTime<Utc>, ClientError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ClientError::Invalid {
            detail: "this machine's clock is before 1970".to_owned(),
        })?;
    let now = i64::try_from(now.as_secs()).map_err(|_| ClientError::Invalid {
        detail: "this machine's clock is out of range".to_owned(),
    })?;

    DateTime::from_timestamp(now, 0).ok_or_else(|| ClientError::Invalid {
        detail: "this machine's clock is out of range".to_owned(),
    })
}

/// Whether the quick do-not-disturb toggle (§7.4's `until`) is in force right now.
/// Never a `quiet_hours` schedule: only the server evaluates those.
#[must_use]
pub fn quiet_now(until: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    until.is_some_and(|until| now < until)
}

/// The hour "until the morning" resolves to, on the preference's clock.
const MORNING_HOUR: u32 = 8;

/// A quick do-not-disturb choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DndPreset {
    /// "Une heure".
    OneHour,
    /// "Jusqu'à demain 8h" — see [`DndPreset::until`] for what that resolves to.
    UntilMorning,
    /// "Désactiver".
    Off,
}

impl DndPreset {
    /// The instant this silences until, or `None` for [`DndPreset::Off`].
    ///
    /// "Until morning" is the next 08:00 on the preference's zone (§7.4: the zone belongs to
    /// the preference, not the device), which at 01:00 is today's, not tomorrow's.
    ///
    /// # Errors
    ///
    /// [`ClientError::Invalid`] if the zone is not an IANA name, or if 08:00 is skipped by a
    /// clock jump that day (refused rather than silently moved).
    pub fn until(
        self,
        now: DateTime<Utc>,
        timezone: &str,
    ) -> Result<Option<DateTime<Utc>>, ClientError> {
        match self {
            Self::Off => return Ok(None),
            Self::OneHour => {
                let later = now
                    .checked_add_signed(chrono::Duration::hours(1))
                    .unwrap_or(DateTime::<Utc>::MAX_UTC);
                return Ok(Some(later));
            }
            Self::UntilMorning => {}
        }

        let zone: Tz = timezone.parse().map_err(|_| ClientError::Invalid {
            detail: format!("`{timezone}` is not an IANA timezone name, such as `Europe/Paris`"),
        })?;

        let morning =
            NaiveTime::from_hms_opt(MORNING_HOUR, 0, 0).ok_or_else(|| ClientError::Invalid {
                detail: "the morning hour is not a time".to_owned(),
            })?;

        let here = now.with_timezone(&zone);
        // Today's 08:00 if it is still ahead, tomorrow's otherwise.
        let date = if here.time() < morning {
            here.date_naive()
        } else {
            here.date_naive()
                .succ_opt()
                .ok_or_else(|| ClientError::Invalid {
                    detail: "there is no day after this one".to_owned(),
                })?
        };

        zone.from_local_datetime(&date.and_time(morning))
            // Clocks went back and 08:00 happened twice: the earlier is when morning arrived.
            .earliest()
            .map(|local| Some(local.with_timezone(&Utc)))
            .ok_or_else(|| ClientError::Invalid {
                detail: format!("08:00 does not exist on {date} in {timezone}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> DateTime<Utc> {
        raw.parse().expect("a test instant")
    }

    #[test]
    fn an_hour_is_an_hour_wherever_you_are() {
        // No calendar involved: an hour is an hour.
        assert_eq!(
            DndPreset::OneHour
                .until(at("2026-08-07T14:30:00Z"), "Europe/Paris")
                .unwrap(),
            Some(at("2026-08-07T15:30:00Z"))
        );
    }

    #[test]
    fn off_silences_until_nothing() {
        assert_eq!(
            DndPreset::Off
                .until(at("2026-08-07T14:30:00Z"), "Europe/Paris")
                .unwrap(),
            None
        );
    }

    #[test]
    fn the_morning_is_the_next_eight_and_not_always_tomorrows() {
        // 01:00 in Paris: "until I'm up" is seven hours, not thirty-one.
        assert_eq!(
            DndPreset::UntilMorning
                .until(at("2026-08-06T23:00:00Z"), "Europe/Paris")
                .unwrap(),
            Some(at("2026-08-07T06:00:00Z")),
        );
    }

    #[test]
    fn after_eight_it_is_tomorrows() {
        // 09:30 in Paris — the morning has been and gone.
        assert_eq!(
            DndPreset::UntilMorning
                .until(at("2026-08-07T07:30:00Z"), "Europe/Paris")
                .unwrap(),
            Some(at("2026-08-08T06:00:00Z")),
        );
    }

    #[test]
    fn the_zone_is_the_preferences_and_not_the_machines() {
        // §7.4: one instant, two zones, two answers.
        let now = at("2026-08-07T12:00:00Z");

        assert_eq!(
            DndPreset::UntilMorning.until(now, "Europe/Paris").unwrap(),
            Some(at("2026-08-08T06:00:00Z"))
        );
        assert_eq!(
            DndPreset::UntilMorning.until(now, "Asia/Tokyo").unwrap(),
            Some(at("2026-08-07T23:00:00Z"))
        );
    }

    #[test]
    fn a_clock_change_is_resolved_at_the_target_and_not_at_today() {
        // Paris springs forward 29 March 2026; reusing today's offset would land an hour late.
        assert_eq!(
            DndPreset::UntilMorning
                .until(at("2026-03-28T12:00:00Z"), "Europe/Paris")
                .unwrap(),
            Some(at("2026-03-29T06:00:00Z")),
        );
    }

    #[test]
    fn a_zone_that_is_not_one_is_refused() {
        assert!(
            DndPreset::UntilMorning
                .until(at("2026-08-07T12:00:00Z"), "Middle-earth/Shire")
                .is_err()
        );
    }

    #[test]
    fn now_utc_reads_a_plausible_instant() {
        // Not pinned to a literal: it just has to land somewhere sane.
        let now = now_utc().unwrap();
        assert!(now.timestamp() > 1_700_000_000, "clearly not 1970");
    }

    #[test]
    fn quiet_now_is_true_strictly_before_the_deadline() {
        let now = at("2026-08-07T12:00:00Z");
        assert!(quiet_now(Some(at("2026-08-07T12:00:01Z")), now));
        assert!(
            !quiet_now(Some(at("2026-08-07T12:00:00Z")), now),
            "the boundary itself has passed"
        );
        assert!(!quiet_now(Some(at("2026-08-07T11:00:00Z")), now));
    }

    #[test]
    fn no_deadline_is_never_quiet() {
        assert!(!quiet_now(None, at("2026-08-07T12:00:00Z")));
    }
}
