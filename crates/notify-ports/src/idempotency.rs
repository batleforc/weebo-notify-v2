//! Replaying a request instead of repeating it (design §8, scenario T16).
//!
//! `PUT /channels/{id}/notifications/{external_id}` is idempotent by construction —
//! the external id *is* the key. `POST /notifications` has no such handle, so it
//! accepts an `Idempotency-Key` header and the same response is replayed for 24
//! hours.
//!
//! The reservation is two-phase on purpose. A single "store the response when
//! done" would leave a window where a retry arriving mid-flight starts a second
//! publish, which is exactly the case retries are made of: a client that timed out
//! and tried again. Claiming the key *first* turns that race into a wait.

use time::OffsetDateTime;

use crate::error::PortResult;

/// What a caller learns when it claims a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The key is ours: go do the work, then call
    /// [`IdempotencyStore::complete`].
    Fresh,
    /// The work was already done. Replay this response verbatim.
    Replayed {
        /// The stored response body.
        response: String,
    },
    /// Somebody else claimed it and has not finished.
    ///
    /// Answered with `409` and a `Retry-After`, not by waiting: holding a request
    /// open on the chance that another one finishes is how a thread pool is spent.
    InFlight,
}

/// Reservations for `Idempotency-Key`.
#[async_trait::async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// Claim a key for a principal.
    ///
    /// Scoped by `principal_ref` (a [`notify_domain::Principal::reference`]) so two
    /// publishers picking the same key — `deploy-1` is not an imaginative choice —
    /// cannot read each other's responses.
    async fn claim(&self, principal_ref: &str, key: &str, now: OffsetDateTime)
    -> PortResult<Claim>;

    /// Store the response for a claimed key, so retries within the window replay it.
    ///
    /// `claimed_at` is the `now` the claim was made with, and it is what proves the
    /// claim is still this caller's: a publish slow enough that its claim lapsed and
    /// was taken over must not overwrite the response of the request that took it.
    /// A claim that is no longer ours is left alone, silently.
    async fn complete(
        &self,
        principal_ref: &str,
        key: &str,
        claimed_at: OffsetDateTime,
        response: &str,
        expires_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Release a claim whose work failed, so the client's retry is not answered
    /// with a permanent [`Claim::InFlight`] for the rest of the window. Only the
    /// claim made at `claimed_at`, for the same reason as [`Self::complete`].
    async fn release(
        &self,
        principal_ref: &str,
        key: &str,
        claimed_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Drop expired reservations. Leader-only (design §11).
    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64>;
}
