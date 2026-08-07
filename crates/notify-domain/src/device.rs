//! Devices: where a notification is delivered, and how far each one has read.
//!
//! A device is two things at once. It holds a **sync cursor**, which is what makes
//! going offline survivable (design §7.2), and it holds a **push transport**, which
//! is what makes a phone buzz. Not every device has both: a `notify watch` in a
//! debug pod is a cursor and nothing else, and that is a supported shape rather
//! than a degraded one.
//!
//! # Why only some devices carry a key
//!
//! Application-level encryption exists on exactly the paths where a **third party
//! carries the payload** (design §6.2):
//!
//! | Transport | Third-party relay | Protection | Device key |
//! | --- | --- | --- | --- |
//! | SSE / WS / sync | no — TLS to us | TLS | none |
//! | Web Push | browser push service | RFC 8291, keys held by the browser | none |
//! | UnifiedPush | the distributor | sealed box | X25519 |
//! | APNs | Apple | sealed box | X25519 |
//!
//! So enrolling a browser, a desktop or a CLI is *registering a transport*, with no
//! client-side crypto at all. Only mobile devices generate a keypair in hardware
//! and publish its public half, and the server re-encrypts to it at delivery.
//! Sealed-box delivery landed with M6 (`notify-push-unifiedpush`, APNs still to
//! come in M7); the invariant was enforced from the start so no device could be
//! enrolled that would later have to be re-enrolled.

use core::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::cursor::Cursor;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, required_text};
use crate::ids::DeviceId;
use crate::limits::MAX_TITLE_CHARS;
use crate::principal::Principal;

/// An X25519 public key, published by a mobile device at enrolment.
///
/// The private half never leaves the device's hardware keystore — Android Keystore
/// or the iOS Keychain (design §6.2). This type only ever holds the public half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(
    feature = "openapi",
    schema(value_type = String, format = Byte, description = "Base64url, 32 bytes")
)]
pub struct DevicePublicKey([u8; 32]);

impl DevicePublicKey {
    /// Wrap raw key bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes, for the transport that seals a payload to them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parse the lowercase hex form used on the wire.
    ///
    /// Hex rather than base64: a key is copied through logs, config and support
    /// tickets, and hex has no variant to get wrong (padding, URL-safe alphabet)
    /// at the cost of 22 characters nobody is counting.
    pub fn parse_hex(raw: &str) -> Result<Self> {
        let malformed = DomainError::Malformed {
            field: "device.public_key",
            reason: "expected 64 lowercase hex characters (an X25519 public key)",
        };
        if raw.len() != 64 {
            return Err(malformed);
        }

        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let start = index * 2;
            let pair = raw.get(start..start + 2).ok_or(malformed.clone())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| malformed.clone())?;
        }
        // Reject the uppercase spelling rather than accepting both: one rendering
        // means one string to compare, index and log.
        if raw.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(malformed);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for DevicePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for DevicePublicKey {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DevicePublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_hex(&raw).map_err(serde::de::Error::custom)
    }
}

/// How the server reaches a device when its client is not connected.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum PushTransport {
    /// None. The device is reachable only while it holds a stream — a CLI, a TUI,
    /// a service account's logical subscription.
    Stream,
    /// Web Push (RFC 8030/8291), for browsers and installed PWAs. The browser
    /// holds the decryption keys; we never see one (design §6.2).
    WebPush {
        /// The push service endpoint the browser handed us.
        endpoint: Url,
        /// The subscription's `p256dh` key, as given by the browser.
        p256dh: String,
        /// The subscription's `auth` secret, as given by the browser.
        auth: String,
    },
    /// UnifiedPush, self-hosted end to end: we expose the push endpoint ourselves
    /// and the distributor lives on the device (design §7.1bis).
    UnifiedPush {
        /// The distributor endpoint to POST to.
        endpoint: Url,
    },
    /// APNs — the one part of the stack that cannot be self-hosted (design §15).
    Apns {
        /// The APNs device token.
        token: String,
    },
}

impl PushTransport {
    /// Whether a third party carries the payload on this path, and therefore
    /// whether the device must publish an X25519 key (design §6.2).
    ///
    /// Web Push says *no* despite going through a push service: RFC 8291 already
    /// encrypts end to end with keys the browser holds internally, and there is no
    /// application key on the web at all.
    #[must_use]
    pub const fn requires_device_key(&self) -> bool {
        matches!(self, Self::UnifiedPush { .. } | Self::Apns { .. })
    }

    /// The label used in metrics and logs (design §13).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::WebPush { .. } => "webpush",
            Self::UnifiedPush { .. } => "unifiedpush",
            Self::Apns { .. } => "apns",
        }
    }
}

/// An enrolled device.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Identity.
    pub id: DeviceId,
    /// Whose it is. Service accounts hold devices too — logical ones, with no push
    /// transport (design §5.2bis).
    pub owner: Principal,
    /// What the owner calls it in the device list, so revoking the right one does
    /// not require guessing.
    pub label: String,
    /// How to reach it.
    pub transport: PushTransport,
    /// Its X25519 public key, when the transport needs one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<DevicePublicKey>,
    /// How far it has read. `None` means it has never synced, and will receive
    /// everything still retained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// When it was enrolled.
    #[serde(with = "time::serde::rfc3339")]
    pub enrolled_at: OffsetDateTime,
    /// Last time it was seen, for the device list.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_seen_at: Option<OffsetDateTime>,
    /// Set on revocation.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub revoked_at: Option<OffsetDateTime>,
}

impl Device {
    /// Enrol a device (design §8, `POST /devices`).
    ///
    /// The one invariant worth failing on: a transport that goes through a
    /// third-party relay must arrive with a public key. Enrolling one without would
    /// produce a device the server cannot deliver to, discovered at the worst
    /// possible moment — the first incident push.
    pub fn enrol(
        id: DeviceId,
        owner: Principal,
        label: &str,
        transport: PushTransport,
        public_key: Option<DevicePublicKey>,
        now: OffsetDateTime,
    ) -> Result<Self> {
        if transport.requires_device_key() && public_key.is_none() {
            return Err(DomainError::Missing {
                field: "device.public_key",
            });
        }

        Ok(Self {
            id,
            owner,
            label: required_text(label, TextKind::SingleLine, MAX_TITLE_CHARS, "device.label")?,
            transport,
            public_key,
            cursor: None,
            enrolled_at: now,
            last_seen_at: None,
            revoked_at: None,
        })
    }

    /// Revoke it (design §8, §15).
    ///
    /// Revocation drops the push subscription, the sync cursor **and** the public
    /// key in one move. Leaving the cursor behind would let a re-enrolled device
    /// silently resume somebody else's position; leaving the key behind would keep
    /// a payload sealed to hardware that is no longer trusted.
    pub fn revoke(&mut self, now: OffsetDateTime) {
        if self.revoked_at.is_some() {
            return;
        }
        self.revoked_at = Some(now);
        self.transport = PushTransport::Stream;
        self.public_key = None;
        self.cursor = None;
    }

    /// Whether it is still enrolled.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }

    /// Advance the sync cursor.
    ///
    /// Monotonic: a client replaying an old cursor — a retry, a race between two
    /// tabs — must never rewind the device and re-deliver what it has already seen.
    pub fn advance(&mut self, cursor: Cursor, now: OffsetDateTime) {
        self.last_seen_at = Some(now);
        if self.cursor.is_none_or(|current| cursor > current) {
            self.cursor = Some(cursor);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use time::Duration as TimeDuration;

    use super::*;
    use crate::ids::UserId;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap()
    }

    fn owner() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn key() -> DevicePublicKey {
        DevicePublicKey::from_bytes([7_u8; 32])
    }

    fn enrol(transport: PushTransport, public_key: Option<DevicePublicKey>) -> Result<Device> {
        Device::enrol(
            DeviceId::generate(),
            owner(),
            "Pixel 8",
            transport,
            public_key,
            now(),
        )
    }

    #[test]
    fn relayed_transports_demand_a_device_key() {
        let apns = PushTransport::Apns {
            token: "abc".to_owned(),
        };
        assert_eq!(
            enrol(apns.clone(), None).unwrap_err(),
            DomainError::Missing {
                field: "device.public_key"
            },
            "a device we cannot seal to is one we discover at the first incident push"
        );
        assert!(enrol(apns, Some(key())).is_ok());
    }

    #[test]
    fn direct_transports_need_no_client_side_crypto() {
        // Web Push goes through a third party but is already encrypted end to end
        // by the browser's own keys (design §6.2) — no application key exists.
        let webpush = PushTransport::WebPush {
            endpoint: Url::parse("https://push.example/abc").unwrap(),
            p256dh: "BPk...".to_owned(),
            auth: "k9...".to_owned(),
        };
        assert!(!webpush.requires_device_key());
        assert!(enrol(webpush, None).is_ok());

        // A `notify watch` in a debug pod: a cursor and nothing more.
        assert!(enrol(PushTransport::Stream, None).is_ok());
    }

    // --- T18 · Device revocation (design §3.4) ----------------------------

    #[test]
    fn critical_t18_revocation_drops_the_subscription_the_key_and_the_cursor() {
        let mut device = enrol(
            PushTransport::Apns {
                token: "abc".to_owned(),
            },
            Some(key()),
        )
        .unwrap();
        device.advance(Cursor::from_sequence(42), now());

        device.revoke(now() + TimeDuration::hours(1));

        assert!(!device.is_active());
        assert_eq!(device.transport, PushTransport::Stream);
        assert_eq!(
            device.public_key, None,
            "a sealed payload must have nowhere to go"
        );
        assert_eq!(
            device.cursor, None,
            "a re-enrolled device must not resume the revoked one's position"
        );
    }

    #[test]
    fn revoking_twice_does_not_move_the_timestamp() {
        let mut device = enrol(PushTransport::Stream, None).unwrap();
        device.revoke(now());
        device.revoke(now() + TimeDuration::hours(1));
        assert_eq!(device.revoked_at, Some(now()));
    }

    #[test]
    fn a_cursor_only_ever_moves_forward() {
        let mut device = enrol(PushTransport::Stream, None).unwrap();

        device.advance(Cursor::from_sequence(10), now());
        device.advance(Cursor::from_sequence(4), now() + TimeDuration::seconds(1));

        assert_eq!(
            device.cursor,
            Some(Cursor::from_sequence(10)),
            "a replayed cursor must not re-deliver what the device already read"
        );
        assert_eq!(device.last_seen_at, Some(now() + TimeDuration::seconds(1)));
    }

    #[test]
    fn every_transport_keeps_its_metric_label() {
        // These strings are the `transport` dimension of `push_delivery_duration_seconds`
        // and `push_failures_total` (design §13): renaming one silently breaks a
        // dashboard rather than a build.
        assert_eq!(PushTransport::Stream.name(), "stream");
        assert_eq!(
            PushTransport::WebPush {
                endpoint: Url::parse("https://push.example/abc").unwrap(),
                p256dh: "BPk".to_owned(),
                auth: "k9".to_owned(),
            }
            .name(),
            "webpush"
        );
        assert_eq!(
            PushTransport::UnifiedPush {
                endpoint: Url::parse("https://up.weebo.poc/abc").unwrap(),
            }
            .name(),
            "unifiedpush"
        );
        assert_eq!(
            PushTransport::Apns {
                token: "abc".to_owned()
            }
            .name(),
            "apns"
        );
    }

    #[test]
    fn a_device_round_trips_through_json() {
        let mut device = enrol(
            PushTransport::Apns {
                token: "abc".to_owned(),
            },
            Some(key()),
        )
        .unwrap();
        device.advance(Cursor::from_sequence(42), now());

        let json = serde_json::to_string(&device).unwrap();
        assert_eq!(serde_json::from_str::<Device>(&json).unwrap(), device);
        assert!(json.contains(&key().to_string()));
    }

    #[test]
    fn public_keys_round_trip_through_hex() {
        let rendered = key().to_string();
        assert_eq!(rendered, "07".repeat(32));
        assert_eq!(DevicePublicKey::parse_hex(&rendered).unwrap(), key());

        for bad in [
            "07",             // too short
            &"07".repeat(33), // too long
            &"0Z".repeat(32), // not hex
            &"AB".repeat(32), // uppercase: one rendering, one comparison
        ] {
            assert!(DevicePublicKey::parse_hex(bad).is_err(), "`{bad}` parsed");
        }
    }
}
