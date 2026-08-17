//! Outbound sinks: routing a channel's events somewhere that is not a device
//! (design §7.1bis).
//!
//! Webhooks and email in v1, with the traits shaped so Matrix and Slack are adapters
//! rather than a redesign. Sinks are configured per channel in GitOps and receive
//! the payload in the clear — the server is trusted (design §6).
//!
//! # The webhook contract, and why it is spelled out here
//!
//! A webhook is a public API we have to keep working for consumers we will never
//! meet, so the details are contract rather than implementation:
//!
//! * The body is the stream event, unchanged (design §8.1).
//! * `X-Weebo-Delivery` is an id the consumer deduplicates on. Retries reuse it —
//!   that is the entire point of sending it.
//! * `X-Weebo-Signature: t=<unix>,v1=<hex(hmac_sha256(secret, t + "." + body))>`.
//!   The timestamp is inside the signed material, so a consumer rejecting
//!   `|now − t| > 5 min` gets replay protection for free.
//!
//! Failed deliveries follow [`notify_domain::RetryPolicy`] — 30 s doubling to 1 h,
//! abandoned after 24 h — and land in a DLQ an admin can requeue (scenario T17).
//!
//! # Three traits, because three different things own the answer
//!
//! * [`SinkRegistry`] says **where a channel's events go**. Its answer comes from
//!   the configuration file, so it is synchronous, in-memory, and rebuilt at boot
//!   rather than read from a row.
//! * [`SinkQueue`] says **what is still owed**. That has to survive a restart, so it
//!   is a store.
//! * [`SinkDispatcher`] says **how to talk to one**. That is HTTP, or SMTP.
//!
//! # The secret is never in the queue
//!
//! A [`SinkDelivery`] names its sink; it does not carry the [`SinkTarget`]. The
//! target — and with it the HMAC secret — is resolved from the registry at the
//! moment of delivery, which buys three things that a serialised target would cost:
//! the secret stays out of the database and therefore out of every dump and backup
//! (design §6.1 puts no application crypto at rest, and a shared secret with a
//! third party is the one credential that has no business being there); rotating it
//! in the configuration applies to deliveries already queued; and a sink removed
//! from the configuration stops being retried instead of being retried at a place
//! nobody serves any more.

use notify_domain::{ChannelId, DeliveryId, SinkId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;
use crate::event::Event;

/// Where a channel's events go, besides devices.
///
/// Built from `[[channels.managed.sinks]]` and held in memory for the lifetime of
/// the process. It is deliberately not a stored entity: §11 owns it, the same way it
/// owns trust policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkTarget {
    /// A signed JSON POST.
    Webhook {
        /// Where to POST.
        url: Url,
        /// The HMAC secret, already resolved from `env:` or `file:` by the config
        /// layer. Never rendered in an API response, a log line or a queue row.
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

/// One delivery of one event to one sink, as the queue holds it.
///
/// Everything here is safe to persist: the target is resolved at send time from the
/// registry (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkDelivery {
    /// Stable across retries, sent as `X-Weebo-Delivery`. The consumer's idempotency
    /// key — reusing it on retry is what makes at-least-once delivery survivable for
    /// them.
    pub delivery_id: DeliveryId,
    /// Which channel it came from.
    pub channel: ChannelId,
    /// Which of that channel's sinks it is for.
    pub sink: SinkId,
    /// What to send.
    pub event: Event,
    /// How many attempts have already been made. Zero on the way to the first one.
    pub attempts: u32,
    /// When it first failed, if it has. Drives the give-up window, and it is a
    /// failure clock rather than a publish clock: a consumer that goes down hours
    /// after a busy publish gets its full day from the moment the trouble started.
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
    /// It failed and will keep failing: a 4xx that is not 408 or 429, an address
    /// nobody answers for.
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

    /// Attempt one delivery to an already-resolved target.
    ///
    /// Signing, headers and timeouts are the adapter's business. As with push, a
    /// consumer refusing is an outcome rather than an error: only the dispatcher
    /// itself being unusable is a [`crate::error::PortError`].
    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &SinkTarget,
    ) -> PortResult<SinkOutcome>;
}

/// What the configuration says a channel routes to (design §7.1bis, §11).
///
/// Synchronous on purpose: every answer is already in memory, and an `async` here
/// would invite an implementation that goes and reads something — which is exactly
/// what would make a sink editable through the API, behind §11's back.
pub trait SinkRegistry: Send + Sync {
    /// The sinks declared for this channel, in configuration order.
    fn sinks_for(&self, channel: &ChannelId) -> Vec<SinkId>;

    /// Resolve one to the target to send to, or `None` if the configuration no
    /// longer declares it.
    fn target(&self, channel: &ChannelId, sink: &SinkId) -> Option<SinkTarget>;

    /// Whether this instance routes anywhere at all — what a health check and the
    /// startup log report.
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
    /// Which sink it was going to, endpoint included: the first thing an operator
    /// needs is where to go and look.
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

/// The retry queue and its dead letters.
///
/// Separate from [`SinkDispatcher`] because they answer to different owners: an
/// adapter knows how to talk to a webhook, this knows what to do when it will not
/// answer. The queue is drained by the leader only (design §11).
///
/// # Claiming is a lease, and that is not a detail
///
/// [`Self::claim_due`] leases rather than removes. A drain that deleted the row as
/// it picked it up would lose every in-flight delivery the moment the leader is
/// killed — a silent hole in an at-least-once promise, and the kind that only shows
/// up as "the webhook sometimes just does not arrive". The lease expires on its own,
/// so a replica that dies mid-attempt costs a duplicate delivery at worst, which is
/// precisely what `X-Weebo-Delivery` exists to make survivable.
#[async_trait::async_trait]
pub trait SinkQueue: Send + Sync {
    /// Enqueue a delivery, either for its first attempt or for a retry after
    /// `next_attempt_at`.
    ///
    /// Keyed on [`SinkDelivery::delivery_id`], so re-enqueueing a claimed delivery
    /// is how a retry is scheduled rather than how a duplicate is created.
    async fn enqueue(
        &self,
        delivery: &SinkDelivery,
        next_attempt_at: OffsetDateTime,
    ) -> PortResult<()>;

    /// Claim deliveries that are due, leasing them until `lease_until`.
    ///
    /// Claiming rather than reading: two replicas both becoming leader for a moment
    /// during a failover must not double-deliver a webhook.
    async fn claim_due(
        &self,
        now: OffsetDateTime,
        limit: u32,
        lease_until: OffsetDateTime,
    ) -> PortResult<Vec<SinkDelivery>>;

    /// Drop a delivery that is done with — accepted by the consumer, or belonging to
    /// a sink the configuration no longer declares.
    async fn complete(&self, delivery: &DeliveryId) -> PortResult<()>;

    /// Give up on a delivery and record it as a dead letter. Removes it from the
    /// queue in the same breath: a delivery cannot be both owed and abandoned.
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
    async fn requeue(&self, id: &DeliveryId, at: OffsetDateTime) -> PortResult<bool>;

    /// Current depth per channel, for `sink_dlq_depth` (design §13).
    async fn dead_letter_depth(&self) -> PortResult<Vec<(ChannelId, u64)>>;
}
