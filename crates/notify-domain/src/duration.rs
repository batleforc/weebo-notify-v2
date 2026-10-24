//! The single-unit duration spelling (`"7d"`, `"4h"`, `"30s"`) shared by payload
//! TTLs, silences and GitOps config (§7.2bis, §2.3, §11).

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

/// A whole-second, single-unit duration: `"90m"` is fine, `"1h30m"` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(
    feature = "openapi",
    schema(value_type = String, pattern = r"^\d+[smhdw]$", example = "48h")
)]
pub struct HumanDuration(Duration);

impl HumanDuration {
    /// Zero; as a TTL it means "never expire" (see [`crate::ttl::Ttl`]).
    pub const ZERO: Self = Self(Duration::ZERO);

    /// Build from seconds in a `const` context; runtime input goes through
    /// [`Self::from_secs`], which checks.
    #[must_use]
    pub const fn from_secs_const(seconds: u64) -> Self {
        Self(Duration::from_secs(seconds))
    }

    /// Parse `<integer><unit>`, with unit in `s`, `m`, `h`, `d`, `w`; a bare integer is seconds.
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

    /// Build from whole seconds, refusing anything past [`MAX_DURATION`].
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
    /// Render in the largest unit that divides evenly.
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
            if seconds.is_multiple_of(unit_seconds) {
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
        /// Accepts `"7d"` and `604800` alike (TOML integers, JSON strings).
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
