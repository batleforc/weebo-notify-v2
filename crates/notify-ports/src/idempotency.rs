//! `Idempotency-Key` replay for `POST /notifications` (§8, T16). The key is
//! claimed first so a mid-flight retry waits instead of publishing twice.

use time::OffsetDateTime;

use crate::error::PortResult;

/// What a caller learns when it claims a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The key is ours: do the work, then call [`IdempotencyStore::complete`].
    Fresh,
    /// The work was already done. Replay this response verbatim.
    Replayed {
        /// The stored response body.
        response: String,
    },
    /// Somebody else claimed it and has not finished; answered with `409`.
    InFlight,
}

/// Reservations for `Idempotency-Key`.
#[async_trait::async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// Claim a key, scoped by `principal_ref` so publishers cannot read each other's responses.
    async fn claim(&self, principal_ref: &str, key: &str, now: OffsetDateTime)
    -> PortResult<Claim>;

    /// Store the response for a claimed key. `claimed_at` proves the claim is still
    /// ours; a lapsed, taken-over claim is silently left alone.
    async fn complete(
        &self,
        principal_ref: &str,
        key: &str,
        claimed_at: OffsetDateTime,
        response: &str,
        expires_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Release a claim whose work failed, so retries are not stuck on
    /// [`Claim::InFlight`]. Only the claim made at `claimed_at`.
    async fn release(
        &self,
        principal_ref: &str,
        key: &str,
        claimed_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Drop expired reservations. Leader-only (design §11).
    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64>;
}
