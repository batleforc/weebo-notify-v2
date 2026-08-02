//! The duration spelling used everywhere a human writes a delay: `"7d"`, `"4h"`,
//! `"30s"` — in a publish payload (`ttl`), in a silence action (`duration`) and in
//! the GitOps channel config (design §7.2bis, §2.3, §11).
//!
//! One parser, one formatter, one vocabulary. Anything a publisher can write, an
//! operator can write in TOML and a client can read back unchanged.

use core::fmt;
use core::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{DomainError, Result};
use crate::limits::MAX_DURATION;

const SECOND: u64 = 1;
const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

/// A duration written the way people write them.
///
/// Deliberately whole-second and single-unit: `"90m"` is fine, `"1h30m"` is not.
/// A notification TTL is a rough intent, not a schedule, and one unit per value
/// keeps the config, the payload and the client display identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HumanDuration(Duration);

impl HumanDuration {
    /// Zero. Where a TTL is expected this means "never expire", which is a policy
    /// decision rather than a duration — see [`crate::ttl::Ttl`].
    pub const ZERO: Self = Self(Duration::ZERO);

    /// Build from seconds in a `const` context, for literals that are obviously
    /// within [`MAX_DURATION`]. Runtime input goes through [`Self::from_secs`],
    /// which checks.
    #[must_use]
    pub const fn from_secs_const(seconds: u64) -> Self {
        Self(Duration::from_secs(seconds))
    }

    /// Parse `<integer><unit>`, with unit in `s`, `m`, `h`, `d`, `w`.
    ///
    /// A bare integer is read as seconds, so a config that says `30` and one that
    /// says `"30s"` agree.
    pub fn parse(raw: &str, field: &'static str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(DomainError::Empty { field });
        }

        let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
        let (digits, unit) = raw.split_at(split);

        let malformed = || DomainError::Malformed {
            field,
            reason: "expected a whole number followed by s, m, h, d or w — e.g. `7d`",
        };

        let amount: u64 = digits.parse().map_err(|_| malformed())?;
        let multiplier = match unit {
            "" | "s" => SECOND,
            "m" => MINUTE,
            "h" => HOUR,
            "d" => DAY,
            "w" => WEEK,
            _ => return Err(malformed()),
        };

        let seconds = amount.checked_mul(multiplier).ok_or(DomainError::TooLong {
            field,
            max: usize::try_from(MAX_DURATION.as_secs()).unwrap_or(usize::MAX),
            actual: usize::MAX,
        })?;
        Self::from_secs(seconds, field)
    }

    /// Build from a whole number of seconds, refusing anything past the sanity
    /// ceiling (design guard rail, [`MAX_DURATION`]).
    pub fn from_secs(seconds: u64, field: &'static str) -> Result<Self> {
        if seconds > MAX_DURATION.as_secs() {
            return Err(DomainError::TooLong {
                field,
                max: usize::try_from(MAX_DURATION.as_secs()).unwrap_or(usize::MAX),
                actual: usize::try_from(seconds).unwrap_or(usize::MAX),
            });
        }
        Ok(Self(Duration::from_secs(seconds)))
    }

    /// The underlying duration.
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Whole seconds.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    /// Whether this is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl fmt::Display for HumanDuration {
    /// Render in the largest unit that divides evenly, so a value parsed from
    /// `"7d"` comes back as `"7d"` and not as `"604800s"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        if seconds == 0 {
            return f.write_str("0s");
        }
        for (unit_seconds, suffix) in [
            (WEEK, 'w'),
            (DAY, 'd'),
            (HOUR, 'h'),
            (MINUTE, 'm'),
            (SECOND, 's'),
        ] {
            if seconds % unit_seconds == 0 {
                return write!(f, "{}{suffix}", seconds / unit_seconds);
            }
        }
        write!(f, "{seconds}s")
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        /// `"7d"` and `604800` are both accepted, because TOML config reaches for
        /// integers and JSON payloads reach for strings.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Seconds(u64),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Text(raw) => Self::parse(&raw, "duration"),
            Repr::Seconds(seconds) => Self::from_secs(seconds, "duration"),
        }
        .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn parses_every_unit() {
        for (raw, seconds) in [
            ("30s", 30),
            ("30", 30),
            ("5m", 300),
            ("4h", 14_400),
            ("7d", 604_800),
            ("2w", 1_209_600),
            ("0", 0),
        ] {
            assert_eq!(HumanDuration::parse(raw, "ttl").unwrap().as_secs(), seconds);
        }
    }

    #[test]
    fn round_trips_through_its_largest_unit() {
        for raw in ["0s", "30s", "5m", "4h", "6d", "2w"] {
            let parsed = HumanDuration::parse(raw, "ttl").unwrap();
            assert_eq!(parsed.to_string(), raw);
        }
        // Equivalent spellings collapse onto the largest unit that divides evenly,
        // so the canonical form is what a client reads back.
        assert_eq!(HumanDuration::parse("7d", "ttl").unwrap().to_string(), "1w");
        assert_eq!(
            HumanDuration::parse("60m", "ttl").unwrap().to_string(),
            "1h"
        );
        // 90 minutes has no whole-hour spelling and stays in minutes.
        assert_eq!(
            HumanDuration::parse("90m", "ttl").unwrap().to_string(),
            "90m"
        );
    }

    #[test]
    fn refuses_nonsense_and_overflow() {
        for bad in ["", "d", "-1h", "1.5h", "7 d", "7y", "1h30m"] {
            assert!(HumanDuration::parse(bad, "ttl").is_err(), "`{bad}` parsed");
        }
        // A unit mistake, not an intent.
        assert!(HumanDuration::parse("9999w", "ttl").is_err());
        assert!(HumanDuration::parse("99999999999999999999w", "ttl").is_err());
    }

    #[test]
    fn deserialises_from_both_text_and_seconds() {
        assert_eq!(
            serde_json::from_str::<HumanDuration>("\"4h\"")
                .unwrap()
                .as_secs(),
            14_400
        );
        assert_eq!(
            serde_json::from_str::<HumanDuration>("14400")
                .unwrap()
                .as_secs(),
            14_400
        );
    }
}
