//! Getting a notification onto a device that is not listening (design §6.3, §7.1).
//!
//! One trait, one implementation per transport, and a dispatcher that picks by
//! [`notify_domain::PushTransport`]. The interesting part is what the payload looks
//! like when it leaves, which differs by whether a third party carries it:
//!
//! * **Web Push** — the payload goes out in the clear at the application level and
//!   RFC 8291 encrypts it end to end with keys the browser holds internally. There
//!   is no application key on the web at all (design §6.2).
//! * **UnifiedPush and APNs** — a relay we do not control carries the bytes, so the
//!   server seals the payload to the device's X25519 public key first. Sealing
//!   landed with M6, in `notify-push-unifiedpush`'s `sealed_box` module; APNs (M7)
//!   will need the same construction and does not share the module with it yet —
//!   there was exactly one consumer when it was written.
//! * **Too big to carry** — every push transport has a size cap, and a rich
//!   notification can exceed it. Then we send a **poke**: identifiers only, and the
//!   client fetches the real thing over TLS. Degrading to a poke is normal
//!   operation, not an error path.

use notify_domain::{ChannelId, Device, NotificationId, Priority};
use serde::{Deserialize, Serialize};

use crate::error::PortResult;

/// What actually goes out to a device.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum PushBody {
    /// The content, ready to display.
    ///
    /// On Web Push this is the plaintext the browser's own encryption then covers.
    /// On a relayed transport it is the sealed blob, opaque to everyone but the
    /// device (design §6.2).
    Payload {
        /// The bytes to hand the transport.
        bytes: Vec<u8>,
        /// Whether they are sealed to the device key.
        sealed: bool,
    },
    /// Identifiers only. The client wakes, fetches over TLS, and displays.
    Poke,
}

/// One delivery.
///
/// Two shapes, because two things get pushed. Almost everything is a notification;
/// the exception is the digest that goes out when a quiet period ends — **one**
/// push saying what was held back, rather than the burst §7.4 exists to prevent.
/// A digest names no notification and belongs to no channel, so folding it into the
/// same struct would mean two fields that are only sometimes meaningful and a
/// transport that has to know which case it is in anyway.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushMessage {
    /// Something happened in a channel.
    Notification {
        /// Which channel it came from — enough for a client to route it before it
        /// has fetched anything.
        channel: ChannelId,
        /// Which notification.
        notification_id: NotificationId,
        /// How loudly to ask. Transports map this to their own urgency headers; it
        /// does not override the recipient's do-not-disturb, which was already
        /// applied before we got here (design §7.4).
        priority: Priority,
        /// The body, in whichever form fitted.
        body: PushBody,
    },
    /// "12 notifications while you were away (2 failure)" — the one push that ends
    /// a quiet period (design §7.4).
    Digest {
        /// How many were held back.
        total: u32,
        /// How many of those closed on a failure. The one status worth calling out
        /// on a lock screen: it is the difference between "catch up when you can"
        /// and "look now".
        failures: u32,
    },
}

impl PushMessage {
    /// How loudly to ask.
    ///
    /// A digest is deliberately not urgent whatever it contains: it arrives *after*
    /// a quiet period, so nothing in it is news that could not wait — and something
    /// that waited eight hours by request should not then buzz like an alarm.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        match self {
            Self::Notification { priority, .. } => *priority,
            Self::Digest { .. } => Priority::Default,
        }
    }
}

/// How a delivery went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The relay accepted it.
    Delivered,
    /// The relay says this subscription is gone — an uninstalled app, a browser
    /// that dropped the subscription, an expired APNs token.
    ///
    /// Distinct from a failure on purpose: the right response is to revoke the
    /// device, not to retry. A transport that reported this as an error would leave
    /// dead subscriptions being retried until their notifications expired.
    Expired,
    /// It failed and may work later — the caller decides whether to retry.
    Failed {
        /// Why, in the vocabulary of the `push_failures_total{reason}` metric
        /// (design §13). A bounded set: this is a metric label, not a message.
        reason: &'static str,
    },
}

/// Sending to one kind of transport.
#[async_trait::async_trait]
pub trait PushTransport: Send + Sync {
    /// The `transport` label used in metrics and logs (design §13).
    fn name(&self) -> &'static str;

    /// The largest payload this transport will carry. Past it, the caller degrades
    /// to [`PushBody::Poke`].
    fn max_payload_bytes(&self) -> usize;

    /// Deliver to one device.
    ///
    /// Returns `Ok(PushOutcome::Failed { .. })` rather than `Err` for a relay that
    /// refused: a failed push is a routine outcome that belongs in a metric, while
    /// [`crate::error::PortError`] is reserved for the transport itself being
    /// unusable — no credentials, no configuration.
    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome>;
}

/// Picking the right transport for a device.
///
/// The seam the composition root fills: which transports exist is deployment
/// configuration (no APNs key means no APNs), and nothing above this needs to know
/// which ones were wired.
#[async_trait::async_trait]
pub trait PushDispatcher: Send + Sync {
    /// Deliver to a device using whichever transport it enrolled with.
    ///
    /// Returns [`crate::error::PortError::NotConfigured`] when the device enrolled
    /// with a transport this instance does not have — a configuration answer, and
    /// one an operator can act on.
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome>;
}
