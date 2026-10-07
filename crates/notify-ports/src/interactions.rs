//! Interactions: acknowledgements, replies and picks (design §2.3).

use notify_domain::{Cursor, InteractionRecord, NotificationId, Recorded, UserId};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Silences live here because they are set from the same buttons (§2.3).
use notify_domain::Silence;

/// Interactions and the per-user silence state.
#[async_trait::async_trait]
pub trait InteractionRepository: Send + Sync {
    /// Apply the domain's decision about an interaction. Uniqueness is re-enforced by
    /// a unique index because concurrent acks race past the domain's snapshot.
    /// Returns the cursor when something was written.
    async fn apply(&self, outcome: &Recorded) -> PortResult<Option<Cursor>>;

    /// Every interaction on a notification, unpaged so a polled approval is never missed (§8).
    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<InteractionRecord>>;

    /// Start a silence for a user, replacing any existing one on the same target (§2.3).
    async fn silence(&self, silence: &Silence) -> PortResult<Cursor>;

    /// Every silence still in force for a user (§8.1).
    async fn active_silences(&self, user: UserId, now: OffsetDateTime) -> PortResult<Vec<Silence>>;
}
