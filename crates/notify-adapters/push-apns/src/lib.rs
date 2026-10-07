//! APNs transport (design §6.2, §6.3, §10bis.5): token-based `.p8` auth, sealed-box payload.
//! Untested against real Apple infrastructure. Uses `mutable-content` so the Notification Service
//! Extension decrypts before display (`content-available` would not invoke it).

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify_domain::{Device, DevicePublicKey, Priority};
use notify_ports::{
    PortError, PortResult, PushMessage, PushOutcome, PushTransport, SEALED_ENVELOPE_VERSION,
};
use p256::pkcs8::DecodePrivateKey as _;
use serde::{Deserialize, Serialize};
use url::Url;

/// Max plaintext: Apple caps the whole JSON at 4096 bytes; minus `aps` wrapper, base64 and sealing, with margin.
const MAX_PAYLOAD: usize = 2_600;

/// `apns-expiration` window; same four hours as Web Push's `TTL`.
const TTL: Duration = Duration::from_secs(4 * 3_600);

/// Provider token reuse window: Apple accepts ~1h and asks not to mint one per push.
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

const APNS_PRODUCTION_HOST: &str = "https://api.push.apple.com";
const APNS_SANDBOX_HOST: &str = "https://api.sandbox.push.apple.com";

/// Why the transport could not be built: an operator mistake found at startup (design §11).
#[derive(Debug, thiserror::Error)]
pub enum ApnsSetup {
    /// Apple's Team ID is always exactly ten alphanumeric characters.
    #[error("[push.apns] team_id must be exactly 10 alphanumeric characters")]
    TeamId,
    /// Apple's Key ID is the same shape as the Team ID.
    #[error("[push.apns] key_id must be exactly 10 alphanumeric characters")]
    KeyId,
    /// The key is not one this build can read.
    #[error(
        "[push.apns] the key is not a P-256 private key (PKCS#8 PEM, as Apple's .p8 files are)"
    )]
    Key,
    /// The HTTP client could not be built.
    #[error("[push.apns] the HTTP client could not be built: {0}")]
    Client(#[source] reqwest::Error),
}

/// What identifies this server to APNs (design §11, `[push.apns]`).
#[derive(Debug, Clone)]
pub struct ApnsConfig {
    /// The `.p8` provider authentication key, PEM-encoded P-256 (PKCS#8).
    pub key_pem: String,
    /// The 10-character Key ID of the `.p8` key (JWT `kid`).
    pub key_id: String,
    /// The 10-character Apple Developer Team ID (JWT `iss`).
    pub team_id: String,
    /// The app's bundle identifier — sent as `apns-topic` on every request.
    pub topic: String,
    /// Push to `api.sandbox.push.apple.com` (debug-signed builds) instead of production.
    pub sandbox: bool,
}

/// Provider token claims: nothing per-push, so [`Apns::provider_token`] can cache one.
#[derive(Serialize)]
struct Provider<'a> {
    iss: &'a str,
    iat: u64,
}

/// APNs, over the wire.
pub struct Apns {
    http: reqwest::Client,
    signing: jsonwebtoken::EncodingKey,
    key_id: String,
    team_id: String,
    topic: String,
    host: &'static str,
    /// The last minted provider token and when; refreshed ~hourly, so an uncontended [`Mutex`].
    cached_token: Mutex<Option<(String, Instant)>>,
}

impl core::fmt::Debug for Apns {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Apns")
            .field("key_id", &self.key_id)
            .field("team_id", &self.team_id)
            .field("topic", &self.topic)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl Apns {
    /// Fails at startup if the team/key IDs are malformed or the key is not a readable P-256 key.
    pub fn new(config: &ApnsConfig) -> Result<Self, ApnsSetup> {
        let host = if config.sandbox {
            APNS_SANDBOX_HOST
        } else {
            APNS_PRODUCTION_HOST
        };
        Self::at(config, host)
    }

    /// [`Apns::new`] against an explicit host, so tests never address Apple.
    fn at(config: &ApnsConfig, host: &'static str) -> Result<Self, ApnsSetup> {
        if !is_apple_id(&config.team_id) {
            return Err(ApnsSetup::TeamId);
        }
        if !is_apple_id(&config.key_id) {
            return Err(ApnsSetup::KeyId);
        }

        let signing = jsonwebtoken::EncodingKey::from_ec_pem(config.key_pem.as_bytes())
            .map_err(|_| ApnsSetup::Key)?;
        // Parsed here so a malformed key fails at startup, not at the first signing.
        p256::SecretKey::from_pkcs8_pem(&config.key_pem).map_err(|_| ApnsSetup::Key)?;

        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(ApnsSetup::Client)?,
            signing,
            key_id: config.key_id.clone(),
            team_id: config.team_id.clone(),
            topic: config.topic.clone(),
            host,
            cached_token: Mutex::new(None),
        })
    }

    /// The `Authorization: bearer …` token, re-minted after [`TOKEN_TTL`].
    fn provider_token(&self) -> PortResult<String> {
        let now = Instant::now();

        if let Some((token, minted)) = self
            .cached_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            && now.duration_since(*minted) < TOKEN_TTL
        {
            return Ok(token.clone());
        }

        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| PortError::unavailable("push.apns", error))?
            .as_secs();

        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let claims = Provider {
            iss: &self.team_id,
            iat,
        };
        let token = jsonwebtoken::encode(&header, &claims, &self.signing)
            .map_err(|error| PortError::unavailable("push.apns", error))?;

        *self
            .cached_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((token.clone(), now));

        Ok(token)
    }
}

/// Alert shown until the extension replaces it; non-empty since Apple forbids removing alert text.
struct AlertPlaceholder {
    title: &'static str,
    body: &'static str,
}

impl Serialize for AlertPlaceholder {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a> {
            title: &'a str,
            body: &'a str,
        }
        Fields {
            title: self.title,
            body: self.body,
        }
        .serialize(serializer)
    }
}

const PLACEHOLDER_ALERT: AlertPlaceholder = AlertPlaceholder {
    title: " ",
    body: " ",
};

/// What tells iOS to hand this push to the Notification Service Extension before
/// showing anything (design §6.3, §10bis.5).
#[derive(Serialize)]
struct Aps {
    alert: &'static AlertPlaceholder,
    #[serde(rename = "mutable-content")]
    mutable_content: u8,
}

/// What crosses the wire to APNs (envelope v2, §6.3): `aps`, a version and the sealed box.
#[derive(Serialize)]
struct Envelope {
    aps: Aps,
    v: u8,
    ciphertext_device: String,
}

impl Envelope {
    /// Seal `message` to `key`; `None` for a weak key (see [`notify_sealed_box::seal`]).
    fn sealed_for(key: &DevicePublicKey, message: &PushMessage) -> Option<Self> {
        Some(Self {
            aps: Aps {
                alert: &PLACEHOLDER_ALERT,
                mutable_content: 1,
            },
            v: SEALED_ENVELOPE_VERSION,
            ciphertext_device: notify_sealed_box::seal_base64(key, &message.sealed_plaintext())?,
        })
    }
}

#[async_trait::async_trait]
impl PushTransport for Apns {
    fn name(&self) -> &'static str {
        "apns"
    }

    fn max_payload_bytes(&self) -> usize {
        MAX_PAYLOAD
    }

    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let notify_domain::PushTransport::Apns { token } = &device.transport else {
            // The registry dispatches by name, so this is a stored-transport mismatch.
            return Err(PortError::corrupt(
                "device.transport",
                "not an APNs registration",
            ));
        };

        // `Device::enrol` requires a key for relayed transports (§6.2), so `None` is corrupt state.
        let Some(public_key) = device.public_key else {
            return Err(PortError::corrupt(
                "device.public_key",
                "an APNs device enrolled with no key",
            ));
        };

        let Some(envelope) = Envelope::sealed_for(&public_key, message) else {
            return Ok(PushOutcome::Expired);
        };

        let body = serde_json::to_vec(&envelope)
            .map_err(|error| PortError::corrupt("push.envelope", error))?;
        let provider_token = self.provider_token()?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| PortError::unavailable("push.apns", error))?
            .as_secs();

        // `self.host` is a fixed `https://` base, so `path_segments_mut` cannot fail.
        #[allow(
            clippy::unwrap_used,
            reason = "the base URL is this crate's own https constant, always parseable and always a base"
        )]
        let url = {
            let mut url = Url::parse(self.host).unwrap();
            url.path_segments_mut()
                .unwrap()
                .extend(["3", "device", token]);
            url
        };

        let response = self
            .http
            .post(url)
            .header("authorization", format!("bearer {provider_token}"))
            .header("apns-topic", &self.topic)
            .header("apns-push-type", "alert")
            .header("apns-priority", apns_priority(message))
            .header("apns-expiration", (now + TTL.as_secs()).to_string())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;

        Ok(match response {
            Ok(response) => {
                let status = response.status().as_u16();
                if (200..=299).contains(&status) {
                    PushOutcome::Delivered
                } else {
                    // APNs folds distinct failures under one status; the body's `reason` tells them apart.
                    let reason = response
                        .json::<ApnsErrorBody>()
                        .await
                        .ok()
                        .map(|body| body.reason);
                    outcome(status, reason.as_deref())
                }
            }
            // Unreachable is a metric, not an `Err`.
            Err(_) => PushOutcome::Failed {
                reason: "unreachable",
            },
        })
    }
}

/// APNs' error body on a non-2xx response: `{"reason": "BadDeviceToken", ...}`.
#[derive(Debug, Deserialize)]
struct ApnsErrorBody {
    reason: String,
}

/// Map a failing (non-2xx) APNs status and `reason` to [`PushOutcome`].
/// `410` and token-invalid `400`s revoke; `PayloadTooLarge` is our bug and must not revoke.
fn outcome(status: u16, reason: Option<&str>) -> PushOutcome {
    match (status, reason) {
        (410, _) | (400, Some("BadDeviceToken" | "DeviceTokenNotForTopic")) => PushOutcome::Expired,
        (400, Some("PayloadTooLarge")) => PushOutcome::Failed {
            reason: "too_large",
        },
        (429, _) => PushOutcome::Failed {
            reason: "rate_limited",
        },
        (400..=499, _) => PushOutcome::Failed { reason: "rejected" },
        _ => PushOutcome::Failed {
            reason: "relay_unavailable",
        },
    }
}

/// `apns-priority`: digests are never urgent; APNs has no finer tier.
const fn apns_priority(message: &PushMessage) -> &'static str {
    match message.priority() {
        Priority::Min | Priority::Low => "5",
        Priority::Default | Priority::High | Priority::Urgent => "10",
    }
}

/// Whether a value is ten ASCII alphanumerics, the shape of an Apple Team or Key ID.
fn is_apple_id(value: &str) -> bool {
    value.len() == 10
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

/// A deterministic ES256 test key, derived from a fixed scalar so no PEM literal trips secret scanners.
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn test_apns_key() -> String {
    use p256::pkcs8::EncodePrivateKey as _;

    #[allow(
        clippy::expect_used,
        reason = "a constant scalar that is a valid P-256 key, or this crate cannot \
                  encode the keys it is built to read"
    )]
    p256::SecretKey::from_slice(&[0x2a; 32])
        .expect("a fixed, valid P-256 scalar")
        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
        .expect("PKCS#8 encoding of a key this crate just built")
        .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{ChannelId, DeviceId, NotificationId, Principal, UserId};
    use notify_ports::PushBody;
    use time::OffsetDateTime;
    use x25519_dalek::{PublicKey, StaticSecret};

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    fn config() -> ApnsConfig {
        ApnsConfig {
            key_pem: test_apns_key(),
            key_id: "ABCDE12345".to_owned(),
            team_id: "TEAM123456".to_owned(),
            topic: "poc.weebo.notify".to_owned(),
            sandbox: false,
        }
    }

    fn device_key() -> DevicePublicKey {
        let secret = StaticSecret::random();
        DevicePublicKey::from_bytes(*PublicKey::from(&secret).as_bytes())
    }

    fn device_with(token: &str, key: DevicePublicKey) -> Device {
        Device::enrol(
            DeviceId::generate(),
            Principal::User {
                id: UserId::generate(),
            },
            "iphone",
            notify_domain::PushTransport::Apns {
                token: token.to_owned(),
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
    fn a_team_id_the_wrong_shape_is_refused_at_startup() {
        for team_id in ["short", "waytoolongforanappleteamid", ""] {
            let bad = ApnsConfig {
                team_id: team_id.to_owned(),
                ..config()
            };
            assert!(
                matches!(Apns::new(&bad), Err(ApnsSetup::TeamId)),
                "`{team_id}` was accepted"
            );
        }
    }

    #[test]
    fn a_key_id_the_wrong_shape_is_refused_at_startup() {
        let bad = ApnsConfig {
            key_id: "not-ten-chars".to_owned(),
            ..config()
        };
        assert!(matches!(Apns::new(&bad), Err(ApnsSetup::KeyId)));
    }

    #[test]
    fn a_key_that_is_not_one_is_refused_at_startup_and_not_at_the_first_push() {
        let bad = ApnsConfig {
            key_pem: "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n".to_owned(),
            ..config()
        };
        assert!(matches!(Apns::new(&bad), Err(ApnsSetup::Key)));
    }

    #[test]
    fn a_valid_config_builds() {
        assert!(Apns::new(&config()).is_ok());
    }

    #[test]
    fn sandbox_selects_the_sandbox_host() {
        let production = Apns::new(&config()).unwrap();
        let sandbox = Apns::new(&ApnsConfig {
            sandbox: true,
            ..config()
        })
        .unwrap();

        assert_eq!(production.host, APNS_PRODUCTION_HOST);
        assert_eq!(sandbox.host, APNS_SANDBOX_HOST);
    }

    #[test]
    fn the_provider_token_is_cached_rather_than_minted_per_call() {
        let transport = Apns::new(&config()).unwrap();

        let first = transport.provider_token().unwrap();
        let second = transport.provider_token().unwrap();

        assert_eq!(first, second, "reused within `TOKEN_TTL`, not re-signed");
    }

    #[test]
    fn the_provider_token_names_the_team_and_the_key() {
        let transport = Apns::new(&config()).unwrap();
        let token = transport.provider_token().unwrap();

        let header: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token.split('.').next().unwrap())
                .unwrap(),
        )
        .unwrap();
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();

        assert_eq!(header["kid"], "ABCDE12345");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(claims["iss"], "TEAM123456");
        assert!(claims["iat"].is_u64());
    }

    #[tokio::test]
    async fn a_device_whose_stored_transport_disagrees_with_its_name_is_corrupt_not_failed() {
        let transport = Apns::new(&config()).unwrap();
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
        let transport = Apns::new(&config()).unwrap();
        let device = device_with(
            "a".repeat(64).as_str(),
            DevicePublicKey::from_bytes([0_u8; 32]),
        );

        let outcome = transport.deliver(&device, &notification()).await.unwrap();
        assert_eq!(outcome, PushOutcome::Expired);
    }

    #[test]
    fn apple_sees_the_placeholder_alert_a_version_and_ciphertext_and_no_identifier() {
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

            assert_eq!(value["aps"]["mutable-content"], 1);
            assert_eq!(value["aps"]["alert"]["title"], " ");
            assert_eq!(value["v"], 2);
            assert!(value["ciphertext_device"].is_string());
            assert_eq!(value.as_object().unwrap().len(), 3, "{rendered}");
            assert!(!rendered.contains("ci-infra"), "{rendered}");
        }
    }

    #[test]
    fn priority_maps_every_value_the_domain_has_onto_apples_two() {
        for (priority, expected) in [
            (Priority::Min, "5"),
            (Priority::Low, "5"),
            (Priority::Default, "10"),
            (Priority::High, "10"),
            (Priority::Urgent, "10"),
        ] {
            let message = PushMessage::Notification {
                channel: ChannelId::parse("ci-infra").unwrap(),
                notification_id: NotificationId::generate(),
                priority,
                body: PushBody::Poke,
            };
            assert_eq!(apns_priority(&message), expected);
        }
    }

    #[test]
    fn outcomes_revoke_on_dead_tokens_and_not_on_our_own_oversized_payload() {
        // 2xx never reaches `outcome()`.
        assert_eq!(outcome(410, Some("Unregistered")), PushOutcome::Expired);
        assert_eq!(outcome(400, Some("BadDeviceToken")), PushOutcome::Expired);
        assert_eq!(
            outcome(400, Some("DeviceTokenNotForTopic")),
            PushOutcome::Expired
        );
        assert!(matches!(
            outcome(400, Some("PayloadTooLarge")),
            PushOutcome::Failed {
                reason: "too_large"
            }
        ));
        assert!(matches!(
            outcome(429, None),
            PushOutcome::Failed {
                reason: "rate_limited"
            }
        ));
        assert!(matches!(
            outcome(403, Some("InvalidProviderToken")),
            PushOutcome::Failed { reason: "rejected" }
        ));
        assert!(matches!(
            outcome(500, None),
            PushOutcome::Failed {
                reason: "relay_unavailable"
            }
        ));
        assert!(
            matches!(
                outcome(400, None),
                PushOutcome::Failed { reason: "rejected" }
            ),
            "an unreadable body degrades to status-only rather than failing the delivery"
        );
    }

    #[tokio::test]
    async fn a_relay_that_cannot_be_reached_is_a_metric_and_not_an_error() {
        // Nothing listens there: fails at the socket after signing and sealing ran.
        let transport = Apns::at(&config(), "http://127.0.0.1:1").unwrap();
        let device = device_with(&"a".repeat(64), device_key());

        let outcome = transport.deliver(&device, &notification()).await.unwrap();

        assert!(
            matches!(
                outcome,
                PushOutcome::Failed {
                    reason: "unreachable"
                }
            ),
            "a dropped connection is not this transport being unusable"
        );
    }
}
