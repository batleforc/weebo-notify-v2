//! Web Push: RFC 8030 delivery, RFC 8291 encryption, RFC 8292 identification
//! (design §6.2, §6.3, §7.1).
//!
//! The first production transport, and the one with the least crypto to write —
//! which is the point of §6.2's table. On this path the payload is protected by
//! **RFC 8291 alone**: the browser holds the key pair internally and never exposes
//! it to JavaScript, so there is no application key on the web at all. The sealed
//! box of §6.2 exists only for the relayed transports (UnifiedPush, APNs), where a
//! third party we do not control carries the bytes and the browser's guarantee does
//! not apply.
//!
//! # What is taken from a library, and why that library
//!
//! The content encryption is `web-push-native`'s — `aes128gcm`, ECDH over P-256,
//! HKDF — taken **without its `vapid` feature**. Two reasons, and the first decided
//! it: the obvious crate, `web-push`, reaches `ece` which reaches **OpenSSL**, the C
//! toolchain this workspace refused for the server in M1 so that the image builds
//! from a plain `cargo build` and the Tauri cross-compilations of M5–M7 stay
//! possible. `web-push-native` is RustCrypto throughout. The second is that its
//! `vapid` feature pulls a second JWT library into a workspace that already has one;
//! the identification header below is signed with `jsonwebtoken`, the same crate
//! `notify-oidc` verifies with.
//!
//! It costs one thing, written here rather than discovered later: `web-push-native`
//! is built against **p256 0.13** while the workspace is on 0.14, so this crate
//! pins 0.13 for the types it hands across that boundary. Both versions were already
//! in the lockfile, so nothing new is duplicated — but a future upgrade has to move
//! them together.
//!
//! # The payload is a contract with the service worker
//!
//! Nothing in the OpenAPI document describes a push payload — it is not an HTTP
//! response — so [`Envelope`] is where that contract lives. A service worker
//! switches on `kind` and renders. Changing it is a wire break with every installed
//! client, exactly as changing §8.1 would be.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notify_domain::{ChannelId, Device, NotificationId, Priority};
use notify_ports::{
    PortError, PortResult, PushBody, PushDispatcher, PushMessage, PushOutcome, PushTransport,
};
use p256::pkcs8::DecodePrivateKey as _;
use serde::Serialize;

/// The most plaintext one push may carry.
///
/// RFC 8291 guarantees a push service accepts at least 4096 octets of *encrypted*
/// payload. `aes128gcm` frames that with 86 octets — a 16-octet salt, the 4-octet
/// record size, the 1-octet key-id length, the 65-octet ephemeral public key and the
/// 16-octet tag — plus one octet of padding delimiter, leaving 4009. Rounded down,
/// because being one byte over the line costs a `413` from a relay that will not
/// explain itself, and the caller degrades to a poke long before it matters.
const MAX_PAYLOAD: usize = 4_000;

/// How long a push may sit in the service's queue before it is dropped.
///
/// Four hours rather than the twelve most examples use: a notification nobody has
/// seen after four hours is one the inbox will show when the app next opens, and a
/// push arriving half a day late is noise pretending to be news.
const TTL: Duration = Duration::from_secs(4 * 3_600);

/// How long a VAPID signature stays valid.
///
/// RFC 8292 caps this at 24 hours. Twelve is used here so a clock a few hours out
/// still produces something a relay accepts, without minting a token that outlives
/// the day it was issued.
const VAPID_VALIDITY: Duration = Duration::from_secs(12 * 3_600);

/// Why a transport could not be built (design §11, `[push.webpush]`).
///
/// Its own type rather than a [`PortError`]: this is an operator's mistake found at
/// startup, and the composition root wants a sentence to print, not a category the
/// fan-out would have to classify.
#[derive(Debug, thiserror::Error)]
pub enum WebPushSetup {
    /// The contact is not a scheme a push service accepts.
    #[error("[push.webpush] contact must be a `mailto:` or `https://` URI (RFC 8292 `sub`)")]
    Contact,
    /// The key is not one this build can read.
    #[error("[push.webpush] the VAPID key is not a P-256 private key (PKCS#8 or SEC1 PEM)")]
    Key,
    /// The HTTP client could not be built.
    #[error("[push.webpush] the HTTP client could not be built: {0}")]
    Client(#[source] reqwest::Error),
}

/// What a service worker receives, once the browser has decrypted it.
///
/// Tagged, and the tag is the same word [`PushMessage`] uses: a client that can read
/// the stream reads this with the same `switch`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Envelope {
    /// Something happened in a channel.
    Notification {
        /// Which channel, so a client can route it before fetching anything.
        channel: ChannelId,
        /// Which notification.
        notification_id: NotificationId,
        /// The content, when it fitted. Absent means this was a poke: wake up, fetch
        /// over TLS, then display (design §6.3).
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
    },
    /// The one push that ends a quiet period (design §7.4).
    Digest {
        /// How many were held back.
        total: u32,
        /// How many of those closed on a failure.
        failures: u32,
    },
}

impl Envelope {
    /// Build one from what the fan-out decided to send.
    fn of(message: &PushMessage) -> Self {
        match message {
            PushMessage::Notification {
                channel,
                notification_id,
                body,
                ..
            } => Self::Notification {
                channel: channel.clone(),
                notification_id: *notification_id,
                content: match body {
                    // Spliced back as JSON rather than nested as an array of
                    // integers: the bytes *are* a serialised `Content`, and a client
                    // reading `content.title` should not have to reassemble it.
                    PushBody::Payload { bytes, .. } => serde_json::from_slice(bytes).ok(),
                    PushBody::Poke => None,
                },
            },
            PushMessage::Digest { total, failures } => Self::Digest {
                total: *total,
                failures: *failures,
            },
        }
    }
}

/// What identifies this server to a push service (design §11, `[push.webpush]`).
#[derive(Debug, Clone)]
pub struct WebPushConfig {
    /// The VAPID private key, PEM-encoded P-256.
    pub private_key_pem: String,
    /// Who to contact about this server: `mailto:…` or `https://…`.
    ///
    /// RFC 8292 makes it the `sub` claim, and push services refuse a signature
    /// without one — it is how a relay reaches an operator whose server has started
    /// misbehaving, so there is no sensible default to invent.
    pub contact: String,
}

/// Web Push, over the wire.
pub struct WebPush {
    http: reqwest::Client,
    signing: jsonwebtoken::EncodingKey,
    /// The uncompressed public point, base64url — the `k=` of the VAPID header and
    /// the `applicationServerKey` a browser subscribes with. Rendered once.
    public_key: String,
    contact: String,
}

impl core::fmt::Debug for WebPush {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WebPush")
            .field("public_key", &self.public_key)
            .field("contact", &self.contact)
            .finish_non_exhaustive()
    }
}

impl WebPush {
    /// Build one from the configured key.
    ///
    /// # Errors
    ///
    /// Fails if the key is not a P-256 private key this build can read, or the
    /// contact is not a scheme a push service accepts — both at startup rather than
    /// at the first push. An operator finding out about a malformed key because
    /// somebody's phone stayed silent is the failure this refuses.
    pub fn new(config: &WebPushConfig) -> Result<Self, WebPushSetup> {
        if !config.contact.starts_with("mailto:") && !config.contact.starts_with("https://") {
            return Err(WebPushSetup::Contact);
        }

        // Read twice on purpose, and it is cheap: `jsonwebtoken` signs, `p256`
        // renders the public point, and neither exposes what the other needs.
        let secret = read_key(&config.private_key_pem)?;
        let signing = jsonwebtoken::EncodingKey::from_ec_pem(config.private_key_pem.as_bytes())
            .map_err(|_| WebPushSetup::Key)?;

        use p256::elliptic_curve::sec1::ToEncodedPoint as _;
        let point = secret.public_key().to_encoded_point(false);

        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(WebPushSetup::Client)?,
            signing,
            public_key: URL_SAFE_NO_PAD.encode(point.as_bytes()),
            contact: config.contact.clone(),
        })
    }

    /// The `applicationServerKey` a browser needs to subscribe (RFC 8292).
    ///
    /// Public by definition — it is what identifies this server *to* the push
    /// service, and every subscriber holds a copy.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// The `Authorization: vapid t=…, k=…` header for one endpoint.
    ///
    /// The audience is the *origin* of the endpoint and not the endpoint itself:
    /// RFC 8292 scopes a signature to a push service, so it is one signature per
    /// origin rather than one per subscription.
    fn authorization(&self, endpoint: &url::Url, now: u64) -> PortResult<String> {
        let Some(host) = endpoint.host_str() else {
            return Err(PortError::corrupt(
                "device.transport",
                "a subscription endpoint with no host",
            ));
        };

        #[derive(Serialize)]
        struct Vapid<'a> {
            aud: String,
            exp: u64,
            sub: &'a str,
        }

        let claims = Vapid {
            aud: format!("{}://{host}", endpoint.scheme()),
            exp: now + VAPID_VALIDITY.as_secs(),
            sub: &self.contact,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        let token = jsonwebtoken::encode(&header, &claims, &self.signing)
            .map_err(|error| PortError::unavailable("push.webpush", error))?;

        Ok(format!("vapid t={token}, k={}", self.public_key))
    }
}

#[async_trait::async_trait]
impl PushTransport for WebPush {
    fn name(&self) -> &'static str {
        "webpush"
    }

    fn max_payload_bytes(&self) -> usize {
        MAX_PAYLOAD
    }

    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let notify_domain::PushTransport::WebPush {
            endpoint,
            p256dh,
            auth,
        } = &device.transport
        else {
            // The registry dispatches by name, so reaching here means a device whose
            // stored transport disagrees with the name it enrolled under.
            return Err(PortError::corrupt(
                "device.transport",
                "not a Web Push subscription",
            ));
        };

        // A subscription whose keys cannot be read will never work again, and the
        // relay is not going to say so. Reported as expired rather than failed, which
        // revokes the device instead of retrying it until the notification does.
        let (Ok(subscriber), Ok(secret)) = (decode_key(p256dh), decode_auth(auth)) else {
            return Ok(PushOutcome::Expired);
        };

        let plaintext = serde_json::to_vec(&Envelope::of(message))
            .map_err(|error| PortError::corrupt("push.envelope", error))?;
        let encrypted = web_push_native::encrypt(plaintext, &subscriber, &secret)
            .map_err(|error| PortError::unavailable("push.webpush", error))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| PortError::unavailable("push.webpush", error))?
            .as_secs();

        let response = self
            .http
            .post(endpoint.clone())
            .header("TTL", TTL.as_secs())
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Urgency", urgency(message))
            .header("Authorization", self.authorization(endpoint, now)?)
            .body(encrypted)
            .send()
            .await;

        Ok(match response {
            Ok(response) => outcome(response.status().as_u16()),
            // The relay was unreachable. A routine outcome on a flaky network and a
            // metric rather than an error: `Err` is for this transport being
            // unusable, which a dropped connection is not.
            Err(_) => PushOutcome::Failed {
                reason: "unreachable",
            },
        })
    }
}

/// Web Push as a dispatcher, so this crate's own tests reach it through the trait
/// the fan-out uses rather than through the concrete type.
#[async_trait::async_trait]
impl PushDispatcher for WebPush {
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        self.deliver(device, message).await
    }
}

/// A registered transport, for the composition root.
///
/// # Errors
///
/// As [`WebPush::new`].
pub fn transport(config: &WebPushConfig) -> Result<Arc<dyn PushTransport>, WebPushSetup> {
    Ok(Arc::new(WebPush::new(config)?))
}

/// Read a P-256 private key in either PEM form an operator is likely to have.
///
/// PKCS#8 (`BEGIN PRIVATE KEY`) is what `openssl genpkey` writes and what most
/// tooling produces; SEC1 (`BEGIN EC PRIVATE KEY`) is what `openssl ecparam` does.
/// Accepting one and refusing the other would be a support question rather than a
/// decision.
fn read_key(pem: &str) -> Result<p256::SecretKey, WebPushSetup> {
    p256::SecretKey::from_pkcs8_pem(pem)
        .or_else(|_| pem.parse::<p256::SecretKey>())
        .map_err(|_| WebPushSetup::Key)
}

/// How a push service's answer maps onto [`PushOutcome`].
///
/// The distinction that matters is `Expired`: `404` and `410` mean the subscription
/// is gone, and the right response is to revoke the device rather than retry it
/// until the notification expires.
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
        400..=499 => PushOutcome::Failed {
            reason: "rejected",
        },
        _ => PushOutcome::Failed {
            reason: "relay_unavailable",
        },
    }
}

/// The `Urgency` header of RFC 8030 §5.3.
///
/// A push service uses it to decide what may wait for a device to wake. It is a hint
/// about batching, not a way past a recipient's do-not-disturb — that was applied
/// before this transport was reached (design §7.4).
const fn urgency(message: &PushMessage) -> &'static str {
    match message.priority() {
        Priority::Min => "very-low",
        Priority::Low => "low",
        Priority::Default | Priority::High => "normal",
        Priority::Urgent => "high",
    }
}

/// A subscriber's `p256dh`: their public key, base64url, uncompressed point.
fn decode_key(raw: &str) -> Result<p256::PublicKey, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| ())?;
    p256::PublicKey::from_sec1_bytes(&bytes).map_err(|_| ())
}

/// A subscriber's `auth`: sixteen octets of shared secret, base64url.
fn decode_auth(raw: &str) -> Result<web_push_native::Auth, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| ())?;
    let sized: [u8; 16] = bytes.try_into().map_err(|_| ())?;
    Ok(web_push_native::Auth::from(sized))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{DeviceId, Principal, UserId};
    use time::OffsetDateTime;

    use super::*;

    /// A P-256 key generated for this test module and used nowhere else.
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgevZzL1gdAFr88hb2\n\
OF/2NxApJCzGCEDdfSp6VQO30hyhRANCAAQRWz+jn65BtOMvdyHKcvjBeBSDZH2r\n\
1RTwjmYSi9R/zpBnuQ4EiMnCqfMPWiZqB4QdbAd0E7oH50VpuZ1P087G\n\
-----END PRIVATE KEY-----\n";

    /// A subscriber's `auth`: sixteen octets, base64url, as a browser hands it out.
    const AUTH: &str = "tBHItJI5svbpez7KI4CCXg";

    /// A subscriber's `p256dh`.
    ///
    /// Derived from a real key rather than pasted, because a string that only looks
    /// like a point makes [`decode_key`] fail and every delivery test then passes by
    /// taking the `Expired` shortcut — proving nothing about the encryption.
    fn subscriber_key() -> String {
        use p256::elliptic_curve::sec1::ToEncodedPoint as _;
        let point = read_key(KEY).unwrap().public_key().to_encoded_point(false);
        URL_SAFE_NO_PAD.encode(point.as_bytes())
    }

    fn config() -> WebPushConfig {
        WebPushConfig {
            private_key_pem: KEY.to_owned(),
            contact: "mailto:ops@weebo.poc".to_owned(),
        }
    }

    fn device_with(p256dh: &str, endpoint: &str) -> Device {
        Device::enrol(
            DeviceId::generate(),
            Principal::User {
                id: UserId::generate(),
            },
            "browser",
            notify_domain::PushTransport::WebPush {
                endpoint: endpoint.parse().unwrap(),
                p256dh: p256dh.to_owned(),
                auth: AUTH.to_owned(),
            },
            None,
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
                bytes: serde_json::to_vec(&serde_json::json!({ "title": "etcd degraded" })).unwrap(),
                sealed: false,
            },
        }
    }

    #[test]
    fn a_key_that_is_not_one_is_refused_at_startup_and_not_at_the_first_push() {
        let bad = WebPushConfig {
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n"
                .to_owned(),
            ..config()
        };

        assert!(matches!(WebPush::new(&bad), Err(WebPushSetup::Key)));
    }

    #[test]
    fn a_contact_a_push_service_would_refuse_is_refused_here_first() {
        for contact in ["ops@weebo.poc", "http://weebo.poc", ""] {
            let bad = WebPushConfig {
                contact: contact.to_owned(),
                ..config()
            };
            assert!(
                matches!(WebPush::new(&bad), Err(WebPushSetup::Contact)),
                "`{contact}` was accepted"
            );
        }
    }

    #[test]
    fn the_public_key_is_the_uncompressed_point_a_browser_subscribes_with() {
        let transport = WebPush::new(&config()).unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(transport.public_key()).unwrap();

        assert_eq!(decoded.len(), 65, "an uncompressed P-256 point");
        assert_eq!(decoded[0], 0x04, "and it says so in its first byte");
    }

    #[test]
    fn the_signature_is_scoped_to_the_push_services_origin_and_not_the_subscription() {
        let transport = WebPush::new(&config()).unwrap();
        let one = transport
            .authorization(&"https://push.example/abc".parse().unwrap(), 1_785_000_000)
            .unwrap();
        let two = transport
            .authorization(&"https://push.example/zzz".parse().unwrap(), 1_785_000_000)
            .unwrap();

        assert_eq!(one, two, "RFC 8292 scopes a signature to a push service");
        assert!(one.starts_with("vapid t="));
        assert!(one.contains(&format!(", k={}", transport.public_key())));
    }

    #[test]
    fn the_signature_says_who_to_contact_and_when_it_stops_being_valid() {
        let transport = WebPush::new(&config()).unwrap();
        let header = transport
            .authorization(&"https://push.example/abc".parse().unwrap(), 1_785_000_000)
            .unwrap();

        let token = header
            .trim_start_matches("vapid t=")
            .split(',')
            .next()
            .unwrap();
        let claims = token.split('.').nth(1).unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();

        assert_eq!(claims["aud"], "https://push.example");
        assert_eq!(claims["sub"], "mailto:ops@weebo.poc");
        assert_eq!(claims["exp"], 1_785_000_000_u64 + VAPID_VALIDITY.as_secs());
    }

    #[test]
    fn an_envelope_carries_the_content_as_json_and_not_as_a_list_of_numbers() {
        let rendered = serde_json::to_value(Envelope::of(&notification())).unwrap();

        assert_eq!(rendered["kind"], "notification");
        assert_eq!(rendered["content"]["title"], "etcd degraded");
    }

    #[test]
    fn a_poke_carries_identifiers_and_nothing_else() {
        let poke = PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Poke,
        };

        let rendered = serde_json::to_value(Envelope::of(&poke)).unwrap();

        assert_eq!(rendered["kind"], "notification");
        assert!(
            rendered.get("content").is_none(),
            "the client wakes and fetches over TLS instead (§6.3)"
        );
    }

    #[test]
    fn a_digest_renders_as_the_two_numbers_it_is() {
        let rendered = serde_json::to_value(Envelope::of(&PushMessage::Digest {
            total: 12,
            failures: 2,
        }))
        .unwrap();

        assert_eq!(rendered["kind"], "digest");
        assert_eq!(rendered["total"], 12);
        assert_eq!(rendered["failures"], 2);
    }

    #[test]
    fn a_relay_saying_the_subscription_is_gone_gets_the_device_revoked_not_retried() {
        assert_eq!(outcome(410), PushOutcome::Expired);
        assert_eq!(outcome(404), PushOutcome::Expired);
        assert_eq!(outcome(201), PushOutcome::Delivered);
        assert!(matches!(
            outcome(429),
            PushOutcome::Failed {
                reason: "rate_limited"
            }
        ));
        assert!(matches!(
            outcome(503),
            PushOutcome::Failed {
                reason: "relay_unavailable"
            }
        ));
    }

    #[tokio::test]
    async fn a_subscription_whose_key_cannot_be_read_is_expired_rather_than_retried_forever() {
        let transport = WebPush::new(&config()).unwrap();
        let broken = device_with("not a key", "https://push.example/abc");

        let outcome = transport.deliver(&broken, &notification()).await.unwrap();

        assert_eq!(outcome, PushOutcome::Expired);
    }

    #[tokio::test]
    async fn a_relay_that_cannot_be_reached_is_a_metric_and_not_an_error() {
        let transport = WebPush::new(&config()).unwrap();
        // A port nothing listens on: the request fails at the socket, which also
        // means the encryption above it ran for real.
        let unreachable = device_with(&subscriber_key(), "http://127.0.0.1:1/push");

        let outcome = transport
            .deliver(&unreachable, &notification())
            .await
            .unwrap();

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

    #[test]
    fn urgency_maps_every_priority_the_domain_has() {
        for (priority, expected) in [
            (Priority::Min, "very-low"),
            (Priority::Low, "low"),
            (Priority::Default, "normal"),
            (Priority::High, "normal"),
            (Priority::Urgent, "high"),
        ] {
            let message = PushMessage::Notification {
                channel: ChannelId::parse("ci-infra").unwrap(),
                notification_id: NotificationId::generate(),
                priority,
                body: PushBody::Poke,
            };
            assert_eq!(urgency(&message), expected);
        }

        assert_eq!(
            urgency(&PushMessage::Digest {
                total: 1,
                failures: 0
            }),
            "normal",
            "what waited eight hours by request must not then sound like an alarm"
        );
    }
}
