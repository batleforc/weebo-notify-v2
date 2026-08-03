//! Outbound sinks: routing a channel's events somewhere that is not a device
//! (design §7.1bis).
//!
//! Webhooks and email in v1, with the trait shaped so Matrix and Slack are adapters
//! rather than a redesign. Sinks are configured per channel in GitOps and receive
//! the payload in the clear — the server is trusted (design §6).
//!
//! # The webhook contract, and why it is spelled out here
//!
//! A webhook is a public API we have to keep working for consumers we will never
//! meet, so the details are contract rather than implementation:
//!
//! * The body is the stream event, unchanged (design §8.1).
//! * `X-Weebo-Delivery` is a UUID the consumer deduplicates on. Retries reuse it —
//!   that is the entire point of sending it.
//! * `X-Weebo-Signature: t=<unix>,v1=<hex(hmac_sha256(secret, t + "." + body))>`.
//!   The timestamp is inside the signed material, so a consumer rejecting
//!   `|now − t| > 5 min` gets replay protection for free.
//!
//! Failed deliveries back off from 30 s to 1 h, give up after 24 h, and land in a
//! DLQ an admin can requeue (design §7.1bis, scenario T17).

use notify_domain::ChannelId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;
use crate::event::Event;

/// Where a channel's events go, besides devices.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SinkTarget {
    /// A signed JSON POST.
    Webhook {
        /// Where to POST.
        url: Url,
        /// The HMAC secret, already resolved from `env:` or `file:` by the config
        /// layer. Never rendered in an API response or a log line.
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
}

/// One attempt to deliver one event to one sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkDelivery {
    /// A UUID, stable across retries, sent as `X-Weebo-Delivery`. The consumer's
    /// idempotency key — reusing it on retry is what makes at-least-once delivery
    /// survivable for them.
    pub delivery_id: String,
    /// Which channel it came from.
    pub channel: ChannelId,
    /// Where it is going.
    pub target: SinkTarget,
    /// What to send.
    pub event: Event,
    /// Which attempt this is, starting at 1. Drives the backoff and decides when
    /// the delivery is old enough for the DLQ.
    pub attempt: u32,
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
    /// It failed and will keep failing: a 4xx that is not 408 or 429, a URL that
    /// no longer resolves.
    ///
    /// Retrying a `410 Gone` for 24 hours helps nobody; this outcome sends it
    /// straight to the DLQ where a human can see it.
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

    /// Attempt one delivery.
    ///
    /// Signing, headers and timeouts are the adapter's business. As with push, a
    /// consumer refusing is an outcome rather than an error: only the dispatcher
    /// itself being unusable is a [`crate::error::PortError`].
    async fn deliver(&self, delivery: &SinkDelivery) -> PortResult<SinkOutcome>;
}

/// A delivery that ran out of retries (design §7.1bis).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadLetter {
    /// Its id, for `POST /admin/dlq/{id}/requeue`.
    pub id: String,
    /// Which channel it came from — the dimension of the `sink_dlq_depth` metric.
    pub channel: ChannelId,
    /// What kind of sink it was going to.
    pub sink_kind: &'static str,
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

/// The retry queue and its dead letters.
///
/// Separate from [`SinkDispatcher`] because they answer to different owners: an
/// adapter knows how to talk to a webhook, this knows what to do when it will not
/// answer. The queue is drained by the leader only (design §11).
#[async_trait::async_trait]
pub trait SinkQueue: Send + Sync {
    /// Enqueue a delivery, either for its first attempt or for a retry after
    /// `next_attempt_at`.
    async fn enqueue(
        &self,
        delivery: &SinkDelivery,
        next_attempt_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Claim deliveries that are due.
    ///
    /// Claiming rather than reading: two replicas both becoming leader for a moment
    /// during a failover must not double-deliver a webhook.
    async fn claim_due(&self, now: OffsetDateTime, limit: u32) -> PortResult<Vec<SinkDelivery>>;

    /// Give up on a delivery and record it as a dead letter.
    async fn bury(
        &self,
        delivery: &SinkDelivery,
        error: &str,
        at: OffsetDateTime,
    ) -> PortResult<()>;

    /// The dead letters of a channel, or of every channel when `channel` is `None`
    /// (design §8, `GET /admin/dlq`).
    async fn dead_letters(&self, channel: Option<&ChannelId>) -> PortResult<Vec<DeadLetter>>;

    /// Put a dead letter back in the queue (design §8, scenario T17). Returns
    /// whether there was one to requeue.
    async fn requeue(&self, id: &str, at: OffsetDateTime) -> PortResult<bool>;

    /// Current depth per channel, for `sink_dlq_depth` (design §13).
    async fn dead_letter_depth(&self) -> PortResult<Vec<(ChannelId, u64)>>;
}
