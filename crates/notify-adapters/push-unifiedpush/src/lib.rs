//! UnifiedPush transport (design §6.2, §7.1, §10bis.4): POST a sealed envelope to the device's distributor endpoint.
//! Sealing is this transport's job (`notify_sealed_box`), identical for external and embedded distributors.
//! Outbound only; there is no inbound distributor route in §8.

use notify_domain::{Device, DevicePublicKey};
use notify_ports::{
    PortError, PortResult, PushDispatcher, PushMessage, PushOutcome, PushTransport,
    SEALED_ENVELOPE_VERSION,
};
use notify_push_http::{PublicOnly, send_retrying};
use serde::Serialize;

/// The largest wire body (whole JSON envelope, sealing and base64 included) a distributor
/// must accept per the spec. Measured, not estimated: over it, the poke goes ([`body_for`]).
const MAX_BODY: usize = 4_096;

/// Why the transport could not be built: an operator configuration problem found at startup (design §11).
#[derive(Debug, thiserror::Error)]
pub enum UnifiedPushSetup {
    /// The HTTP client could not be built.
    #[error("[push.unifiedpush] the HTTP client could not be built: {0}")]
    Client(#[source] reqwest::Error),
}

/// What a distributor sees (envelope v2, §6.3): a version and the sealed box; ids ride inside it.
#[derive(Debug, Serialize)]
struct Envelope {
    v: u8,
    /// The sealed box, base64.
    ciphertext_device: String,
}

impl Envelope {
    /// Seal `message` to `key`; `None` for a weak key (see [`notify_sealed_box::seal`]).
    fn sealed_for(key: &DevicePublicKey, message: &PushMessage) -> Option<Self> {
        Some(Self {
            v: SEALED_ENVELOPE_VERSION,
            ciphertext_device: notify_sealed_box::seal_base64(
                key,
                SEALED_ENVELOPE_VERSION,
                &message.sealed_plaintext(),
            )?,
        })
    }
}

/// The wire body for `message`, or for its poke when that is over [`MAX_BODY`].
/// `None` for a key nothing can be sealed to, as [`Envelope::sealed_for`].
fn body_for(key: &DevicePublicKey, message: &PushMessage) -> PortResult<Option<Vec<u8>>> {
    let encode = |message: &PushMessage| {
        Envelope::sealed_for(key, message)
            .map(|envelope| serde_json::to_vec(&envelope))
            .transpose()
            .map_err(|error| PortError::corrupt("push.envelope", error))
    };
    match encode(message)? {
        Some(body) if body.len() > MAX_BODY => encode(&message.as_poke()),
        fitted => Ok(fitted),
    }
}

/// UnifiedPush, over the wire.
#[derive(Debug)]
pub struct UnifiedPush {
    http: reqwest::Client,
}

impl UnifiedPush {
    /// Fails only if the HTTP client cannot be built.
    pub fn new() -> Result<Self, UnifiedPushSetup> {
        Ok(Self {
            // SSRF guard: the endpoint is device-chosen, so no redirects and public IPs only.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                // A proxy would resolve the name itself, and `PublicOnly` would
                // never be asked: `HTTPS_PROXY` in the environment must not be a
                // way around it.
                .no_proxy()
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
        MAX_BODY
    }

    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let notify_domain::PushTransport::UnifiedPush { endpoint } = &device.transport else {
            // The registry dispatches by name, so this is a stored-transport mismatch.
            return Err(PortError::corrupt(
                "device.transport",
                "not a UnifiedPush registration",
            ));
        };

        // Re-checked for rows predating enrolment validation; IP literals bypass the resolver.
        if notify_domain::check_push_endpoint(endpoint).is_err() {
            return Ok(PushOutcome::Expired);
        }

        // `Device::enrol` requires a key for relayed transports (§6.2), so `None` is corrupt state.
        let Some(public_key) = device.public_key else {
            return Err(PortError::corrupt(
                "device.public_key",
                "a UnifiedPush device enrolled with no key",
            ));
        };

        // A non-contributory key never recovers: expire (revoke) rather than retry.
        let Some(body) = body_for(&public_key, message)? else {
            return Ok(PushOutcome::Expired);
        };

        let response = send_retrying(
            self.http
                .post(endpoint.clone())
                .header("Content-Type", "application/json")
                .body(body),
        )
        .await;

        Ok(match response {
            Some(response) => outcome(response.status().as_u16()),
            // Unreachable is a metric, not an `Err` (which means the transport is unusable).
            None => PushOutcome::Failed {
                reason: "unreachable",
            },
        })
    }
}

/// UnifiedPush as a dispatcher, so tests reach it through the fan-out's trait.
#[async_trait::async_trait]
impl PushDispatcher for UnifiedPush {
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        self.deliver(device, message).await
    }
}

/// Map a distributor status to [`PushOutcome`]; `404`/`410` mean revoke, not retry.
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

            assert_eq!(value["v"], 3);
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

    fn titled(len: usize) -> PushMessage {
        PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Payload {
                bytes: serde_json::to_vec(&serde_json::json!({ "title": "x".repeat(len) }))
                    .unwrap(),
                sealed: false,
            },
        }
    }

    #[test]
    fn the_largest_content_that_fits_is_sent_whole_and_one_byte_more_is_a_poke() {
        let key = device_key();
        let full = |len: usize| {
            serde_json::to_vec(&Envelope::sealed_for(&key, &titled(len)).unwrap())
                .unwrap()
                .len()
        };
        // The longest title whose full envelope is still within the ceiling —
        // base64 grows in steps, so the line is found rather than computed.
        let fits = (0..MAX_BODY)
            .rev()
            .find(|&len| full(len) <= MAX_BODY)
            .unwrap();

        let at = body_for(&key, &titled(fits)).unwrap().unwrap();
        assert_eq!(at.len(), full(fits), "sent with its content");
        assert!(at.len() <= MAX_BODY);

        let over = body_for(&key, &titled(fits + 1)).unwrap().unwrap();
        assert!(full(fits + 1) > MAX_BODY);
        let poke = titled(fits + 1).as_poke();
        assert_eq!(
            over.len(),
            serde_json::to_vec(&Envelope::sealed_for(&key, &poke).unwrap())
                .unwrap()
                .len(),
            "identifiers only; the device fetches the rest over TLS"
        );
    }
}
