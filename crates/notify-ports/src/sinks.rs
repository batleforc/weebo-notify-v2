//! Outbound sinks per channel (§7.1bis): signed webhooks deduped by `X-Weebo-Delivery`,
//! retried per [`notify_domain::RetryPolicy`] into a DLQ (T17). Deliveries name their
//! sink; the secret is resolved from config at send time and never stored.

use notify_domain::{ChannelId, DeliveryId, SinkId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;
use crate::event::Event;

/// Where a channel's events go besides devices; built from config, held in memory (§11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkTarget {
    /// A signed JSON POST.
    Webhook {
        /// Where to POST.
        url: Url,
        /// The HMAC secret, resolved from `env:`/`file:`. Never rendered in a response,
        /// log line or queue row.
        secret: String,
    },
    /// An email.
    Email {
        /// Where to send it.
        address: String,
    },
}

impl SinkTarget {
    /// The `type` label used in metrics and in the DLQ listing (design §13).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Webhook { .. } => "webhook",
            Self::Email { .. } => "email",
        }
    }

    /// Where it points, which is also how it is identified (design §7.1bis).
    #[must_use]
    pub fn destination(&self) -> String {
        match self {
            Self::Webhook { url, .. } => url.to_string(),
            Self::Email { address } => address.clone(),
        }
    }

    /// Its identity — `kind:destination`, free of the secret above.
    pub fn id(&self) -> notify_domain::Result<SinkId> {
        SinkId::new(self.kind(), &self.destination())
    }
}

/// One delivery of one event to one sink, as the queue holds it (safe to persist).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkDelivery {
    /// Stable across retries, sent as `X-Weebo-Delivery` for consumer dedup.
    pub delivery_id: DeliveryId,
    /// Which channel it came from.
    pub channel: ChannelId,
    /// Which of that channel's sinks it is for.
    pub sink: SinkId,
    /// What to send.
    pub event: Event,
    /// How many attempts have already been made. Zero on the way to the first one.
    pub attempts: u32,
    /// When it first failed, if it has; the give-up window counts from here.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub first_failed_at: Option<OffsetDateTime>,
}

/// How an attempt went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkOutcome {
    /// The consumer accepted it.
    Delivered,
    /// It failed and is worth retrying — a timeout, a 5xx, a connection refused.
    Retry {
        /// Why, for the log and the DLQ listing.
        reason: String,
    },
    /// It failed and will keep failing (a 4xx other than 408/429): straight to the DLQ.
    Rejected {
        /// Why.
        reason: String,
    },
}

/// Sending to a sink.
#[async_trait::async_trait]
pub trait SinkDispatcher: Send + Sync {
    /// The `type` values this dispatcher handles.
    fn handles(&self) -> &'static [&'static str];

    /// Attempt one delivery to an already-resolved target. A consumer refusing is an
    /// outcome; only an unusable dispatcher is a [`crate::error::PortError`].
    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &SinkTarget,
    ) -> PortResult<SinkOutcome>;
}

/// What the configuration says a channel routes to (§7.1bis, §11). Synchronous so
/// no implementation reads sinks from anywhere but config.
pub trait SinkRegistry: Send + Sync {
    /// The sinks declared for this channel, in configuration order.
    fn sinks_for(&self, channel: &ChannelId) -> Vec<SinkId>;

    /// Resolve one to its target, or `None` if config no longer declares it.
    fn target(&self, channel: &ChannelId, sink: &SinkId) -> Option<SinkTarget>;

    /// Whether this instance routes anywhere at all.
    fn is_empty(&self) -> bool;
}

/// A delivery that ran out of retries (design §7.1bis).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadLetter {
    /// Its id, for `POST /admin/dlq/{id}/requeue`.
    pub id: DeliveryId,
    /// Which channel it came from — the dimension of the `sink_dlq_depth` metric.
    pub channel: ChannelId,
    /// Which sink it was going to, endpoint included.
    pub sink: SinkId,
    /// The last failure.
    pub last_error: String,
    /// How many attempts it took before giving up.
    pub attempts: u32,
    /// When it first failed.
    #[serde(with = "time::serde::rfc3339")]
    pub first_failed_at: OffsetDateTime,
    /// When it was given up on.
    #[serde(with = "time::serde::rfc3339")]
    pub dead_at: OffsetDateTime,
}

/// The retry queue and its dead letters, drained by the leader only (§11).
/// [`Self::claim_due`] leases rather than removes, so a dead leader costs a
/// duplicate delivery, never a lost one.
#[async_trait::async_trait]
pub trait SinkQueue: Send + Sync {
    /// Enqueue a delivery for its first attempt or a retry after `next_attempt_at`.
    /// Keyed on [`SinkDelivery::delivery_id`], so re-enqueueing never duplicates.
    async fn enqueue(
        &self,
        delivery: &SinkDelivery,
        next_attempt_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Lease due deliveries until `lease_until`, so concurrent leaders don't double-deliver.
    async fn claim_due(
        &self,
        now: OffsetDateTime,
        limit: u32,
        lease_until: OffsetDateTime,
    ) -> PortResult<Vec<SinkDelivery>>;

    /// Drop a finished delivery (accepted, or its sink is no longer configured).
    async fn complete(&self, delivery: &DeliveryId) -> PortResult<()>;

    /// Give up on a delivery: record a dead letter and dequeue it atomically.
    async fn bury(
        &self,
        delivery: &SinkDelivery,
        error: &str,
        at: OffsetDateTime,
    ) -> PortResult<()>;

    /// The dead letters of a channel, or of every channel when `None` (`GET /admin/dlq`).
    async fn dead_letters(&self, channel: Option<&ChannelId>) -> PortResult<Vec<DeadLetter>>;

    /// Put a dead letter back in the queue (T17). Returns whether there was one.
    async fn requeue(&self, id: &DeliveryId, at: OffsetDateTime) -> PortResult<bool>;

    /// Current depth per channel, for `sink_dlq_depth` (design §13).
    async fn dead_letter_depth(&self) -> PortResult<Vec<(ChannelId, u64)>>;
}
