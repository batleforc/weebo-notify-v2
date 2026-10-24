//! Storing and reading notifications. Writes return their cursor (ADR-0012); reads
//! exclude deleted/expired rows, except [`NotificationRepository::get`].

use notify_domain::{
    AttachmentId, ChannelId, Cursor, ExternalId, Notification, NotificationId, Principal, Priority,
    Revision, State, Status,
};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::outbox::AfterWrite;
use crate::pagination::{Page, PageRequest};

/// What one channel's unread badge shows (design §7.3, §10.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnreadCount {
    /// The channel the badge hangs off.
    pub channel: ChannelId,
    /// How many of its notifications this principal has not read; never zero (such
    /// channels are omitted).
    pub unread: u64,
}

/// Filters for an inbox listing (§8); empty/`None` means "do not filter".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxQuery {
    /// Which channels to read, already narrowed to what the caller may see (this
    /// port does not authorise).
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
    /// Keep only notifications whose head revision has action buttons (`Some(false)`:
    /// none) (§2.3).
    pub has_actions: Option<bool>,
    /// Where and how much.
    pub page: PageRequest,
}

/// A full-text query (§8), scoped to the caller's own channels (T27).
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
    /// Where it lived; gets a tombstone and an "expired while away" count.
    pub channel: ChannelId,
    /// Whether anybody ever received it; undelivered ones feed the counter (§7.2bis).
    pub was_delivered: bool,
}

/// A notification the server closed for want of activity (design §7.2bis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedNotification {
    /// Which one.
    pub id: NotificationId,
    /// Where it lived; the closure event goes to that channel.
    pub channel: ChannelId,
}

/// Notifications and their revisions.
#[async_trait::async_trait]
pub trait NotificationRepository: Send + Sync {
    /// Fetch one, deleted or expired included, so callers can answer
    /// [`notify_domain::DomainError::NotificationGone`].
    async fn get(&self, id: NotificationId) -> PortResult<Option<Notification>>;

    /// Find the live notification behind a publisher's `external_id` (§8); tombstones
    /// are never resurrected.
    async fn find_by_external_id(
        &self,
        channel: &ChannelId,
        external_id: &ExternalId,
    ) -> PortResult<Option<Notification>>;

    /// Store a new notification and its first revision with its event row.
    /// [`crate::error::PortError::Conflict`] if a concurrent publisher won the
    /// `external_id` race; the caller re-reads and revises (T16).
    async fn insert(&self, notification: &Notification, revision: &Revision) -> PortResult<Cursor> {
        self.insert_with(notification, revision, &AfterWrite::default())
            .await
    }

    /// [`Self::insert`], committing `after` in the same transaction (ADR-0025).
    async fn insert_with(
        &self,
        notification: &Notification,
        revision: &Revision,
        after: &AfterWrite,
    ) -> PortResult<Cursor>;

    /// Append a revision and update the head, persisting the domain's `seq` decision.
    async fn append_revision(
        &self,
        notification: &Notification,
        revision: &Revision,
    ) -> PortResult<Cursor> {
        self.append_revision_with(notification, revision, &AfterWrite::default())
            .await
    }

    /// [`Self::append_revision`], committing `after` in the same transaction
    /// (ADR-0025).
    async fn append_revision_with(
        &self,
        notification: &Notification,
        revision: &Revision,
        after: &AfterWrite,
    ) -> PortResult<Cursor>;

    /// List an inbox.
    async fn list(&self, query: &InboxQuery) -> PortResult<Page<Notification>>;

    /// Unread notifications per channel for this principal (§7.3), counted server-side
    /// over the whole channel. Same visibility as `list`; zero channels are omitted.
    async fn unread_counts(
        &self,
        principal: &Principal,
        channels: &[ChannelId],
    ) -> PortResult<Vec<UnreadCount>>;

    /// The revision history of one notification, oldest first (design §8).
    async fn revisions(&self, id: NotificationId, page: PageRequest) -> PortResult<Page<Revision>>;

    /// Full-text search within the caller's channels.
    async fn search(&self, query: &SearchQuery) -> PortResult<Page<Notification>>;

    /// Mark a notification deleted and append its tombstone; idempotent (`None` if
    /// already deleted).
    async fn mark_deleted(
        &self,
        id: NotificationId,
        at: OffsetDateTime,
    ) -> PortResult<Option<Cursor>>;

    /// Claim a batch of expired notifications, leader-only (§11), so the caller can
    /// emit tombstones and counters before the rows go.
    async fn claim_expired(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<ExpiredNotification>>;

    /// Whether any live notification in `channels` references this attachment: the
    /// download authorisation, since objects have no ACL of their own (§6.1).
    async fn attachment_is_visible(
        &self,
        attachment: &AttachmentId,
        channels: &[ChannelId],
    ) -> PortResult<bool>;

    /// Whether a revision in `channel` names this attachment, on a notification live or
    /// deleted after `deleted_after`: its object is kept for that row, so a later publish
    /// in the same channel may name it again. Liveness does not matter, purge does.
    async fn attachment_in_channel(
        &self,
        attachment: &AttachmentId,
        channel: &ChannelId,
        deleted_after: OffsetDateTime,
    ) -> PortResult<bool>;

    /// Attachment ids referenced **only** by rows due for purge (§7.2bis); call before
    /// [`Self::purge`]. Never returns an id a live revision still uses.
    async fn orphaned_attachments(
        &self,
        deleted_before: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<AttachmentId>>;

    /// Hard-delete soft-deleted rows once their tombstones have reached clients (§7.2bis).
    async fn purge(&self, deleted_before: OffsetDateTime, limit: u32) -> PortResult<u64>;

    /// Close open lifecycle notifications idle past their channel's deadline (§7.2bis).
    /// Channels with [`CloseAfter::Never`] are absent from `deadlines`. Sets
    /// `state`/`closed_at` and appends a closure event (no revision).
    ///
    /// [`CloseAfter::Never`]: notify_domain::CloseAfter::Never
    async fn close_stale(
        &self,
        deadlines: &[(ChannelId, OffsetDateTime)],
        at: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<ClosedNotification>>;
}
