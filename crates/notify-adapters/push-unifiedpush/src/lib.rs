//! UnifiedPush: a self-hosted relay, and the sealed box it exists to justify
//! (design §6.2, §6.3, §7.1, §10bis.4, M6).
//!
//! # The first transport with an application key
//!
//! Web Push (M3.5) never needed one: RFC 8291 already encrypts end to end with keys
//! the browser holds internally, so [`notify_ports::PushBody::Payload`]'s `sealed`
//! flag was always `false` on that path. Here it stays `false` too — sealing is not
//! a flag the fan-out flips, it is this transport's own job, done against
//! `device.public_key` once it has the plaintext. The shape existed since the
//! device model was written (§6.2's invariant: no relayed transport enrols without
//! a key); the crypto behind it is `notify_sealed_box`, its own crate since M7 once
//! `notify-push-apns` became the sealed box's second consumer.
//!
//! # What "self-hosted" means for the wire format
//!
//! [`notify_domain::PushTransport::UnifiedPush`] carries one field: the distributor
//! endpoint to `POST` to. Whether that endpoint is a real external relay (a system
//! distributor such as ntfy, talking to infrastructure this project does not run) or
//! this product's own embedded distributor is a decision the **device** makes at
//! enrolment (design §10bis.4) — this transport does the same thing either way, POST
//! a sealed envelope to whatever URL the device handed over at `POST /devices`. That
//! symmetry is deliberate: a server that behaved differently for "its own" devices
//! would be trusting the embedded case with less crypto than the one path that
//! actually leaves this project's infrastructure, which is exactly backwards.
//!
//! # What this crate does not take
//!
//! `[push.unifiedpush]`'s rationale (design §7.1bis) mentions a "per-device opaque
//! token" authenticating an inbound public entry point of this server's own. No such
//! route exists in §8's endpoint table, and none is added here: this crate is the
//! **outbound** half only — the fan-out dispatches to a device's registered
//! endpoint, mirroring `notify-push-webpush` exactly. An inbound endpoint would only
//! matter for an embedded-distributor flow that is itself Android/Kotlin work, not
//! server work, and is left for whoever writes that half to specify against a real
//! §8 route rather than a rationale paragraph.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notify_domain::{ChannelId, Device, DevicePublicKey, NotificationId};
use notify_ports::{
    PortError, PortResult, PushBody, PushDispatcher, PushMessage, PushOutcome, PushTransport,
};
use serde::Serialize;

/// The most plaintext one push may carry.
///
/// Unlike RFC 8291 (Web Push), UnifiedPush's spec does not guarantee a minimum body
/// size a distributor must accept — this is a deliberately conservative budget
/// rather than a cited number. Sealing adds 72 octets (32-octet ephemeral key,
/// 24-octet nonce, 16-octet AEAD tag), the sealed bytes are base64'd (a third more),
/// and the envelope's `channel`/`notification_id` fields cost roughly another 110
/// octets of JSON — the arithmetic that keeps the wire body under 4 KiB, a size most
/// HTTP intermediaries accept without configuration. Revisit once a real distributor
/// gives a number worth citing instead of a margin.
const MAX_PAYLOAD: usize = 2_800;

/// Why the transport could not be built (design §11, `[push.unifiedpush]`).
///
/// Its own type rather than a [`PortError`], matching `notify-push-webpush`: this is
/// an operator's configuration problem, found at startup.
#[derive(Debug, thiserror::Error)]
pub enum UnifiedPushSetup {
    /// The HTTP client could not be built.
    #[error("[push.unifiedpush] the HTTP client could not be built: {0}")]
    Client(#[source] reqwest::Error),
}

/// What crosses the wire to a distributor endpoint (design §6.3).
///
/// Unlike Web Push's `Envelope`, which puts the whole message inside the browser's
/// own encryption, `channel` and `notification_id` travel in the clear here — the
/// design's own wire shape for the mobile blob (§6.3: `{channel_id, notification_id,
/// ciphertext_device}`), presumably so a distributor can route or coalesce without
/// ever holding a device's private key, which it never has. Only content crosses
/// sealed.
#[derive(Debug, Serialize)]
struct Envelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<ChannelId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notification_id: Option<NotificationId>,
    /// The sealed box, base64. Absent only for a poke: nothing to protect but the
    /// identifiers above, and the client fetches the rest over TLS (design §6.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    ciphertext_device: Option<String>,
}

/// What a digest seals to (design §7.4) — no channel, no notification, just the two
/// counts a quiet period ended with.
#[derive(Serialize)]
struct DigestPayload {
    total: u32,
    failures: u32,
}

impl Envelope {
    /// Build one from what the fan-out decided to send, sealing whatever plaintext
    /// there is to `key`.
    ///
    /// `None` propagates a weak recipient key (design's threat model trusts an
    /// enrolled device, but not blindly — see [`notify_sealed_box::seal`]) rather than
    /// silently sending an envelope with no ciphertext, which would read to a
    /// distributor exactly like a poke and be wrong in a way nothing downstream
    /// could tell apart from the real thing.
    fn sealed_for(key: &DevicePublicKey, message: &PushMessage) -> Option<Self> {
        Some(match message {
            PushMessage::Notification {
                channel,
                notification_id,
                body,
                ..
            } => Self {
                channel: Some(channel.clone()),
                notification_id: Some(*notification_id),
                ciphertext_device: match body {
                    PushBody::Poke => None,
                    PushBody::Payload { bytes, .. } => Some(seal_base64(key, bytes)?),
                },
            },
            PushMessage::Digest { total, failures } => {
                let plaintext = serde_json::to_vec(&DigestPayload {
                    total: *total,
                    failures: *failures,
                })
                .unwrap_or_default();
                Self {
                    channel: None,
                    notification_id: None,
                    ciphertext_device: Some(seal_base64(key, &plaintext)?),
                }
            }
        })
    }
}

fn seal_base64(key: &DevicePublicKey, plaintext: &[u8]) -> Option<String> {
    notify_sealed_box::seal(key, plaintext).map(|sealed| URL_SAFE_NO_PAD.encode(sealed))
}

/// UnifiedPush, over the wire.
#[derive(Debug)]
pub struct UnifiedPush {
    http: reqwest::Client,
}

impl UnifiedPush {
    /// Build one.
    ///
    /// # Errors
    ///
    /// Fails only if the HTTP client cannot be built — there is no key to validate
    /// and no credential to read, unlike Web Push's VAPID key: the destination is
    /// per-device (the endpoint from enrolment), and nothing here signs or
    /// identifies this server to it.
    pub fn new() -> Result<Self, UnifiedPushSetup> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(UnifiedPushSetup::Client)?,
        })
    }
}

#[async_trait::async_trait]
impl PushTransport for UnifiedPush {
    fn name(&self) -> &'static str {
        "unifiedpush"
    }

    fn max_payload_bytes(&self) -> usize {
        MAX_PAYLOAD
    }

    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let notify_domain::PushTransport::UnifiedPush { endpoint } = &device.transport else {
            // The registry dispatches by name, so reaching here means a device whose
            // stored transport disagrees with the name it enrolled under.
            return Err(PortError::corrupt(
                "device.transport",
                "not a UnifiedPush registration",
            ));
        };

        // `Device::enrol` refuses a relayed transport with no key (design §6.2), so
        // a `None` here is not a device that forgot to publish one — it is stored
        // state disagreeing with an invariant the domain is supposed to hold for it.
        let Some(public_key) = device.public_key else {
            return Err(PortError::corrupt(
                "device.public_key",
                "a UnifiedPush device enrolled with no key",
            ));
        };

        // A key that fails the contributory check will never seal to anything a
        // real device could open — reported as expired rather than retried, the same
        // response Web Push gives a subscription whose keys cannot be read.
        let Some(envelope) = Envelope::sealed_for(&public_key, message) else {
            return Ok(PushOutcome::Expired);
        };

        let body = serde_json::to_vec(&envelope)
            .map_err(|error| PortError::corrupt("push.envelope", error))?;

        let response = self
            .http
            .post(endpoint.clone())
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await;

        Ok(match response {
            Ok(response) => outcome(response.status().as_u16()),
            // The distributor endpoint was unreachable. A routine outcome on a flaky
            // network and a metric rather than an error: `Err` is for this transport
            // being unusable, which a dropped connection is not.
            Err(_) => PushOutcome::Failed {
                reason: "unreachable",
            },
        })
    }
}

/// UnifiedPush as a dispatcher, so this crate's own tests reach it through the trait
/// the fan-out uses rather than through the concrete type — the same reason
/// `notify-push-webpush` has one.
#[async_trait::async_trait]
impl PushDispatcher for UnifiedPush {
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        self.deliver(device, message).await
    }
}

/// How a distributor's answer maps onto [`PushOutcome`].
///
/// The distinction that matters is `Expired`: `404`/`410` mean the endpoint is gone
/// — an uninstalled distributor, an app that revoked its own registration — and the
/// right response is to revoke the device rather than retry it until the
/// notification expires. Same table `notify-push-webpush` uses: nothing here is
/// specific to either relay, both are answering as HTTP servers.
const fn outcome(status: u16) -> PushOutcome {
    match status {
        200..=299 => PushOutcome::Delivered,
        404 | 410 => PushOutcome::Expired,
        413 => PushOutcome::Failed {
            reason: "too_large",
        },
        429 => PushOutcome::Failed {
            reason: "rate_limited",
        },
        400..=499 => PushOutcome::Failed { reason: "rejected" },
        _ => PushOutcome::Failed {
            reason: "relay_unavailable",
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{ChannelId, DeviceId, Principal, Priority, UserId};
    use time::OffsetDateTime;
    use x25519_dalek::{PublicKey, StaticSecret};

    use super::*;

    fn device_key() -> DevicePublicKey {
        let secret = StaticSecret::random();
        DevicePublicKey::from_bytes(*PublicKey::from(&secret).as_bytes())
    }

    fn device_with(endpoint: &str, key: DevicePublicKey) -> Device {
        Device::enrol(
            DeviceId::generate(),
            Principal::User {
                id: UserId::generate(),
            },
            "phone",
            notify_domain::PushTransport::UnifiedPush {
                endpoint: endpoint.parse().unwrap(),
            },
            Some(key),
            OffsetDateTime::now_utc(),
        )
        .unwrap()
    }

    fn notification() -> PushMessage {
        PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Payload {
                bytes: serde_json::to_vec(&serde_json::json!({ "title": "etcd degraded" }))
                    .unwrap(),
                sealed: false,
            },
        }
    }

    #[test]
    fn a_transport_this_deployment_has_no_config_to_get_wrong() {
        // Unlike Web Push there is no key file to fail on — the point of the
        // constructor test is that there is nothing here to fail.
        assert!(UnifiedPush::new().is_ok());
    }

    #[tokio::test]
    async fn a_device_whose_stored_transport_disagrees_with_its_name_is_corrupt_not_failed() {
        let transport = UnifiedPush::new().unwrap();
        let device = Device::enrol(
            DeviceId::generate(),
            Principal::User {
                id: UserId::generate(),
            },
            "browser",
            notify_domain::PushTransport::WebPush {
                endpoint: "https://push.example/abc".parse().unwrap(),
                p256dh: "BPk".to_owned(),
                auth: "k9".to_owned(),
            },
            None,
            OffsetDateTime::now_utc(),
        )
        .unwrap();

        let error = transport
            .deliver(&device, &notification())
            .await
            .unwrap_err();
        assert!(matches!(error, PortError::Corrupt { .. }));
    }

    #[tokio::test]
    async fn a_weak_device_key_is_reported_expired_rather_than_sealed_to() {
        let transport = UnifiedPush::new().unwrap();
        let device = device_with(
            "https://distributor.example/abc",
            DevicePublicKey::from_bytes([0_u8; 32]),
        );

        let outcome = transport.deliver(&device, &notification()).await.unwrap();
        assert_eq!(outcome, PushOutcome::Expired);
    }

    #[test]
    fn a_poke_carries_identifiers_and_no_ciphertext() {
        let key = device_key();
        let message = PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Poke,
        };

        let envelope = Envelope::sealed_for(&key, &message).unwrap();

        assert!(envelope.channel.is_some());
        assert!(envelope.notification_id.is_some());
        assert!(
            envelope.ciphertext_device.is_none(),
            "a poke has nothing to protect — the client fetches over TLS"
        );
    }

    #[test]
    fn a_payload_seals_and_a_digest_seals_and_neither_names_a_channel_for_the_digest() {
        let key = device_key();

        let from_notification = Envelope::sealed_for(&key, &notification()).unwrap();
        assert!(from_notification.channel.is_some());
        assert!(from_notification.ciphertext_device.is_some());

        let digest = Envelope::sealed_for(
            &key,
            &PushMessage::Digest {
                total: 12,
                failures: 2,
            },
        )
        .unwrap();
        assert!(
            digest.channel.is_none(),
            "a digest names no channel — it ends a quiet period, not one channel's"
        );
        assert!(digest.ciphertext_device.is_some());
    }

    #[test]
    fn outcomes_revoke_on_410_and_retry_on_429() {
        assert_eq!(outcome(200), PushOutcome::Delivered);
        assert_eq!(outcome(410), PushOutcome::Expired);
        assert_eq!(
            outcome(429),
            PushOutcome::Failed {
                reason: "rate_limited"
            }
        );
    }
}
