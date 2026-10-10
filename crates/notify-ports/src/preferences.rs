//! Do-not-disturb, stored per user (§7.4). Writes append a `preference` event
//! carrying both the DND and active silences (§8.1, ADR-0012).

use notify_domain::{Cursor, Dnd, NotificationId, Status, UserId};
use time::OffsetDateTime;

use crate::error::PortResult;

/// What one user has had held back, plus the preference that decides when the
/// digest goes out ([`Dnd::is_active`] stays a domain question).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredTally {
    /// Whose, and the schedule that decides when the digest goes out.
    pub dnd: Dnd,
    /// How many pushes were held back.
    pub total: u32,
    /// How many of those closed on a failure (§7.4).
    pub failures: u32,
}

/// The per-user delivery preference.
#[async_trait::async_trait]
pub trait PreferenceRepository: Send + Sync {
    /// What this user has set; [`Dnd::off`] when they never set anything.
    async fn dnd(&self, user: UserId) -> PortResult<Dnd>;

    /// Replace this user's do-not-disturb and announce it, stamped at `now`.
    /// Clears the held-back tally.
    async fn set_dnd(&self, dnd: &Dnd, now: OffsetDateTime) -> PortResult<Cursor>;

    /// Count one more push held back for this user (§7.4). A no-op for users with
    /// no stored preference. Idempotent per `(user, notification, seq)`: the fan-out
    /// is at least once and per device, so a revision must not be counted twice.
    async fn defer(
        &self,
        user: UserId,
        notification: NotificationId,
        seq: u32,
        status: Status,
    ) -> PortResult<()>;

    /// Everyone with something held back, quiet period over or not, ordered by user
    /// and paged by `after` so callers can walk past users who are still quiet.
    async fn deferred_tallies(
        &self,
        after: Option<UserId>,
        limit: u32,
    ) -> PortResult<Vec<DeferredTally>>;

    /// Subtract a delivered digest from a user's tally (not zero it: pushes held
    /// back meanwhile still count next time).
    async fn clear_deferred(&self, user: UserId, total: u32, failures: u32) -> PortResult<()>;
}
