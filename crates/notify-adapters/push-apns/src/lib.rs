//! APNs: token-based `.p8` authentication, and the sealed box relayed transports
//! share (design §6.2, §6.3, §10bis.5, M7).
//!
//! # The server half of a milestone whose other half cannot exist here
//!
//! M7's Swift side — the Notification Service Extension that decrypts before
//! display, Keychain X25519 enrollment, the whole iOS build — needs Xcode, which
//! cannot run in this project's Linux infrastructure at all (design §14.4, D34;
//! unlike D33's Android gap, there is no image this could ever land in). This
//! transport is the other half: it has a real caller (the fan-out, via
//! [`notify_ports::PushTransport`]) and talks to a stable, publicly documented
//! Apple API, neither of which needs the Swift side to exist first — the same
//! reasoning that let `notify-push-unifiedpush` (M6) ship its outbound half with no
//! Android build in reach either. **Tested everywhere, never against real Apple
//! infrastructure**: nothing here has ever received a `200` from
//! `api.push.apple.com`, the same honesty `notify-push-unifiedpush`'s own module doc
//! states for UnifiedPush.
//!
//! # `mutable-content`, not `content-available`
//!
//! A Notification Service Extension only runs *before display* — the whole point,
//! since it is what decrypts [`notify_sealed_box`]'s ciphertext — when the payload
//! carries `"aps":{"alert":{...},"mutable-content":1}`. A plain `content-available`
//! background push does not invoke it for that purpose; getting this wrong would
//! mean the feature silently does nothing the day the Swift half exists to receive
//! it. Apple also forbids the extension from *removing* the alert text, only
//! replacing it, so [`AlertPlaceholder`] is not empty — it is what a device sees for
//! the instant before the extension overwrites it with the decrypted title.
//!
//! # The payload budget, and why it differs from UnifiedPush's
//!
//! Apple's hard cap is 4096 bytes for the **entire** JSON payload, stricter than Web
//! Push's 4096-*encrypted-body-only*. See [`MAX_PAYLOAD`] for the arithmetic —
//! conservative by the same deliberate margin `notify-push-unifiedpush`'s own budget
//! is, and a little smaller than it, because this envelope also carries the `aps`/
//! `mutable-content` wrapper UnifiedPush's does not.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notify_domain::{Device, DevicePublicKey, Priority};
use notify_ports::{
    PortError, PortResult, PushDispatcher, PushMessage, PushOutcome, PushTransport,
    SEALED_ENVELOPE_VERSION,
};
use p256::pkcs8::DecodePrivateKey as _;
use serde::{Deserialize, Serialize};
use url::Url;

/// The most plaintext one push may carry.
///
/// Apple's limit is 4096 bytes for the whole JSON body, not just `aps`. The fixed
/// skeleton — `{"aps":{"alert":{"title":" ","body":" "},"mutable-content":1},
/// "v":2,"ciphertext_device":""}` — is under 100 bytes; rounding up for safety
/// leaves ~250 bytes of fixed overhead. The remaining ~3846 bytes for the base64'd
/// (`URL_SAFE_NO_PAD`, no JSON-escaping inflation) `ciphertext_device` back out to
/// ~2884 raw sealed bytes, minus the sealed box's own 72-byte overhead (32-octet
/// ephemeral key, 24-octet nonce, 16-octet tag) and the ~150-byte sealed wrapper
/// around the content (kind, channel, notification id — envelope v2) ≈ 2660 raw
/// content. Rounded down, deliberately a little under `notify-push-unifiedpush`'s
/// budget: this envelope carries the `aps` wrapper that one does not. Revisit once a real device
/// gives a number worth citing instead of a margin.
const MAX_PAYLOAD: usize = 2_600;

/// How long APNs may hold a push before giving up on it, sent as `apns-expiration`.
///
/// Same four hours and the same reasoning `notify-push-webpush`'s `TTL` states: a
/// notification nobody has seen after four hours is one the inbox shows when the
/// app next opens, not news worth arriving half a day late.
const TTL: Duration = Duration::from_secs(4 * 3_600);

/// How long a minted provider token is reused before signing another.
///
/// Apple accepts a token for up to roughly an hour and asks integrators not to mint
/// one per push. Unlike VAPID (`notify-push-webpush` signs fresh every time because
/// the `aud` claim is per-push-service-origin), nothing in an APNs provider token's
/// claims varies per request, so caching costs nothing and minting one per delivery
/// would just be needless signing on a request's hot path.
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

const APNS_PRODUCTION_HOST: &str = "https://api.push.apple.com";
const APNS_SANDBOX_HOST: &str = "https://api.sandbox.push.apple.com";

/// Why the transport could not be built (design §11, `[push.apns]`).
///
/// Its own type rather than a [`PortError`], matching `notify-push-webpush`: an
/// operator's mistake found at startup, not a category the fan-out has to classify.
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
    /// The 10-character Key ID Apple assigned this `.p8` key — becomes the JWT
    /// `kid` header.
    pub key_id: String,
    /// The 10-character Apple Developer Team ID — becomes the JWT `iss` claim.
    pub team_id: String,
    /// The app's bundle identifier — sent as `apns-topic` on every request.
    pub topic: String,
    /// Whether to push against the sandbox environment (`api.sandbox.push.apple.com`,
    /// what a debug-signed build's device tokens are valid against) instead of
    /// production. A real deployment needs both eventually — one instance per
    /// environment, like any other `[push.apns]` credential — not a per-device
    /// toggle, since which environment a token is valid for is fixed at the app's
    /// own signing, not something this server observes.
    pub sandbox: bool,
}

/// The APNs provider token's claims — `iss`/`iat` only, nothing that varies per
/// push, which is exactly why [`Apns::provider_token`] can cache one.
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
    /// The last minted provider token and when, behind a plain [`Mutex`]: refreshed
    /// roughly hourly on a low-frequency path, not a contended one — see
    /// [`Apns::provider_token`].
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
    /// Build one from the configured key.
    ///
    /// # Errors
    ///
    /// Fails if the team/key IDs are not the shape Apple assigns, or the key is not
    /// a P-256 private key this build can read — all at startup rather than at the
    /// first push, the same rule `notify-push-webpush`'s constructor states.
    pub fn new(config: &ApnsConfig) -> Result<Self, ApnsSetup> {
        let host = if config.sandbox {
            APNS_SANDBOX_HOST
        } else {
            APNS_PRODUCTION_HOST
        };
        Self::at(config, host)
    }

    /// The same construction, against an explicit host — [`Apns::new`]'s own real
    /// path, and what the tests use to prove the unreachable-relay path without
    /// ever addressing Apple's real edge.
    fn at(config: &ApnsConfig, host: &'static str) -> Result<Self, ApnsSetup> {
        if !is_apple_id(&config.team_id) {
            return Err(ApnsSetup::TeamId);
        }
        if !is_apple_id(&config.key_id) {
            return Err(ApnsSetup::KeyId);
        }

        let signing = jsonwebtoken::EncodingKey::from_ec_pem(config.key_pem.as_bytes())
            .map_err(|_| ApnsSetup::Key)?;
        // Read a second time, purely so a malformed key is caught here rather than
        // waiting for `jsonwebtoken` to fail it at the first sign — mirrors
        // `notify-push-webpush`'s own `read_key` pass for the same reason.
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

    /// The `Authorization: bearer …` token, minted fresh past [`TOKEN_TTL`] and
    /// reused before it.
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

/// The placeholder every push carries before the Notification Service Extension
/// overwrites it. Non-empty on purpose: Apple lets the extension replace the alert
/// text, never remove it, so an empty string would be a payload the extension is
/// not allowed to fully correct.
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

/// What crosses the wire to APNs (design §6.3, envelope v2): the `aps` wrapper, a
/// version and the sealed box. The channel and notification ids ride inside the
/// box ([`PushMessage::sealed_plaintext`]), pokes included — Apple routes by
/// device token and needs neither.
#[derive(Serialize)]
struct Envelope {
    aps: Aps,
    v: u8,
    ciphertext_device: String,
}

impl Envelope {
    /// Seal what the fan-out decided to send to `key`. `None` propagates a weak
    /// recipient key, the same `notify-push-unifiedpush::Envelope::sealed_for` does
    /// and for the same reason.
    fn sealed_for(key: &DevicePublicKey, message: &PushMessage) -> Option<Self> {
        Some(Self {
            aps: Aps {
                alert: &PLACEHOLDER_ALERT,
                mutable_content: 1,
            },
            v: SEALED_ENVELOPE_VERSION,
            ciphertext_device: seal_base64(key, &message.sealed_plaintext())?,
        })
    }
}

fn seal_base64(key: &DevicePublicKey, plaintext: &[u8]) -> Option<String> {
    notify_sealed_box::seal(key, plaintext).map(|sealed| URL_SAFE_NO_PAD.encode(sealed))
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
            // The registry dispatches by name, so reaching here means a device whose
            // stored transport disagrees with the name it enrolled under.
            return Err(PortError::corrupt(
                "device.transport",
                "not an APNs registration",
            ));
        };

        // `Device::enrol` refuses a relayed transport with no key (design §6.2), so
        // a `None` here is stored state disagreeing with an invariant the domain is
        // supposed to hold for it — the same corruption `notify-push-unifiedpush`
        // reports the same way.
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

        // `self.host` is one of this crate's own two constants and
        // `path_segments_mut` only refuses a URL that cannot be a base, which no
        // `https://` URL is.
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
                    // Unlike a relay that spells every failure in its status code,
                    // APNs folds distinct failures under the same one (`400` covers
                    // both "the device is gone" and "our own payload was too big"),
                    // with the real distinction only in the body — reading it is
                    // what keeps a live device from being misclassified as expired.
                    let reason = response
                        .json::<ApnsErrorBody>()
                        .await
                        .ok()
                        .map(|body| body.reason);
                    outcome(status, reason.as_deref())
                }
            }
            // APNs was unreachable. A routine outcome on a flaky network and a
            // metric rather than an error, the same distinction the sibling
            // transports draw.
            Err(_) => PushOutcome::Failed {
                reason: "unreachable",
            },
        })
    }
}

/// APNs as a dispatcher, so this crate's own tests reach it through the trait the
/// fan-out uses — the same reason the sibling transports have one.
#[async_trait::async_trait]
impl PushDispatcher for Apns {
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        self.deliver(device, message).await
    }
}

/// APNs' error body on a non-2xx response: `{"reason": "BadDeviceToken", ...}`.
/// Other fields (`timestamp`, present only on `410`) are not read — nothing here
/// yet acts on *when* a token became invalid, only that it did.
#[derive(Debug, Deserialize)]
struct ApnsErrorBody {
    reason: String,
}

/// How a **failing** APNs status and, where it matters, its `reason` map onto
/// [`PushOutcome`].
///
/// Only ever called for a non-2xx response — [`Apns::deliver`] answers `Delivered`
/// itself without reading the body, so this has no `2xx` arm of its own and treats
/// one as `relay_unavailable` if it were ever asked, which it is not in practice.
///
/// `410` is always a dead token. `400` is not always the same thing — `BadDeviceToken`
/// and `DeviceTokenNotForTopic` mean the token will never work here again (revoke,
/// same as `410`); `PayloadTooLarge` is this transport's own bug, not the device's,
/// and must not revoke a live subscription. A body that fails to parse degrades to
/// status-only, rather than failing the whole delivery over an unreadable error.
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

/// How loudly to ask APNs to wake the device.
///
/// Apple accepts only two values. A digest is deliberately never urgent — the same
/// rule `notify-push-webpush`'s `urgency` states — and everything else asks for
/// immediate delivery: unlike a browser's push service, APNs has no low-power
/// "whenever is convenient" tier worth distinguishing further.
const fn apns_priority(message: &PushMessage) -> &'static str {
    match message.priority() {
        Priority::Min | Priority::Low => "5",
        Priority::Default | Priority::High | Priority::Urgent => "10",
    }
}

/// Whether a value is the shape Apple assigns a Team ID or a Key ID: exactly ten
/// ASCII alphanumeric characters.
fn is_apple_id(value: &str) -> bool {
    value.len() == 10
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

/// A deterministic ES256 key for tests, built rather than pasted — the same reason
/// and the same construction `notify-push-webpush::test_vapid_key` uses: a PEM
/// literal in a source file is exactly what `task audit` (trivy) and the gitleaks
/// pre-commit hook exist to catch, and a fixed scalar is valid, deterministic, and
/// not a secret in any sense a scanner could confuse with one.
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
        // 2xx never reaches `outcome()` — `Apns::deliver` answers `Delivered`
        // itself before it would — so there is no `200` case to assert here.
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
        // A port nothing listens on, same as the sibling transports' own version of
        // this test: the request fails at the socket, which also means signing the
        // provider token and sealing the payload both ran for real above it.
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
