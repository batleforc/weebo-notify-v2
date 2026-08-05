//! Outbound sinks: naming one, and knowing when to stop retrying (design §7.1bis).
//!
//! The transport lives in an adapter and the queue lives in a store; what is here is
//! the part that is neither — **the identity of a sink** and **the schedule its
//! deliveries follow**. Both are decisions the design document already made, and
//! both are the kind that a retry loop otherwise re-invents slightly differently
//! every time somebody touches it.
//!
//! # Why a sink is identified by where it points
//!
//! §11 declares sinks inside a channel's block, with no name of their own:
//!
//! ```toml
//! [[channels.managed.sinks]]
//! type   = "webhook"
//! url    = "https://hooks.weebo.poc/incidents"
//! secret_ref = "env:WNK_SINK_INCIDENTS_HMAC"
//! ```
//!
//! So the identity has to be derived, and the destination is the only part that
//! both identifies it and survives a restart. That gives three properties worth
//! having: a queued delivery still finds its sink after a redeploy, a delivery
//! whose sink left the configuration resolves to nothing and is dropped rather than
//! retried forever, and a dead letter in the console names the endpoint an operator
//! has to go and look at.
//!
//! It also means **a sink pointed at a new URL is a new sink**, which is the honest
//! reading: whatever is queued was signed for, and destined to, somewhere else.
//!
//! What a [`SinkId`] must never contain is a credential. `https://user:pass@host`
//! is a legal URL and would put a password in a database column, in an API
//! response and in a log line at once — refused at the point the configuration is
//! read, which is the only place that can tell the difference.

use core::fmt;
use core::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, sanitize};

/// Longest a sink identity may be. A URL with a path, and no more: past this it is
/// a query string somebody is using as configuration.
pub const MAX_SINK_ID_CHARS: usize = 512;

/// The kinds of sink this build knows how to talk to (design §7.1bis).
///
/// A closed list for the same reason the audit vocabulary is one: a `type` nobody
/// implements has to be refused at startup, and an unknown value read back from a
/// queue row has to be reported rather than guessed at.
pub const SINK_KINDS: &[&str] = &["webhook", "email"];

/// Which sink of which channel, written as `kind:destination`.
///
/// Stable across restarts and reconfigurations, free of secrets, and legible in a
/// dead-letter listing — see the module documentation for why it is derived rather
/// than declared.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "openapi", schema(value_type = String))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SinkId(String);

impl SinkId {
    /// Build one from a kind and where it points.
    ///
    /// The destination is sanitised like every other text field (design §2.3): a
    /// dead letter is rendered in a terminal by the TUI, and an endpoint carrying an
    /// escape sequence would be rendered there too.
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

    /// The kind half — the dimension of `sink_dlq_depth` (design §13) and the way a
    /// dispatcher decides whether a delivery is its business.
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

/// How long to wait before trying again, and when to stop (design §7.1bis).
///
/// # Why the schedule is a value and not a loop
///
/// "Exponential backoff from 30 s to 1 h, give up after 24 h" is one sentence in
/// the design and three easy mistakes in code: a delay that stops growing at the
/// wrong place, a cap applied before the doubling, and a give-up measured from the
/// wrong instant. Written once here, it is also *testable* once — which matters
/// because the loop that consumes it runs on the leader, every few seconds, against
/// somebody else's endpoint.
///
/// # What the clock is measured from
///
/// From the **first failure**, not from the first attempt. A delivery that succeeds
/// immediately never has a deadline, and one that starts failing gets a full day
/// from the moment the trouble began rather than from the moment it was published —
/// which is the difference an operator sees when a consumer goes down hours after a
/// busy publish.
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
    /// The schedule design §7.1bis publishes: 30 s doubling to 1 h, abandoned after
    /// 24 h.
    ///
    /// A constant rather than a configuration key: it is part of the webhook
    /// contract consumers are told about, so an instance that quietly retried for a
    /// week would be lying to them about what "at-least-once" costs.
    pub const CONTRACT: Self = Self {
        first_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(3_600),
        give_up_after: Duration::from_secs(24 * 3_600),
    };

    /// How long to wait after `attempts` failed attempts.
    ///
    /// `attempts` counts what has already been tried, so the first failure asks for
    /// [`Self::first_delay`] and each one after that doubles until the ceiling. Zero
    /// is answered as if it were one: nothing calls it that way, and returning
    /// "immediately" would turn a caller's off-by-one into a busy loop against
    /// somebody else's server.
    #[must_use]
    pub const fn backoff(&self, attempts: u32) -> Duration {
        let doublings = attempts.saturating_sub(1);
        // Shifting past the width of the type is undefined for the CPU and merely
        // wrong for us; 32 doublings of 30 s is already three thousand years.
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

    /// Whether a delivery that has been failing since `first_failed_at` has run out
    /// of day.
    ///
    /// Answered on the *elapsed* time rather than on an attempt count, because the
    /// count depends on how often the drain loop happened to run and the promise
    /// made to consumers is a duration.
    #[must_use]
    pub fn is_exhausted(&self, first_failed_at: OffsetDateTime, now: OffsetDateTime) -> bool {
        let Ok(window) = time::Duration::try_from(self.give_up_after) else {
            // Unreachable for the constant above; failing closed here means a
            // nonsensical policy buries deliveries rather than retrying them forever.
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
        // Not "accepted and skipped later": a `type = "matrix"` in the configuration
        // is an operator expecting deliveries that will never happen.
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
