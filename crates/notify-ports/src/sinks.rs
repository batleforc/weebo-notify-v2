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

    /// Where it points, as far as it is safe to say (design §7.1bis).
    ///
    /// A webhook is shown as [`redact_url`] renders it — origin plus keyed
    /// fingerprint — and never in full: plenty of consumers (Slack, Discord, Teams)
    /// put their credential in the *path*, and this string is stored in the queue,
    /// listed by the dead-letter API, written into the audit log and logged.
    #[must_use]
    pub fn destination(&self, key: &FingerprintKey) -> String {
        match self {
            Self::Webhook { url, .. } => redact_url(url, key),
            Self::Email { address } => address.clone(),
        }
    }

    /// Its identity — `kind:destination`, free of the secret above and of whatever
    /// the url's path carries.
    pub fn id(&self, key: &FingerprintKey) -> notify_domain::Result<SinkId> {
        SinkId::new(self.kind(), &self.destination(key))
    }
}

/// The server-side key (`[sinks] fingerprint_key`) a webhook url's fingerprint is
/// an HMAC under. A plain digest of a stored url is a brute-force target for short
/// query tokens. Holds the MAC, never the bytes; `Debug` redacts it.
#[derive(Clone)]
pub struct FingerprintKey(hmac::Hmac<sha2::Sha256>);

impl FingerprintKey {
    /// 256 bits, the HMAC's own strength.
    pub const MIN_BYTES: usize = 32;

    /// `None` when `key` is shorter than [`Self::MIN_BYTES`].
    #[must_use]
    pub fn new(key: &[u8]) -> Option<Self> {
        if key.len() < Self::MIN_BYTES {
            return None;
        }
        <hmac::Hmac<sha2::Sha256> as hmac::KeyInit>::new_from_slice(key)
            .ok()
            .map(Self)
    }
}

impl core::fmt::Debug for FingerprintKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FingerprintKey([redacted])")
    }
}

/// Hex digits of the fingerprint a redacted url keeps (64 bits).
const FINGERPRINT_HEX: usize = 16;

/// A url with everything after its origin replaced by a keyed fingerprint:
/// `https://hooks.slack.com#4f1c0a9e2b7d3c58`. Path and query often hold the
/// credential. Stable per key, since queued deliveries find their sink by it;
/// rotation goes through `[sinks] previous_fingerprint_keys`.
#[must_use]
pub fn redact_url(url: &Url, key: &FingerprintKey) -> String {
    use hmac::Mac as _;

    let mut mac = key.0.clone();
    mac.update(url.as_str().as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut out = url[..url::Position::BeforePath].to_owned();
    out.push('#');
    let mut fingerprint = base16ct::lower::encode_string(&digest);
    fingerprint.truncate(FINGERPRINT_HEX);
    out.push_str(&fingerprint);
    out
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
    /// Which sink it was going to: origin plus keyed fingerprint ([`redact_url`]).
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

    /// Drop a finished delivery (accepted, or answered for by the dispatcher).
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

    /// Forget dead letters given up on before `older_than`, at most `limit` of them.
    /// Returns how many went.
    ///
    /// A dead letter carries the whole event it failed to deliver, so keeping them
    /// for ever is a retention policy nobody chose; the leader's drain prunes them
    /// once they are older than `[sinks] dead_letter_retention`.
    async fn prune_dead_letters(&self, older_than: OffsetDateTime, limit: u32) -> PortResult<u64>;

    /// Rename stored sink ids — queued deliveries and dead letters alike — to
    /// whatever `rekey` maps them to; an id it answers `None` for is left as it is.
    /// Returns how many rows moved.
    ///
    /// Run once at boot: a webhook's id is a keyed fingerprint (see
    /// [`FingerprintKey`]), so a key rotation or an upgrade from a build that stored
    /// the url changes the id of every row already written, and only the server
    /// knows both the key and the urls needed to say what each became.
    async fn rekey_sinks(
        &self,
        rekey: &(dyn for<'s> Fn(&'s str) -> Option<SinkId> + Sync),
    ) -> PortResult<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> FingerprintKey {
        FingerprintKey::new(&[byte; FingerprintKey::MIN_BYTES]).unwrap()
    }

    #[test]
    fn a_fingerprint_is_stable_under_one_key_and_unrelated_under_another() {
        let url = Url::parse("https://hooks.slack.com/services/T0/B0/SECRETPATH").unwrap();

        let once = redact_url(&url, &key(1));
        assert_eq!(
            once,
            redact_url(&url, &key(1)),
            "a queued row must find its sink again"
        );
        assert_ne!(
            once,
            redact_url(&url, &key(2)),
            "without the key it says nothing"
        );
        assert!(once.starts_with("https://hooks.slack.com#"), "{once}");
        assert!(!once.contains("SECRET"), "{once}");
        assert_eq!(
            once.len(),
            "https://hooks.slack.com#".len() + FINGERPRINT_HEX
        );
    }

    #[test]
    fn a_short_key_is_refused_and_a_key_is_never_printed() {
        assert!(FingerprintKey::new(&[7; FingerprintKey::MIN_BYTES - 1]).is_none());
        assert_eq!(format!("{:?}", key(b'k')), "FingerprintKey([redacted])");
    }
}
