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
use notify_domain::{Device, DevicePublicKey};
use notify_ports::{
    PortError, PortResult, PushDispatcher, PushMessage, PushOutcome, PushTransport,
    SEALED_ENVELOPE_VERSION,
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
const MAX_PAYLOAD: usize = 2_750;

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

/// What crosses the wire to a distributor endpoint (design §6.3, envelope v2).
///
/// A version and the sealed box, nothing else. Channel and notification ids used
/// to travel in the clear here (v1); a third-party distributor — a public ntfy —
/// could then map who follows what without ever opening a box. They now ride
/// inside it ([`PushMessage::sealed_plaintext`]), pokes included.
#[derive(Debug, Serialize)]
struct Envelope {
    v: u8,
    /// The sealed box, base64.
    ciphertext_device: String,
}

impl Envelope {
    /// Seal what the fan-out decided to send to `key`.
    ///
    /// `None` propagates a weak recipient key (design's threat model trusts an
    /// enrolled device, but not blindly — see [`notify_sealed_box::seal`]) rather
    /// than sending something a device could not trust.
    fn sealed_for(key: &DevicePublicKey, message: &PushMessage) -> Option<Self> {
        Some(Self {
            v: SEALED_ENVELOPE_VERSION,
            ciphertext_device: seal_base64(key, &message.sealed_plaintext())?,
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
            // The endpoint is the device's choice, so the client refuses what
            // `check_push_endpoint` refuses, once more after DNS: a redirect or a
            // name resolving inside the network is the same request forgery as an
            // IP literal.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(PublicOnly)
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

        // Enrolment checked it, but a row written before that check existed did not
        // go through it — and an IP literal never reaches the resolver below.
        if notify_domain::check_push_endpoint(endpoint).is_err() {
            return Ok(PushOutcome::Expired);
        }

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

/// A resolver that only ever hands back public addresses.
///
/// ponytail: duplicated in `notify-push-webpush`; a shared crate if a third
/// transport ever dials an address a user picked.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|addr| notify_domain::is_public_address(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("the push endpoint resolves to no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
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

    use notify_domain::{ChannelId, DeviceId, NotificationId, Principal, Priority, UserId};
    use notify_ports::PushBody;
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
    fn the_relay_sees_a_version_and_ciphertext_and_no_identifier() {
        let key = device_key();
        let poke = PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Poke,
        };
        let digest = PushMessage::Digest {
            total: 12,
            failures: 2,
        };

        for message in [notification(), poke, digest] {
            let rendered =
                serde_json::to_string(&Envelope::sealed_for(&key, &message).unwrap()).unwrap();
            let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

            assert_eq!(value["v"], 2);
            assert!(value["ciphertext_device"].is_string());
            assert_eq!(value.as_object().unwrap().len(), 2, "{rendered}");
            assert!(!rendered.contains("ci-infra"), "{rendered}");
        }
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
