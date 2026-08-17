//! Do not disturb: the planned, broad half of "stop notifying me" (design §7.4).
//!
//! [`Silence`](crate::silence) is the other half — reactive and targeted, one
//! notification or one `group_key`. This one is about *time* and *channels*: nights,
//! weekends, an afternoon off, everything except the one channel that may always
//! ring.
//!
//! # It lives on the server, and it is one question
//!
//! The preference is per user and stored server-side, so turning it on from a phone
//! quiets a laptop too (§7.4) — and every client is told, over the `preference`
//! event of §8.1, rather than each of them keeping its own idea. What all of them
//! then ask is the same single question, [`Dnd::is_muted`]: *should this channel, at
//! this priority, make a noise right now?* Nothing else in the product is allowed to
//! reason about what "DND" means; a second implementation of that answer is how a
//! phone ends up buzzing at 3am while the desktop stays quiet.
//!
//! What it suppresses is **push and local pop-ups**, never delivery: the stream
//! keeps flowing and the inbox keeps filling (§7.4). An app somebody chose to leave
//! open is not an interruption.
//!
//! # The timezone belongs to the preference
//!
//! `22:00–08:00, Europe/Paris` means *that person's* nights, not the nights of
//! whatever zone their laptop is in this month. So the zone is stored alongside the
//! window and resolved against the compiled-in IANA database, exactly as the profile
//! timezone of §2.4 is — and for the same reason: a zone accepted today because it
//! merely looked plausible is a notification that fails to arrive in six months,
//! with nothing to connect the two.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, Weekday};

use crate::error::{DomainError, Result};
use crate::ids::{ChannelId, UserId};
use crate::notification::Priority;

/// A day of the week, as the wire spells it.
///
/// Its own enum rather than `time::Weekday` because this one is API: `"mon"` is a
/// contract, and the serde representation of somebody else's type is not ours to
/// promise.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Day {
    /// Monday.
    Mon,
    /// Tuesday.
    Tue,
    /// Wednesday.
    Wed,
    /// Thursday.
    Thu,
    /// Friday.
    Fri,
    /// Saturday.
    Sat,
    /// Sunday.
    Sun,
}

impl From<Weekday> for Day {
    fn from(weekday: Weekday) -> Self {
        match weekday {
            Weekday::Monday => Self::Mon,
            Weekday::Tuesday => Self::Tue,
            Weekday::Wednesday => Self::Wed,
            Weekday::Thursday => Self::Thu,
            Weekday::Friday => Self::Fri,
            Weekday::Saturday => Self::Sat,
            Weekday::Sunday => Self::Sun,
        }
    }
}

/// A time of day on a wall clock, to the minute.
///
/// Minutes, not seconds: quiet hours are a human intention ("ten in the evening"),
/// and a field nobody will ever set to a non-zero value is a field somebody will
/// eventually set wrong. Stored as minutes since local midnight because that is the
/// form every comparison here wants, and rendered `HH:MM` because that is the form
/// §7.4 writes it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
// Described as the `HH:MM` string it is on the wire, not as the integer it wraps.
#[cfg_attr(feature = "openapi", schema(value_type = String, example = "22:00"))]
pub struct WallClock(u16);

impl WallClock {
    /// Build one, refusing anything a clock could not show.
    pub const fn new(hour: u8, minute: u8) -> Result<Self> {
        if hour > 23 || minute > 59 {
            return Err(DomainError::Malformed {
                field: "dnd.quiet_hours",
                reason: "not a time of day, expected `HH:MM` between 00:00 and 23:59",
            });
        }
        Ok(Self(hour as u16 * 60 + minute as u16))
    }

    /// Minutes since local midnight — the form every comparison here wants.
    #[must_use]
    pub const fn minutes(self) -> u16 {
        self.0
    }
}

impl fmt::Display for WallClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

impl FromStr for WallClock {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self> {
        let malformed = DomainError::Malformed {
            field: "dnd.quiet_hours",
            reason: "not a time of day, expected `HH:MM` between 00:00 and 23:59",
        };
        let (hour, minute) = raw.split_once(':').ok_or_else(|| malformed.clone())?;
        // Length-checked so `1:5` and `022:00` are refused rather than accepted into
        // a preference whose rendering would not match what was sent.
        if hour.len() != 2 || minute.len() != 2 {
            return Err(malformed);
        }
        let (Ok(hour), Ok(minute)) = (hour.parse(), minute.parse()) else {
            return Err(malformed);
        };
        Self::new(hour, minute)
    }
}

impl Serialize for WallClock {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WallClock {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// One recurring weekly window of quiet (design §7.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// The days it applies to. **Empty means every day** — the common case, and the
    /// one nobody should have to spell out seven times.
    ///
    /// For a window that crosses midnight the day is the one it **starts** on: a
    /// Friday `22:00–08:00` is the night from Friday to Saturday, which is what a
    /// person means by "Friday night".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<Day>,
    /// When quiet begins, on the local clock.
    pub start: WallClock,
    /// When it ends. May be earlier than `start`, which means the window crosses
    /// midnight.
    pub end: WallClock,
    /// The IANA zone `start` and `end` are read in — the preference's own, never the
    /// device's.
    pub timezone: String,
}

impl QuietHours {
    /// Build one, validated.
    ///
    /// A window whose ends are equal is refused rather than interpreted: `00:00` to
    /// `00:00` reads as "all day" to one person and "never" to another, and the one
    /// who guesses wrong finds out by missing something.
    pub fn new(
        days: Vec<Day>,
        start: WallClock,
        end: WallClock,
        timezone: impl Into<String>,
    ) -> Result<Self> {
        let timezone = timezone.into();
        validate_timezone(&timezone)?;

        if start == end {
            return Err(DomainError::Malformed {
                field: "dnd.quiet_hours",
                reason: "starts and ends at the same time — say `00:00`–`23:59` for a whole day",
            });
        }

        let mut days = days;
        days.sort_unstable();
        days.dedup();

        Ok(Self {
            days,
            start,
            end,
            timezone,
        })
    }

    /// Whether this window is open at `now`.
    ///
    /// Returns `false` when the zone cannot be resolved. That is the safe direction
    /// and worth naming: a preference is only ever stored through [`Self::new`],
    /// which validates the zone, so getting here means the row predates a tzdata
    /// change — and the failure mode of guessing is a phone that stays silent for a
    /// week without saying why.
    #[must_use]
    pub fn covers(&self, now: OffsetDateTime) -> bool {
        let Some(offset) = offset_at(&self.timezone, now) else {
            return false;
        };
        let local = now.to_offset(offset);
        let minutes = u16::from(local.hour()) * 60 + u16::from(local.minute());
        let today = Day::from(local.weekday());

        if self.start.minutes() < self.end.minutes() {
            return self.on(today) && (self.start.minutes()..self.end.minutes()).contains(&minutes);
        }

        // Crosses midnight: the tail of the day it starts on, or the head of the
        // next one — and the next one belongs to *yesterday's* entry in `days`.
        (minutes >= self.start.minutes() && self.on(today))
            || (minutes < self.end.minutes() && self.on(Day::from(local.weekday().previous())))
    }

    /// Whether a given local day is one of ours. No days listed means all of them.
    fn on(&self, day: Day) -> bool {
        self.days.is_empty() || self.days.contains(&day)
    }
}

/// How wide a do-not-disturb reaches (design §7.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
// Tagged `kind` rather than `scope`, because the field that holds it is already
// called `scope`: tagging it with its own name renders `"scope":{"scope":…}`, which
// reads like a mistake and is one to write against.
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DndScope {
    /// Every channel. What a preference means when it does not say.
    #[default]
    Everything,
    /// Only these ones are quieted; the rest ring as usual.
    Channels {
        /// The channels the preference applies to.
        channels: Vec<ChannelId>,
    },
}

impl DndScope {
    /// Whether this scope reaches a given channel.
    #[must_use]
    pub fn reaches(&self, channel: &ChannelId) -> bool {
        match self {
            Self::Everything => true,
            Self::Channels { channels } => channels.contains(channel),
        }
    }
}

/// One user's delivery preferences (design §7.4).
///
/// Carried whole by the `preference` event of §8.1 rather than as a delta: a client
/// that missed one update would otherwise apply the next to the wrong baseline.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dnd {
    /// Whose it is.
    pub user: UserId,
    /// Recurring weekly windows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quiet_hours: Vec<QuietHours>,
    /// The quick toggle: quiet until this instant, whatever the schedule says.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub until: Option<OffsetDateTime>,
    /// Which channels any of the above applies to.
    #[serde(default)]
    pub scope: DndScope,
    /// Channels that ring anyway — the on-call case: mute everything *except* the
    /// one that matters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<ChannelId>,
    /// Whether `urgent` is allowed through.
    ///
    /// Off by default, and opt-in by the **recipient**: the publisher chooses how
    /// insistent a notification is, the person receiving it chooses what may wake
    /// them (§2.3, §7.4). Note that this never reaches an iOS critical alert — no
    /// entitlement, by decision.
    #[serde(default)]
    pub urgent_bypasses: bool,
}

/// A do-not-disturb as somebody *states* it: everything except whose it is.
///
/// The draft/entity split this codebase uses everywhere else ([`ContentDraft`],
/// [`RevisionDraft`]), and here it carries a decision: **the subject is never an
/// input.** A caller states quiet hours and exceptions; the use case decides who
/// they belong to, because it is the only thing that knows.
///
/// Leaving `user` on the request body was tried and it is worse than misleading —
/// it is *required*, so a client that legitimately does not know its own id cannot
/// build a body at all, and one that guesses gets a deserialisation error about a
/// field the server was going to overwrite.
///
/// [`ContentDraft`]: crate::content::ContentDraft
/// [`RevisionDraft`]: crate::notification::RevisionDraft
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DndDraft {
    /// Recurring weekly windows. Absent means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quiet_hours: Vec<QuietHours>,
    /// Quiet until this instant. Absent ends the quick toggle.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub until: Option<OffsetDateTime>,
    /// Which channels it applies to.
    #[serde(default)]
    pub scope: DndScope,
    /// Channels that ring anyway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<ChannelId>,
    /// Whether `urgent` may come through.
    #[serde(default)]
    pub urgent_bypasses: bool,
}

impl DndDraft {
    /// Attach the subject.
    #[must_use]
    pub fn for_user(self, user: UserId) -> Dnd {
        Dnd {
            user,
            quiet_hours: self.quiet_hours,
            until: self.until,
            scope: self.scope,
            exceptions: self.exceptions,
            urgent_bypasses: self.urgent_bypasses,
        }
    }
}

impl Dnd {
    /// The preference of somebody who has never set one: nothing is muted.
    #[must_use]
    pub const fn off(user: UserId) -> Self {
        Self {
            user,
            quiet_hours: Vec::new(),
            until: None,
            scope: DndScope::Everything,
            exceptions: Vec::new(),
            urgent_bypasses: false,
        }
    }

    /// Whether a quiet period is running at `now`, ignoring which channel.
    ///
    /// The question the digest of §7.4 asks — "is this person still not to be
    /// disturbed" — and the one an interface shows as a toggle.
    #[must_use]
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.until.is_some_and(|until| now < until)
            || self.quiet_hours.iter().any(|window| window.covers(now))
    }

    /// **The** question: should this make a noise right now?
    ///
    /// The order of the answers is the design, not an optimisation:
    ///
    /// 1. **an exception rings, always** — including through a `until` somebody set
    ///    by hand. §7.4 gives the on-call case ("mute everything *except*
    ///    `cluster-ovh-1`") without saying who wins when the two disagree, and this
    ///    is the only reading that leaves it usable: the other one would mean
    ///    undoing the exception every time you silence the rest, which is precisely
    ///    the moment nobody wants to think about it;
    /// 2. **out of scope rings** — a per-channel DND is not a global one;
    /// 3. **no quiet period, no muting** — a preference that exists is not a
    ///    preference that is running;
    /// 4. **`urgent` passes only if this user said it may.**
    #[must_use]
    pub fn is_muted(&self, channel: &ChannelId, priority: Priority, now: OffsetDateTime) -> bool {
        if self.exceptions.contains(channel) || !self.scope.reaches(channel) {
            return false;
        }
        if !self.is_active(now) {
            return false;
        }
        !(priority == Priority::Urgent && self.urgent_bypasses)
    }

    /// Validate a preference assembled from the wire.
    ///
    /// The windows validated themselves on the way in ([`QuietHours::new`]); what is
    /// left is what only the whole is in a position to see.
    pub fn validated(self) -> Result<Self> {
        for window in &self.quiet_hours {
            validate_timezone(&window.timezone)?;
            if window.start == window.end {
                return Err(DomainError::Malformed {
                    field: "dnd.quiet_hours",
                    reason: "starts and ends at the same time — say `00:00`–`23:59` for a whole day",
                });
            }
        }

        // An exception outside the scope is not refused: it is harmless today and
        // still true the day the scope widens. Refusing it would make narrowing a
        // scope fail on a field the person did not touch.
        Ok(self)
    }
}

/// Check a timezone against the compiled-in IANA database.
///
/// Same check as the profile timezone of §2.4, deliberately: the two are the same
/// kind of value, and one of them being stricter than the other is how `Europe/Pris`
/// gets in through the door nobody was watching.
fn validate_timezone(name: &str) -> Result<()> {
    chrono_tz::Tz::from_str(name)
        .map(|_| ())
        .map_err(|_| DomainError::Malformed {
            field: "dnd.quiet_hours.timezone",
            reason: "not an IANA timezone name, such as `Europe/Paris`",
        })
}

/// What a zone's clocks read, relative to UTC, at a given instant.
///
/// The one place this crate borrows chrono: `chrono-tz` **is** the IANA database and
/// its interface is chrono's. Everything either side of this function is
/// `time::OffsetDateTime`, so the borrowing does not spread — and a lookup that
/// crosses a DST boundary is exactly why the offset is asked for per instant rather
/// than stored with the window.
fn offset_at(timezone: &str, at: OffsetDateTime) -> Option<UtcOffset> {
    use chrono::Offset as _;
    use chrono::TimeZone as _;

    let zone = chrono_tz::Tz::from_str(timezone).ok()?;
    let instant = chrono::DateTime::from_timestamp(at.unix_timestamp(), 0)?;
    let seconds = zone
        .offset_from_utc_datetime(&instant.naive_utc())
        .fix()
        .local_minus_utc();

    UtcOffset::from_whole_seconds(seconds).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use time::Duration as TimeDuration;
    use time::macros::datetime;

    use super::*;

    fn channel(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    fn at(hour: u8, minute: u8) -> WallClock {
        WallClock::new(hour, minute).unwrap()
    }

    fn nights() -> QuietHours {
        QuietHours::new(Vec::new(), at(22, 0), at(8, 0), "Europe/Paris").unwrap()
    }

    fn dnd(quiet_hours: Vec<QuietHours>) -> Dnd {
        Dnd {
            quiet_hours,
            ..Dnd::off(UserId::generate())
        }
    }

    // --- The wall clock -----------------------------------------------------

    #[test]
    fn a_wall_clock_round_trips_through_the_text_the_wire_carries() {
        assert_eq!(at(22, 0).to_string(), "22:00");
        assert_eq!("22:00".parse::<WallClock>().unwrap(), at(22, 0));
        assert_eq!("08:30".parse::<WallClock>().unwrap(), at(8, 30));
    }

    #[test]
    fn a_time_no_clock_could_show_is_refused() {
        for bad in [
            "24:00", "22:60", "2:00", "22:0", "22", "22:00:00", "", "ab:cd",
        ] {
            assert!(bad.parse::<WallClock>().is_err(), "`{bad}` parsed");
        }
        assert!(WallClock::new(24, 0).is_err());
        assert!(WallClock::new(0, 60).is_err());
    }

    // --- Quiet hours --------------------------------------------------------

    #[test]
    fn a_window_that_starts_and_ends_together_is_refused_rather_than_guessed() {
        let refused = QuietHours::new(Vec::new(), at(0, 0), at(0, 0), "Europe/Paris");
        assert!(refused.is_err());
    }

    #[test]
    fn a_timezone_is_checked_against_the_real_database() {
        assert!(QuietHours::new(Vec::new(), at(22, 0), at(8, 0), "Europe/Pris").is_err());
        assert!(QuietHours::new(Vec::new(), at(22, 0), at(8, 0), "Europe/Paris").is_ok());
    }

    #[test]
    fn a_night_window_covers_both_sides_of_midnight() {
        let nights = nights();
        // Paris is UTC+2 in August.
        assert!(
            nights.covers(datetime!(2026-08-04 21:00 UTC)),
            "23:00 local"
        );
        assert!(
            nights.covers(datetime!(2026-08-05 01:00 UTC)),
            "03:00 local"
        );
        assert!(
            !nights.covers(datetime!(2026-08-04 12:00 UTC)),
            "14:00 local"
        );
        assert!(
            !nights.covers(datetime!(2026-08-05 06:30 UTC)),
            "08:30 local"
        );
    }

    #[test]
    fn the_zone_is_the_preferences_and_not_the_instants() {
        // The same moment, read in two zones: quiet in Paris, the middle of the
        // afternoon in Los Angeles. A user who travels keeps their nights.
        let paris = nights();
        let california =
            QuietHours::new(Vec::new(), at(22, 0), at(8, 0), "America/Los_Angeles").unwrap();
        let moment = datetime!(2026-08-04 21:00 UTC);

        assert!(paris.covers(moment));
        assert!(!california.covers(moment));
    }

    #[test]
    fn a_night_window_belongs_to_the_day_it_starts_on() {
        // Friday 22:00 to Saturday 08:00 is what somebody means by "Friday night".
        // 2026-08-07 is a Friday.
        let friday_night =
            QuietHours::new(vec![Day::Fri], at(22, 0), at(8, 0), "Europe/Paris").unwrap();

        assert!(
            friday_night.covers(datetime!(2026-08-07 21:00 UTC)),
            "Friday 23:00 local"
        );
        assert!(
            friday_night.covers(datetime!(2026-08-08 04:00 UTC)),
            "Saturday 06:00 local, which is still Friday night"
        );
        assert!(
            !friday_night.covers(datetime!(2026-08-08 21:00 UTC)),
            "Saturday 23:00 local is Saturday night, which was not asked for"
        );
    }

    #[test]
    fn a_daytime_window_stays_inside_its_day() {
        let lunch = QuietHours::new(vec![Day::Wed], at(12, 0), at(14, 0), "Europe/Paris").unwrap();

        // 2026-08-05 is a Wednesday; 11:00 UTC is 13:00 in Paris.
        assert!(lunch.covers(datetime!(2026-08-05 11:00 UTC)));
        assert!(
            !lunch.covers(datetime!(2026-08-05 13:00 UTC)),
            "15:00 local"
        );
        assert!(!lunch.covers(datetime!(2026-08-06 11:00 UTC)), "a Thursday");
    }

    #[test]
    fn the_window_boundaries_are_half_open() {
        let lunch = QuietHours::new(Vec::new(), at(12, 0), at(14, 0), "UTC").unwrap();

        assert!(lunch.covers(datetime!(2026-08-05 12:00 UTC)), "it starts");
        assert!(!lunch.covers(datetime!(2026-08-05 14:00 UTC)), "it is over");
    }

    #[test]
    fn days_are_normalised_so_two_equal_preferences_compare_equal() {
        let listed = QuietHours::new(
            vec![Day::Fri, Day::Mon, Day::Fri],
            at(22, 0),
            at(8, 0),
            "UTC",
        )
        .unwrap();

        assert_eq!(listed.days, vec![Day::Mon, Day::Fri]);
    }

    // --- The preference as a whole ------------------------------------------

    #[test]
    fn a_preference_nobody_set_mutes_nothing() {
        let off = Dnd::off(UserId::generate());
        assert!(!off.is_active(datetime!(2026-08-05 02:00 UTC)));
        assert!(!off.is_muted(
            &channel("ci-infra"),
            Priority::Urgent,
            datetime!(2026-08-05 02:00 UTC)
        ));
    }

    #[test]
    fn a_quiet_hour_mutes_a_channel_within_it() {
        let night = datetime!(2026-08-05 01:00 UTC);
        assert!(dnd(vec![nights()]).is_muted(&channel("ci-infra"), Priority::Default, night));
    }

    #[test]
    fn the_quick_toggle_mutes_until_its_instant_and_then_stops() {
        let now = datetime!(2026-08-04 12:00 UTC);
        let preference = Dnd {
            until: Some(now + TimeDuration::hours(2)),
            ..Dnd::off(UserId::generate())
        };

        assert!(preference.is_muted(&channel("ci-infra"), Priority::Default, now));
        assert!(!preference.is_muted(
            &channel("ci-infra"),
            Priority::Default,
            now + TimeDuration::hours(3)
        ));
    }

    #[test]
    fn a_channel_outside_the_scope_rings_as_usual() {
        let night = datetime!(2026-08-05 01:00 UTC);
        let preference = Dnd {
            scope: DndScope::Channels {
                channels: vec![channel("ci-infra")],
            },
            ..dnd(vec![nights()])
        };

        assert!(preference.is_muted(&channel("ci-infra"), Priority::Default, night));
        assert!(!preference.is_muted(&channel("cluster-ovh-1"), Priority::Default, night));
    }

    // --- T19 · DND (design §3.4) --------------------------------------------
    //
    // Two of the scenario's four clauses are decisions, and they live here: an
    // excepted channel rings, and `urgent` does not unless this user allowed it.
    // The other two — no OS push while quiet with the inbox still filling, and one
    // digest on the way out — are about delivery, and are covered where delivery
    // happens (M3.3 and M3.4).

    /// The decision §7.4 left open, and the on-call case it exists for.
    #[test]
    fn critical_t19_an_exception_rings_through_everything_including_a_manual_toggle() {
        let now = datetime!(2026-08-05 01:00 UTC);
        let preference = Dnd {
            until: Some(now + TimeDuration::hours(8)),
            exceptions: vec![channel("cluster-ovh-1")],
            ..dnd(vec![nights()])
        };

        assert!(preference.is_muted(&channel("ci-infra"), Priority::Default, now));
        assert!(
            !preference.is_muted(&channel("cluster-ovh-1"), Priority::Default, now),
            "muting everything must not mean undoing the exception first"
        );
    }

    #[test]
    fn critical_t19_urgent_wakes_somebody_only_if_they_said_it_may() {
        let night = datetime!(2026-08-05 01:00 UTC);
        let quiet = dnd(vec![nights()]);
        assert!(
            quiet.is_muted(&channel("ci-infra"), Priority::Urgent, night),
            "the publisher does not get to decide what wakes a person"
        );

        let permissive = Dnd {
            urgent_bypasses: true,
            ..dnd(vec![nights()])
        };
        assert!(!permissive.is_muted(&channel("ci-infra"), Priority::Urgent, night));
        assert!(
            permissive.is_muted(&channel("ci-infra"), Priority::High, night),
            "the bypass is for `urgent`, not for everything loud"
        );
    }

    #[test]
    fn a_preference_round_trips_through_the_shape_8_1_puts_on_the_wire() {
        let preference = Dnd {
            until: Some(datetime!(2026-08-04 12:00 UTC)),
            exceptions: vec![channel("cluster-ovh-1")],
            urgent_bypasses: true,
            ..dnd(vec![nights()])
        };

        let json = serde_json::to_string(&preference).unwrap();
        assert!(json.contains("\"start\":\"22:00\""), "{json}");
        assert!(
            json.contains("\"scope\":{\"kind\":\"everything\"}"),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Dnd>(&json).unwrap(), preference);
    }

    #[test]
    fn a_window_that_came_back_from_storage_is_validated_like_one_that_came_from_a_client() {
        // Deserialisation does not run `QuietHours::new`, so `validated` is what a
        // use case calls on anything it did not build itself.
        let raw = serde_json::json!({
            "user": UserId::generate(),
            "quiet_hours": [{ "start": "22:00", "end": "08:00", "timezone": "Europe/Pris" }]
        });

        let parsed: Dnd = serde_json::from_value(raw).unwrap();
        assert_eq!(
            parsed.scope,
            DndScope::Everything,
            "an absent scope is the whole instance"
        );
        assert!(
            parsed.validated().is_err(),
            "a bad zone must not survive a round trip"
        );
    }
}
