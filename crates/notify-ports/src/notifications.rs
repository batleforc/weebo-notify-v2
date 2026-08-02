//! Storing and reading notifications.
//!
//! Two conventions run through this port and are worth stating once rather than on
//! every method:
//!
//! **A write returns the cursor it produced.** Recording a revision and appending
//! its event row are one transaction (ADR-0012), so the write is the only place
//! that knows the resulting position. Handing it back is what lets a publisher be
//! told where its notification landed, and what lets the fan-out poke carry a
//! cursor instead of a payload.
//!
//! **Reads never resurrect what is gone.** Deleted and expired notifications are
//! excluded from every list, every search and every lookup by external id. Only
//! [`NotificationRepository::get`] can still see a tombstoned row, because a caller
//! sometimes has to tell "never existed" from "no longer exists" — a distinction a
//! 404 loses.

use notify_domain::{
    ChannelId, Cursor, ExternalId, Notification, NotificationId, Priority, Revision, State, Status,
};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// Filters for an inbox listing (design §8, `GET /channels/{id}/notifications`).
///
/// Every field is a narrowing: empty vectors and `None` mean "do not filter on
/// this", never "match nothing".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxQuery {
    /// Which channels to read. Must already be narrowed to what the caller may
    /// see — this port does not do authorisation, and an empty list means
    /// "no channels", which correctly returns nothing.
    pub channels: Vec<ChannelId>,
    /// Keep only these lifecycle states.
    pub states: Vec<State>,
    /// Keep only these statuses.
    pub statuses: Vec<Status>,
    /// Keep only notifications carrying all of these tags.
    pub tags: Vec<String>,
    /// Keep only notifications at or above this priority.
    pub min_priority: Option<Priority>,
    /// Keep only notifications sharing this grouping key.
    pub group_key: Option<String>,
    /// Where and how much.
    pub page: PageRequest,
}

/// A full-text query (design §8, `GET /search`).
///
/// Scoped by construction: `channels` is the caller's own membership list, so a
/// search cannot return a notification from a channel they are not in — the
/// property scenario T27 exists to pin down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// What to look for, across title, body and tags.
    pub text: String,
    /// The channels the caller may read.
    pub channels: Vec<ChannelId>,
    /// Where and how much.
    pub page: PageRequest,
}

/// A notification that is due for purge (design §7.2bis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredNotification {
    /// Which one.
    pub id: NotificationId,
    /// Where it lived — the purge broadcasts a tombstone to that channel, and
    /// counts towards its "N expired while you were away".
    pub channel: ChannelId,
    /// Whether anybody ever received it. An expired-undelivered notification is
    /// the one that feeds the counter rather than the backlog (design §7.2bis).
    pub was_delivered: bool,
}

/// Notifications and their revisions.
#[async_trait::async_trait]
pub trait NotificationRepository: Send + Sync {
    /// Fetch one, including a deleted or expired one.
    ///
    /// The only read that sees tombstoned rows, so a caller can answer "gone" with
    /// [`notify_domain::DomainError::NotificationGone`] rather than a bare 404.
    async fn get(&self, id: NotificationId) -> PortResult<Option<Notification>>;

    /// Find the live notification a publisher's own identifier points at
    /// (design §8, `PUT .../{external_id}`).
    ///
    /// Excludes deleted and expired rows on purpose: re-running a job whose
    /// previous notification has aged out creates a new one rather than resurrecting
    /// a tombstone.
    async fn find_by_external_id(
        &self,
        channel: &ChannelId,
        external_id: &ExternalId,
    ) -> PortResult<Option<Notification>>;

    /// Store a new notification and its first revision, appending the event row in
    /// the same transaction.
    ///
    /// Returns [`crate::error::PortError::Conflict`] when a concurrent publisher won
    /// the race on the same `external_id`; the caller re-reads and revises instead,
    /// which is what makes the upsert idempotent under retries (scenario T16).
    async fn insert(&self, notification: &Notification, revision: &Revision) -> PortResult<Cursor>;

    /// Append a revision to an existing notification and update its head.
    ///
    /// The domain has already assigned the `seq` and applied last-write-wins; the
    /// adapter persists that decision rather than re-deciding it.
    async fn append_revision(
        &self,
        notification: &Notification,
        revision: &Revision,
    ) -> PortResult<Cursor>;

    /// List an inbox.
    async fn list(&self, query: &InboxQuery) -> PortResult<Page<Notification>>;

    /// The revision history of one notification, oldest first (design §8).
    async fn revisions(&self, id: NotificationId, page: PageRequest) -> PortResult<Page<Revision>>;

    /// Full-text search within the caller's channels.
    async fn search(&self, query: &SearchQuery) -> PortResult<Page<Notification>>;

    /// Mark a notification deleted and append its tombstone.
    ///
    /// Idempotent: deleting an already-deleted notification returns `None` rather
    /// than failing or emitting a second tombstone.
    async fn mark_deleted(
        &self,
        id: NotificationId,
        at: OffsetDateTime,
    ) -> PortResult<Option<Cursor>>;

    /// Claim a batch of notifications whose retention has run out.
    ///
    /// Batched and leader-only (design §11): the purge job calls this in a loop
    /// until it comes back short. Returning the batch rather than deleting it
    /// outright lets the caller emit tombstones and update the expired counters
    /// before the rows go.
    async fn claim_expired(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<ExpiredNotification>>;

    /// Hard-delete previously soft-deleted rows, once their tombstones have had
    /// time to reach every client (design §7.2bis).
    async fn purge(&self, deleted_before: OffsetDateTime, limit: u32) -> PortResult<u64>;
}
