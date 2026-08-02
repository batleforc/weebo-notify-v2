//! Interactions: acknowledgements, replies and picks (design §2.3).

use notify_domain::{Cursor, InteractionRecord, NotificationId, Recorded, UserId};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Silences, which are per-user delivery state rather than interactions, but live
/// alongside them because they are set from the same buttons (design §2.3).
use notify_domain::Silence;

/// Interactions and the per-user silence state.
#[async_trait::async_trait]
pub trait InteractionRepository: Send + Sync {
    /// Apply the domain's decision about an interaction.
    ///
    /// The uniqueness rules — `ack` idempotent per (user, key), `choice` exclusive
    /// and last-write-wins — are decided by the domain and enforced *again* here by
    /// a unique index. That duplication is deliberate: the domain reasons about a
    /// snapshot, and two concurrent acknowledgements are a race it cannot see.
    ///
    /// Returns the resulting cursor when something was actually written;
    /// [`Recorded::Unchanged`] writes nothing and broadcasts nothing.
    async fn apply(&self, outcome: &Recorded) -> PortResult<Option<Cursor>>;

    /// Every interaction on a notification (design §8, `GET .../interactions`).
    ///
    /// This is the endpoint a waiting service polls for its `approve`, so it reads
    /// the whole set rather than a page: an approval that fell off page two would
    /// be a very confusing bug.
    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<InteractionRecord>>;

    /// Start a silence for a user (design §2.3).
    ///
    /// Replaces any existing silence on the same target: re-silencing a group
    /// extends it rather than stacking a second one nobody can see to lift.
    async fn silence(&self, silence: &Silence) -> PortResult<Cursor>;

    /// Every silence still in force for a user, which is what a `preference` event
    /// carries (design §8.1).
    async fn active_silences(&self, user: UserId, now: OffsetDateTime) -> PortResult<Vec<Silence>>;
}
