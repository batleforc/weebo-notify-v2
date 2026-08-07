//! The quick do-not-disturb choices every client offers (§7.4, §9.2, §10bis.3ter).
//!
//! # Why the arithmetic is here and not in each client
//!
//! The tray menu (§10bis.3ter), the TUI's `:` line (§9.2) and the CLI all offer the
//! same short list, and each has to turn a label into an **instant** before it can
//! write the preference — §7.4 stores `until`, not "an hour from whenever you read
//! this". D22 is why that is not left to them: the do-not-disturb *rule* lived in
//! three copies once, and the fix was one owner. This is the same shape, one layer
//! up — three menus, one calculation.
//!
//! It is in `notify-client-core` and not in `notify-domain` because the server never
//! asks this question. A preset is an affordance of a client, and the clients depend
//! on this crate rather than on the server's domain.
//!
//! The web is TypeScript and cannot call it, which is a real limit and worth naming
//! rather than glossing: what this buys is that the three *Rust* surfaces agree, and
//! that a reviewer checking the rule has one file to read.

use chrono::{DateTime, NaiveTime, TimeZone as _, Utc};
use chrono_tz::Tz;

use crate::ClientError;

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
    /// # What "tomorrow 8am" actually means
    ///
    /// **The next 08:00 on the preference's clock**, which is not always tomorrow's.
    /// §10bis.3ter labels it "jusqu'à demain 8h", and read literally at 01:00 that
    /// would silence for thirty-one hours — straight through the coming working day,
    /// which is the opposite of what somebody reaching for it at one in the morning
    /// wants. The preset means "until I am up again", so it resolves to the next
    /// 08:00 and it is the label that is slightly loose.
    ///
    /// # Why the caller supplies the zone
    ///
    /// §7.4 is explicit that the zone belongs to the preference and not to the
    /// device: somebody travelling keeps their own mornings. This function will not
    /// reach for the machine's zone, because that is the bug the design names.
    ///
    /// # Errors
    ///
    /// [`ClientError::Invalid`] if the zone is not an IANA name, or if 08:00 does not
    /// exist on the day it lands on — a clock jump that skips it. Refused rather
    /// than nudged: silencing until a *different* hour than the one on the menu is
    /// the kind of thing nobody reports and everybody stops trusting.
    pub fn until(
        self,
        now: DateTime<Utc>,
        timezone: &str,
    ) -> Result<Option<DateTime<Utc>>, ClientError> {
        match self {
            Self::Off => return Ok(None),
            Self::OneHour => return Ok(Some(now + chrono::Duration::hours(1))),
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
            // Ambiguous means the clocks went back and 08:00 happened twice; the
            // earlier is right, because it is the first moment the morning arrived.
            .earliest()
            .map(|local| Some(local.with_timezone(&Utc)))
            .ok_or_else(|| ClientError::Invalid {
                detail: format!("08:00 does not exist on {date} in {timezone}"),
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn at(raw: &str) -> DateTime<Utc> {
        raw.parse().expect("a test instant")
    }

    #[test]
    fn an_hour_is_an_hour_wherever_you_are() {
        // No calendar involved: an hour is an hour, and resolving it through a zone
        // would be arithmetic looking for a way to be wrong.
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
        // 01:00 in Paris. The literal reading of the label would silence for
        // thirty-one hours, through the whole coming working day; somebody reaching
        // for this at one in the morning means "until I am up", which is seven.
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
        // §7.4: somebody travelling keeps their own mornings. One instant, two
        // answers — and that difference is the whole reason the zone is stored with
        // the preference rather than read off the device.
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
        // Europe/Paris springs forward at 02:00 on 29 March 2026. The trap is taking
        // the offset *now* — still winter — and reusing it for a target that lands
        // after the jump, which would put the deadline an hour late.
        assert_eq!(
            DndPreset::UntilMorning
                .until(at("2026-03-28T12:00:00Z"), "Europe/Paris")
                .unwrap(),
            Some(at("2026-03-29T06:00:00Z")),
        );
    }

    #[test]
    fn a_zone_that_is_not_one_is_refused() {
        // The same refusal the preference itself makes: a zone accepted because it
        // looked plausible is a silence that lifts at the wrong hour in six months,
        // with nothing to connect the two.
        assert!(
            DndPreset::UntilMorning
                .until(at("2026-08-07T12:00:00Z"), "Middle-earth/Shire")
                .is_err()
        );
    }
}
