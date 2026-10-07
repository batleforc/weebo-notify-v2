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

/// What a connection tells the lease table about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRequest {
    /// Whose connection (`Principal::reference`).
    pub principal: String,
    /// Which replica holds it.
    pub replica: String,
    /// `sse` or `websocket`.
    pub transport: String,
    /// The client's own name for itself (its `User-Agent`), truncated.
    pub client: String,
    /// How it authenticated: `session`, `key`, `device:<id>`, `ci`.
    pub credential: String,
    /// A hash of the presented credential, never the credential itself.
    pub fingerprint: String,
}

/// One live connection, as its owner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The lease id.
    pub id: String,
    /// `sse` or `websocket`.
    pub transport: String,
    /// The client's `User-Agent`.
    pub client: String,
    /// How it authenticated.
    pub credential: String,
    /// Hash of its credential, so a caller can recognise its own connection.
    pub fingerprint: String,
    /// When it connected.
    pub opened_at: OffsetDateTime,
}

/// The answer to a connection asking for a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// Granted, under this id.
    Granted(String),
    /// The principal already holds `max` (§8.2): `429`.
    Full,
    /// Its owner disconnected this credential from realtime: `403`, don't retry.
    Blocked,
}

/// Live stream connections counted across every replica (§8.2). A lease its replica
/// stops renewing expires, so a killed replica cannot lock anyone out for long.
#[async_trait::async_trait]
pub trait StreamLeases: Send + Sync {
    /// Take one of `max` leases for the request's principal.
    async fn acquire(&self, request: &LeaseRequest, max: usize) -> PortResult<Acquired>;
    /// Give one back.
    async fn release(&self, lease: &str) -> PortResult<()>;
    /// Extend every lease `replica` holds, drop what expired anywhere, and return the
    /// ids `replica` still holds — any other was disconnected and must close.
    async fn renew(&self, replica: &str) -> PortResult<Vec<String>>;
    /// The principal's live connections, oldest first.
    async fn list(&self, principal: &str) -> PortResult<Vec<Lease>>;
    /// Drop one of the principal's leases and keep its credential off realtime until
    /// `until`; `false` when it is not theirs or already gone.
    async fn disconnect(
        &self,
        principal: &str,
        lease: &str,
        until: OffsetDateTime,
    ) -> PortResult<bool>;
}
