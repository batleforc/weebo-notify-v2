//! Outbound sinks (§7.1bis): a sink's identity and its retry schedule. A sink is
//! identified by `kind:destination` (config gives no name), so a new URL is a new
//! sink and removed sinks resolve to nothing. Credentials in URLs are refused.

use core::fmt;
use core::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, sanitize};

/// Longest a sink identity may be.
pub const MAX_SINK_ID_CHARS: usize = 512;

/// The kinds of sink this build knows how to talk to (§7.1bis); unknown kinds are refused.
pub const SINK_KINDS: &[&str] = &["webhook", "email"];

/// Which sink of which channel, written as `kind:destination` (never contains secrets).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "openapi", schema(value_type = String))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SinkId(String);

impl SinkId {
    /// Build one from a kind and where it points; the destination is sanitised (§2.3).
    pub fn new(kind: &str, destination: &str) -> Result<Self> {
        let kind = sanitize(kind, TextKind::SingleLine).to_ascii_lowercase();
        if !SINK_KINDS.contains(&kind.as_str()) {
            return Err(DomainError::Malformed {
                field: "sink.type",
                reason: "unknown sink kind; this build serves `webhook` and `email`",
            });
        }

        let destination = sanitize(destination, TextKind::SingleLine);
        if destination.is_empty() {
            return Err(DomainError::Empty {
                field: "sink.destination",
            });
        }

        let value = format!("{kind}:{destination}");
        let len = value.chars().count();
        if len > MAX_SINK_ID_CHARS {
            return Err(DomainError::TooLong {
                field: "sink.destination",
                max: MAX_SINK_ID_CHARS,
                actual: len,
            });
        }

        Ok(Self(value))
    }

    /// Read one back — from a queue row, or from an admin requeueing a dead letter.
    pub fn parse(raw: &str) -> Result<Self> {
        let (kind, destination) = raw.split_once(':').ok_or(DomainError::Malformed {
            field: "sink.id",
            reason: "expected `kind:destination`",
        })?;
        Self::new(kind, destination)
    }

    /// The kind half, used as a metric label and for dispatcher routing.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.0.split_once(':').map_or("", |(kind, _)| kind)
    }

    /// Where it points.
    #[must_use]
    pub fn destination(&self) -> &str {
        self.0
            .split_once(':')
            .map_or("", |(_, destination)| destination)
    }

    /// The whole thing, as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SinkId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The retry schedule (§7.1bis): exponential backoff, giving up a fixed time
/// after the **first failure** (not the first attempt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// How long after the first failure to try again.
    first_delay: Duration,
    /// The ceiling the doubling stops at.
    max_delay: Duration,
    /// How long a delivery may keep failing before it becomes a dead letter.
    give_up_after: Duration,
}

impl RetryPolicy {
    /// The published contract (§7.1bis): 30 s doubling to 1 h, abandoned after 24 h.
    pub const CONTRACT: Self = Self {
        first_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(3_600),
        give_up_after: Duration::from_secs(24 * 3_600),
    };

    /// How long to wait after `attempts` failed attempts, doubling from
    /// [`Self::first_delay`] up to the ceiling. Zero is treated as one.
    #[must_use]
    pub const fn backoff(&self, attempts: u32) -> Duration {
        let doublings = attempts.saturating_sub(1);
        // Avoid shifting past the type width; 32 doublings is already absurd.
        let factor = if doublings >= 32 {
            u64::MAX
        } else {
            1_u64 << doublings
        };

        let seconds = self.first_delay.as_secs().saturating_mul(factor);
        if seconds >= self.max_delay.as_secs() {
            self.max_delay
        } else {
            Duration::from_secs(seconds)
        }
    }

    /// Whether a delivery failing since `first_failed_at` has exceeded the window,
    /// judged on elapsed time rather than attempt count.
    #[must_use]
    pub fn is_exhausted(&self, first_failed_at: OffsetDateTime, now: OffsetDateTime) -> bool {
        let Ok(window) = time::Duration::try_from(self.give_up_after) else {
            // Fail closed: a nonsensical policy buries rather than retries forever.
            return true;
        };
        now - first_failed_at >= window
    }

    /// The window an operator is told about, for the console and the docs.
    #[must_use]
    pub const fn give_up_after(&self) -> Duration {
        self.give_up_after
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::CONTRACT
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn a_sink_is_named_by_where_it_points() {
        let id = SinkId::new("webhook", "https://hooks.weebo.poc/incidents").unwrap();

        assert_eq!(id.kind(), "webhook");
        assert_eq!(id.destination(), "https://hooks.weebo.poc/incidents");
        assert_eq!(SinkId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn a_kind_this_build_does_not_serve_is_refused() {
        // Unknown kinds are refused, not silently skipped.
        assert!(SinkId::new("matrix", "!room:weebo.poc").is_err());
    }

    #[test]
    fn a_destination_carrying_an_escape_sequence_is_stripped_not_stored() {
        // A dead letter is rendered in a terminal by the TUI (design §2.3, T5).
        let id = SinkId::new("webhook", "https://hooks\u{1b}[2J.weebo.poc/x").unwrap();
        assert!(!id.as_str().contains('\u{1b}'));
    }

    #[test]
    fn an_empty_destination_is_not_a_sink() {
        assert!(SinkId::new("webhook", "   ").is_err());
        assert!(SinkId::parse("webhook").is_err());
    }

    #[test]
    fn the_backoff_doubles_from_thirty_seconds_and_stops_at_an_hour() {
        let policy = RetryPolicy::CONTRACT;

        assert_eq!(policy.backoff(1), Duration::from_secs(30));
        assert_eq!(policy.backoff(2), Duration::from_secs(60));
        assert_eq!(policy.backoff(3), Duration::from_secs(120));
        assert_eq!(policy.backoff(8), Duration::from_secs(3_600));
        assert_eq!(
            policy.backoff(9_000),
            Duration::from_secs(3_600),
            "the ceiling holds however many attempts have gone by"
        );
    }

    #[test]
    fn a_zero_attempt_still_waits() {
        assert_eq!(
            RetryPolicy::CONTRACT.backoff(0),
            Duration::from_secs(30),
            "an off-by-one must not become a busy loop against somebody else's server"
        );
    }

    #[test]
    fn a_day_of_failing_is_the_end_of_it() {
        let policy = RetryPolicy::CONTRACT;
        let started = OffsetDateTime::UNIX_EPOCH;

        assert!(!policy.is_exhausted(started, started + time::Duration::hours(23)));
        assert!(policy.is_exhausted(started, started + time::Duration::hours(24)));
    }
}
