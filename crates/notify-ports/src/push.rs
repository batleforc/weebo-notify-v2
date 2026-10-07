//! Pushing to a device that is not listening (§6.3, §7.1). Web Push relies on
//! RFC 8291; relayed transports get a payload sealed to the device's X25519 key
//! (`notify-sealed-box`); oversize payloads degrade to a poke, which is normal.

use notify_domain::{ChannelId, Device, NotificationId, Priority};
use serde::{Deserialize, Serialize};

use crate::error::PortResult;

/// What actually goes out to a device.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum PushBody {
    /// The content, ready to display: plaintext on Web Push, the sealed blob on a
    /// relayed transport (§6.2).
    Payload {
        /// The bytes to hand the transport.
        bytes: Vec<u8>,
        /// Whether they are sealed to the device key.
        sealed: bool,
    },
    /// Identifiers only. The client wakes, fetches over TLS, and displays.
    Poke,
}

/// One delivery: a notification, or the digest that ends a quiet period (§7.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushMessage {
    /// Something happened in a channel.
    Notification {
        /// Which channel it came from, so a client can route it before fetching anything.
        channel: ChannelId,
        /// Which notification.
        notification_id: NotificationId,
        /// How loudly to ask; recipient do-not-disturb was already applied (§7.4).
        priority: Priority,
        /// The body, in whichever form fitted.
        body: PushBody,
    },
    /// "12 notifications while you were away (2 failure)", ending a quiet period (§7.4).
    Digest {
        /// How many were held back.
        total: u32,
        /// How many of those closed on a failure.
        failures: u32,
    },
}

impl PushMessage {
    /// How loudly to ask. A digest is never urgent: it follows a requested quiet period.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        match self {
            Self::Notification { priority, .. } => *priority,
            Self::Digest { .. } => Priority::Default,
        }
    }

    /// What a relayed transport seals to the device key (envelope v2, §6.3): every
    /// identifier rides inside the box so the relay learns nothing.
    #[must_use]
    pub fn sealed_plaintext(&self) -> Vec<u8> {
        let value = match self {
            Self::Notification {
                channel,
                notification_id,
                body,
                ..
            } => {
                let mut value = serde_json::json!({
                    "kind": "notification",
                    "channel": channel,
                    "notification_id": notification_id,
                });
                if let PushBody::Payload { bytes, .. } = body {
                    value["content"] =
                        serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
                }
                value
            }
            Self::Digest { total, failures } => serde_json::json!({
                "kind": "digest",
                "total": total,
                "failures": failures,
            }),
        };
        serde_json::to_vec(&value).unwrap_or_default()
    }
}

/// The envelope version relayed transports put in the clear (design §6.3): `2` is
/// "everything but the ciphertext is sealed", see [`PushMessage::sealed_plaintext`].
pub const SEALED_ENVELOPE_VERSION: u8 = 2;

/// How a delivery went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The relay accepted it.
    Delivered,
    /// The relay says this subscription is gone: revoke the device, don't retry.
    Expired,
    /// It failed and may work later — the caller decides whether to retry.
    Failed {
        /// Why, as a bounded `push_failures_total{reason}` label (§13).
        reason: &'static str,
    },
}

/// Sending to one kind of transport.
#[async_trait::async_trait]
pub trait PushTransport: Send + Sync {
    /// The `transport` label used in metrics and logs (design §13).
    fn name(&self) -> &'static str;

    /// The largest payload this transport will carry; past it, send [`PushBody::Poke`].
    fn max_payload_bytes(&self) -> usize;

    /// Deliver to one device. A relay refusal is `Ok(PushOutcome::Failed { .. })`;
    /// `Err` means the transport itself is unusable.
    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome>;
}

/// Picking the right transport for a device; which ones exist is deployment config.
#[async_trait::async_trait]
pub trait PushDispatcher: Send + Sync {
    /// Deliver via the device's enrolled transport;
    /// [`crate::error::PortError::NotConfigured`] if this instance lacks it.
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome>;
}
