//! What a publish still owes once it commits — recorded in the same transaction
//! (ADR-0025).
//!
//! A publish used to commit, then push, queue the sinks, derive a thumbnail and
//! store its idempotent replay, all inside the request. A request is a future, and a
//! future can be dropped: a client giving up or the shutdown drain running out lost
//! whatever had not happened yet, and a retry with the same `Idempotency-Key` found
//! a claim with no response and published twice once it lapsed.
//!
//! So the write carries an [`AfterWrite`], and the store commits it with the
//! revision: the sink deliveries go straight into the sink queue, the replay is
//! stored against the key, and the device push and thumbnail become one
//! [`PendingFanout`] row a worker claims ([`FanoutOutbox`]). Either all of it
//! happened or none of it did.

use notify_domain::{
    AttachmentId, Cursor, DeliveryId, DeviceId, Notification, NotificationId, Revision, SinkId,
};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::event::{Audience, Event, EventPayload, RevisionEvent};
use crate::sinks::SinkDelivery;

/// The work a write leaves owed, committed with it. `Default` is "nothing".
#[derive(Debug, Default)]
pub struct AfterWrite {
    /// Record a [`PendingFanout`]: push to devices, then derive `thumbnail`.
    pub fanout: bool,
    /// The attachment whose thumbnail the worker derives (design D30), if any.
    pub thumbnail: Option<AttachmentId>,
    /// The channel's sinks, one queued delivery each (design §7.1bis).
    pub sinks: Vec<SinkId>,
    /// The `Idempotency-Key` response to store (design §8, T16).
    pub reply: Option<IdempotentReply>,
}

/// An `Idempotency-Key` response, stored with the write it answers for.
///
/// The response embeds the cursor, which only exists inside the transaction — so
/// the caller hands over how to encode it rather than the bytes.
pub struct IdempotentReply {
    /// The claim's owner ([`notify_domain::Principal::reference`]).
    pub principal_ref: String,
    /// The key.
    pub key: String,
    /// The `now` the claim was made with — its identity, as in
    /// [`crate::IdempotencyStore::complete`]. A claim that was taken over is left
    /// alone.
    pub claimed_at: OffsetDateTime,
    /// When the replay stops being served.
    pub expires_at: OffsetDateTime,
    /// The response, given the cursor the write landed on.
    pub encode: Box<dyn Fn(Cursor) -> PortResult<String> + Send + Sync>,
}

impl core::fmt::Debug for IdempotentReply {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IdempotentReply")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl AfterWrite {
    /// The sink deliveries this write owes, each with its own id.
    ///
    /// The event is rebuilt from the same three values the store appended it from,
    /// so a consumer sees byte for byte what a client sees on the stream (§8.1).
    /// One id per sink, not per publish: `X-Weebo-Delivery` is the consumer's
    /// idempotency key, and two consumers sharing one would make each other's
    /// deduplication wrong.
    #[must_use]
    pub fn deliveries(
        &self,
        notification: &Notification,
        revision: &Revision,
        cursor: Cursor,
    ) -> Vec<SinkDelivery> {
        if self.sinks.is_empty() {
            return Vec::new();
        }
        let event = Event {
            cursor,
            audience: Audience::Channel {
                id: notification.channel.clone(),
            },
            recorded_at: revision.received_at,
            // A fact about a delivery, and the log is read by everybody (§7.4, D22).
            muted_by_dnd: false,
            payload: EventPayload::Revision(Box::new(RevisionEvent::new(notification, revision))),
        };
        self.sinks
            .iter()
            .map(|sink| SinkDelivery {
                delivery_id: DeliveryId::generate(),
                channel: notification.channel.clone(),
                sink: sink.clone(),
                event: event.clone(),
                attempts: 0,
                first_failed_at: None,
            })
            .collect()
    }
}

/// Device push and thumbnail work a committed write still owes.
///
/// Carries the notification's id and not the snapshot the write committed: the
/// worker re-reads the notification when it gets to the row, so a delete that
/// landed in between pushes nothing, and two revisions drained out of order both
/// push the current head rather than the older one last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFanout {
    /// The write's cursor, which is also the row's identity — one write, one row.
    pub cursor: Cursor,
    /// The notification the write was about.
    pub notification: NotificationId,
    /// The attachment to thumbnail, if the revision showed one.
    pub thumbnail: Option<AttachmentId>,
    /// How many times the row has been claimed, this claim included.
    pub attempts: u32,
}

/// The pending fan-out rows.
///
/// Claimed as a lease, like [`crate::SinkQueue::claim_due`], which the worker
/// keeps extending while it pushes ([`FanoutOutbox::extend_fanout`]): a worker
/// killed mid-push leaves its rows owed again once the lease runs out, and the
/// retry skips the devices already pushed ([`FanoutOutbox::record_delivery`]). A
/// crash therefore costs at most one duplicate, for the devices whose push was in
/// flight (clients replace a notification by id), never a lost one.
///
/// And bounded, so no row can hold the outbox hostage: each claim counts as an
/// attempt, and a row that has used up `max_attempts`, that cannot be decoded, or
/// that has waited past `too_old` is moved aside as dead instead of being claimed
/// again — one bad row never blocks the others.
#[async_trait::async_trait]
pub trait FanoutOutbox: Send + Sync {
    /// Lease up to `limit` live rows that nobody holds, oldest first, until
    /// `lease_until`. Safe from any number of replicas at once.
    ///
    /// Retires as dead, first, every unheld row that has been claimed
    /// `max_attempts` times already or was queued before `too_old`.
    async fn claim_fanout(
        &self,
        now: OffsetDateTime,
        limit: u32,
        lease_until: OffsetDateTime,
        max_attempts: u32,
        too_old: OffsetDateTime,
    ) -> PortResult<Vec<PendingFanout>>;

    /// Drop a row whose work is done — and, once it was its notification's last,
    /// the deliveries [`FanoutOutbox::record_delivery`] kept for it.
    async fn complete_fanout(&self, cursor: Cursor) -> PortResult<()>;

    /// Hold `cursors` until `lease_until`: the worker's heartbeat while it is still
    /// pushing, so a live worker never has its rows claimed from under it.
    async fn extend_fanout(
        &self,
        cursors: &[Cursor],
        lease_until: OffsetDateTime,
    ) -> PortResult<()>;

    /// The devices an earlier attempt already pushed revision `seq` of
    /// `notification` to.
    async fn delivered_devices(
        &self,
        notification: NotificationId,
        seq: u32,
    ) -> PortResult<Vec<DeviceId>>;

    /// Remember that `device` was pushed revision `seq` of `notification`, so a
    /// retry skips it. Idempotent.
    async fn record_delivery(
        &self,
        notification: NotificationId,
        seq: u32,
        device: DeviceId,
    ) -> PortResult<()>;

    /// Give a row back unfinished: record why, and make it claimable again at
    /// `retry_at` rather than when its lease would have run out.
    async fn retry_fanout(
        &self,
        cursor: Cursor,
        error: &str,
        retry_at: OffsetDateTime,
    ) -> PortResult<()>;
}
