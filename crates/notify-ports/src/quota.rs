//! Rate limits and quotas (design §8.2).
//!
//! Limits are per *subject* — an API key, a channel, an `(issuer, subject)` pair for
//! token exchange — and never global. A CI job in a retry loop must degrade its own
//! publishing and nobody else's; a shared limit would let one noisy publisher take a
//! channel down for everybody, which is a worse failure than the one it prevents
//! (scenario T14).
//!
//! The state lives behind a port because it is genuinely shared: with several
//! replicas, an in-process counter enforces `n × 60/min` rather than `60/min`.

use time::OffsetDateTime;

use crate::error::PortResult;

/// What is being limited.
///
/// A closed set rather than a free-form string: every limit here appears in
/// design §8.2, and one that does not is a limit nobody documented.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Quota {
    /// Publishes by one API key, across every channel: 60/min, burst 120.
    PublishPerKey {
        /// The key's textual reference.
        key: String,
    },
    /// Publishes into one channel, whoever is publishing: 600/min. Protects the
    /// fan-out rather than the publisher.
    PublishPerChannel {
        /// The channel.
        channel: String,
    },
    /// Token exchanges for one `(issuer, subject)`: 10/min (design §5.3).
    TokenExchange {
        /// Issuer and subject, joined.
        subject: String,
    },
    /// Searches by one principal: 30/min. Full-text is the expensive read.
    Search {
        /// The principal's reference.
        principal: String,
    },
}

/// The answer to "may this one through?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowance {
    /// Yes.
    Allowed {
        /// How many are left in the current window, for a `X-RateLimit-Remaining`
        /// header.
        remaining: u32,
    },
    /// No — `429`, with this many seconds in `Retry-After`.
    Limited {
        /// When to come back.
        retry_after_seconds: u32,
    },
}

impl Allowance {
    /// Whether the request may proceed.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

/// Counting requests against a quota.
#[async_trait::async_trait]
pub trait RateLimiter: Send + Sync {
    /// Count one request and say whether it is allowed.
    ///
    /// Consumes on the way *in*, so a caller that is over the limit is refused
    /// before doing the work rather than after.
    ///
    /// `configured` is the override design §8.2 promises — "défauts globaux,
    /// surchargeables par canal (`limits` dans `[[channels.managed]]`, §11)" —
    /// and `None` means the published default applies. It is a **separate
    /// argument rather than a field of [`Quota`]** because the two answer
    /// different questions: `Quota` says *who is being counted*, and it is the
    /// map key, the row key and the thing compared for equality. Folding a
    /// ceiling into it would make two requests against the same channel look like
    /// different subjects the moment an operator edited the number.
    async fn check(
        &self,
        quota: &Quota,
        configured: Option<u32>,
        now: OffsetDateTime,
    ) -> PortResult<Allowance>;
}
