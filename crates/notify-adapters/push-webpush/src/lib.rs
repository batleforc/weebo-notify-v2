//! Web Push: RFC 8030 delivery, RFC 8291 encryption, RFC 8292 VAPID (design §6.2, §7.1).
//! Encryption is [`encrypt`] on aws-lc; VAPID is signed by `jsonwebtoken` on the same backend.
//! [`Envelope`] is the wire contract with the service worker: changing it breaks installed clients.

mod encrypt;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notify_domain::{ChannelId, Device, NotificationId, Priority};
use notify_ports::{
    PortError, PortResult, PushBody, PushDispatcher, PushMessage, PushOutcome, PushTransport,
};
use notify_push_http::{PublicOnly, send_retrying};
use p256::pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _};
use serde::Serialize;

/// Max plaintext of the whole serialised [`Envelope`]: RFC 8291's 4096 encrypted octets
/// minus `aes128gcm`'s 86-octet header and 17-octet record overhead. Exact; a test encrypts at the line.
const MAX_PLAINTEXT: usize = 3_993;

/// Queue lifetime at the push service; older than this, the inbox shows it on next open anyway.
const TTL: Duration = Duration::from_hours(4);

/// VAPID token lifetime: half RFC 8292's 24h cap, so a skewed clock is still accepted.
const VAPID_VALIDITY: Duration = Duration::from_hours(12);

/// Why a transport could not be built: an operator mistake found at startup (design §11).
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

/// What a service worker receives once decrypted; tagged like [`PushMessage`].
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Envelope {
    /// Something happened in a channel.
    Notification {
        /// Which channel, so a client can route it before fetching anything.
        channel: ChannelId,
        /// Which notification.
        notification_id: NotificationId,
        /// The content, when it fitted; absent means a poke — fetch over TLS (§6.3).
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
    },
    /// A person closed it (RFC 0004): the worker replaces the alert under the same
    /// tag, silently, and a visible tab takes it from the stream instead.
    Closure {
        /// Which channel.
        channel: ChannelId,
        /// Which notification.
        notification_id: NotificationId,
        /// Who closed it, as members see them.
        #[serde(skip_serializing_if = "Option::is_none")]
        closed_by: Option<String>,
    },
    /// The one push that ends a quiet period (design §7.4).
    Digest {
        /// How many were held back.
        total: u32,
        /// How many of those closed on a failure.
        failures: u32,
    },
}

/// What gets encrypted: the envelope, or its poke when the envelope is over
/// [`MAX_PLAINTEXT`] — the service worker fetches over TLS (design §6.3).
fn plaintext(message: &PushMessage) -> PortResult<Vec<u8>> {
    let encode = |message: &PushMessage| {
        serde_json::to_vec(&Envelope::of(message))
            .map_err(|error| PortError::corrupt("push.envelope", error))
    };
    let full = encode(message)?;
    if full.len() <= MAX_PLAINTEXT {
        return Ok(full);
    }
    encode(&message.as_poke())
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
                    // The bytes are a serialised `Content`: splice as JSON, not a byte array.
                    PushBody::Payload { bytes, .. } => serde_json::from_slice(bytes).ok(),
                    PushBody::Poke => None,
                },
            },
            PushMessage::Closure {
                channel,
                notification_id,
                closed_by,
            } => Self::Closure {
                channel: channel.clone(),
                notification_id: *notification_id,
                closed_by: closed_by.clone(),
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
    /// Operator contact (`mailto:…` or `https://…`), the required RFC 8292 `sub` claim.
    pub contact: String,
}

/// Web Push, over the wire.
pub struct WebPush {
    http: reqwest::Client,
    signing: jsonwebtoken::EncodingKey,
    /// Uncompressed public point, base64url: VAPID `k=` and the `applicationServerKey`.
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
    /// Fails at startup if the key is not a readable P-256 key or the contact scheme is wrong.
    pub fn new(config: &WebPushConfig) -> Result<Self, WebPushSetup> {
        use p256::elliptic_curve::sec1::ToSec1Point as _;

        if !config.contact.starts_with("mailto:") && !config.contact.starts_with("https://") {
            return Err(WebPushSetup::Contact);
        }

        // `p256` reads either PEM form and renders the public point; `jsonwebtoken`
        // signs from the PKCS#8 it re-encodes, so SEC1 keys work on any backend.
        let secret = read_key(&config.private_key_pem)?;
        let pkcs8 = secret.to_pkcs8_der().map_err(|_| WebPushSetup::Key)?;
        let signing = jsonwebtoken::EncodingKey::from_ec_der(pkcs8.as_bytes());

        let point = secret.public_key().to_sec1_point(false);

        Ok(Self {
            // SSRF guard: the endpoint is subscriber-chosen, so no redirects and public IPs only (§6.2).
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                // A proxy would resolve the name itself, and `PublicOnly` would
                // never be asked: `HTTPS_PROXY` in the environment must not be a
                // way around it.
                .no_proxy()
                .dns_resolver(PublicOnly)
                // reqwest's `gzip` is on for the feeds (RFC 0002 §6.5); not here.
                .no_gzip()
                .build()
                .map_err(WebPushSetup::Client)?,
            signing,
            public_key: URL_SAFE_NO_PAD.encode(point.as_bytes()),
            contact: config.contact.clone(),
        })
    }

    /// The `applicationServerKey` a browser needs to subscribe (RFC 8292).
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// The `Authorization: vapid t=…, k=…` header; RFC 8292 scopes it to the endpoint's origin.
    fn authorization(&self, endpoint: &url::Url, now: u64) -> PortResult<String> {
        #[derive(Serialize)]
        struct Vapid<'a> {
            aud: String,
            exp: u64,
            sub: &'a str,
        }

        let Some(host) = endpoint.host_str() else {
            return Err(PortError::corrupt(
                "device.transport",
                "a subscription endpoint with no host",
            ));
        };

        let claims = Vapid {
            aud: format!("{}://{host}", endpoint.scheme()),
            exp: now.saturating_add(VAPID_VALIDITY.as_secs()),
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
        MAX_PLAINTEXT
    }

    async fn deliver(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        let notify_domain::PushTransport::WebPush {
            endpoint,
            p256dh,
            auth,
        } = &device.transport
        else {
            // The registry dispatches by name, so this is a stored-transport mismatch.
            return Err(PortError::corrupt(
                "device.transport",
                "not a Web Push subscription",
            ));
        };

        // Re-checked for rows predating enrolment validation; IP literals bypass the resolver.
        if notify_domain::check_push_endpoint(endpoint).is_err() {
            return Ok(PushOutcome::Expired);
        }

        // Unreadable keys never recover: expire (revoke) rather than retry.
        let (Ok(subscriber), Ok(secret)) = (decode_key(p256dh), decode_auth(auth)) else {
            return Ok(PushOutcome::Expired);
        };

        let plaintext = plaintext(message)?;
        let encrypted = encrypt::encrypt(&plaintext, &subscriber, &secret)
            .map_err(|error| PortError::unavailable("push.webpush", error))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| PortError::unavailable("push.webpush", error))?
            .as_secs();

        let response = send_retrying(
            self.http
                .post(endpoint.clone())
                .header("TTL", TTL.as_secs())
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .header("Urgency", urgency(message))
                .header("Authorization", self.authorization(endpoint, now)?)
                .body(encrypted),
        )
        .await;

        Ok(match response {
            Some(response) => outcome(response.status().as_u16()),
            // Unreachable relay is a metric, not an `Err` (which means the transport is unusable).
            None => PushOutcome::Failed {
                reason: "unreachable",
            },
        })
    }
}

/// Web Push as a dispatcher, so tests reach it through the fan-out's trait.
#[async_trait::async_trait]
impl PushDispatcher for WebPush {
    async fn dispatch(&self, device: &Device, message: &PushMessage) -> PortResult<PushOutcome> {
        self.deliver(device, message).await
    }
}

/// A VAPID key for tests, derived from a fixed scalar so no PEM literal trips secret scanners.
#[cfg(any(test, feature = "test-utils"))]
#[must_use]
pub fn test_vapid_key() -> String {
    use p256::pkcs8::EncodePrivateKey as _;

    #[expect(
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

/// Read a P-256 private key as PKCS#8 or SEC1 PEM.
fn read_key(pem: &str) -> Result<p256::SecretKey, WebPushSetup> {
    p256::SecretKey::from_pkcs8_pem(pem)
        .or_else(|_| pem.parse::<p256::SecretKey>())
        .map_err(|_| WebPushSetup::Key)
}

/// Map a push service status to [`PushOutcome`]; `404`/`410` mean revoke, not retry.
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

/// The RFC 8030 §5.3 `Urgency` batching hint; do-not-disturb was already applied (§7.4).
const fn urgency(message: &PushMessage) -> &'static str {
    match message.priority() {
        Priority::Min => "very-low",
        Priority::Low => "low",
        Priority::Default | Priority::High => "normal",
        Priority::Urgent => "high",
    }
}

/// A subscriber's `p256dh`: their public key, base64url, uncompressed point. Checked on
/// the curve here, so an unreadable key expires the device instead of failing every push.
fn decode_key(raw: &str) -> Result<Vec<u8>, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| ())?;
    p256::PublicKey::from_sec1_bytes(&bytes).map_err(|_| ())?;
    Ok(bytes)
}

/// A subscriber's `auth`: sixteen octets of shared secret, base64url.
fn decode_auth(raw: &str) -> Result<[u8; 16], ()> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| ())?;
    bytes.try_into().map_err(|_| ())
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::arithmetic_side_effects,
        reason = "test arithmetic on small, known values"
    )]

    use notify_domain::{DeviceId, Principal, UserId};
    use time::OffsetDateTime;

    use super::*;

    /// A subscriber's `auth`: sixteen octets, base64url, as a browser hands it out.
    const AUTH: &str = "tBHItJI5svbpez7KI4CCXg";

    /// A real subscriber `p256dh`, so delivery tests exercise encryption, not the `Expired` shortcut.
    fn subscriber_key() -> String {
        use p256::elliptic_curve::sec1::ToSec1Point as _;
        let point = read_key(&test_vapid_key())
            .unwrap()
            .public_key()
            .to_sec1_point(false);
        URL_SAFE_NO_PAD.encode(point.as_bytes())
    }

    fn config() -> WebPushConfig {
        WebPushConfig {
            private_key_pem: test_vapid_key(),
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
                bytes: serde_json::to_vec(&serde_json::json!({ "title": "etcd degraded" }))
                    .unwrap(),
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
        use p256::pkcs8::EncodePublicKey as _;

        let transport = WebPush::new(&config()).unwrap();
        // ECDSA here is randomised, so two tokens never match byte for byte: check that
        // each verifies against the advertised key for the origin, whatever the path.
        let public = read_key(&test_vapid_key())
            .unwrap()
            .public_key()
            .to_public_key_pem(p256::pkcs8::LineEnding::LF)
            .unwrap();
        let key = jsonwebtoken::DecodingKey::from_ec_pem(public.as_bytes()).unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.validate_exp = false;
        validation.set_audience(&["https://push.example"]);

        for path in ["https://push.example/abc", "https://push.example/zzz"] {
            let header = transport
                .authorization(&path.parse().unwrap(), 1_785_000_000)
                .unwrap();
            assert!(header.contains(&format!(", k={}", transport.public_key())));
            let token = header
                .trim_start_matches("vapid t=")
                .split(',')
                .next()
                .unwrap();
            jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)
                .unwrap_or_else(|error| panic!("{path}: {error}"));
        }
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
        // RFC 6761 name: fails before the socket, after encryption ran.
        let unreachable = device_with(&subscriber_key(), "https://push.invalid/push");

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

    /// A notification whose serialised envelope is exactly `len` octets.
    fn enveloped_at(len: usize) -> PushMessage {
        let with_title = |title: String| PushMessage::Notification {
            channel: ChannelId::parse("ci-infra").unwrap(),
            notification_id: NotificationId::generate(),
            priority: Priority::Default,
            body: PushBody::Payload {
                bytes: serde_json::to_vec(&serde_json::json!({ "title": title })).unwrap(),
                sealed: false,
            },
        };
        let overhead = serde_json::to_vec(&Envelope::of(&with_title(String::new())))
            .unwrap()
            .len();
        with_title("x".repeat(len - overhead))
    }

    #[test]
    fn the_plaintext_ceiling_encrypts_to_exactly_what_rfc_8291_guarantees() {
        let subscriber = decode_key(&subscriber_key()).unwrap();
        let secret = decode_auth(AUTH).unwrap();

        let encrypted = encrypt::encrypt(&[b'x'; MAX_PLAINTEXT], &subscriber, &secret).unwrap();

        assert_eq!(
            encrypted.len(),
            4_096,
            "header, delimiter and tag all counted"
        );
    }

    #[test]
    fn an_envelope_at_the_ceiling_keeps_its_content_and_one_past_it_is_a_poke() {
        let at = plaintext(&enveloped_at(MAX_PLAINTEXT)).unwrap();
        assert_eq!(at.len(), MAX_PLAINTEXT);
        assert!(serde_json::from_slice::<serde_json::Value>(&at).unwrap()["content"].is_object());

        let over = plaintext(&enveloped_at(MAX_PLAINTEXT + 1)).unwrap();
        let over: serde_json::Value = serde_json::from_slice(&over).unwrap();
        assert!(
            over.get("content").is_none(),
            "the worker fetches over TLS instead"
        );
        assert_eq!(
            over["channel"], "ci-infra",
            "a poke still says where to look"
        );
    }
}
