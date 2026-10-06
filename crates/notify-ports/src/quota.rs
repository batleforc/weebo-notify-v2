//! Rate limits and quotas (§8.2), per subject and never global (T14). State is
//! behind a port so limits hold across replicas.

use time::OffsetDateTime;

use crate::error::PortResult;

/// What is being limited; every variant is documented in §8.2.
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
        /// How many are left in the current window (`X-RateLimit-Remaining`).
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
    /// Count one request and say whether it is allowed, consuming on the way in.
    /// `configured` is the per-channel override (§8.2), `None` for the default; it is
    /// not part of [`Quota`] because `Quota` is the counter key.
    async fn check(
        &self,
        quota: &Quota,
        configured: Option<u32>,
        now: OffsetDateTime,
    ) -> PortResult<Allowance>;
}
