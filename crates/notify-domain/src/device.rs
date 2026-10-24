//! Devices: where a notification is delivered (push transport) and how far each
//! has read (sync cursor) (§7.2). Only relayed transports (UnifiedPush, APNs) need
//! an X25519 key; Web Push uses RFC 8291 browser keys, streams just TLS (§6.2).

use core::fmt;
use core::net::IpAddr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::cursor::Cursor;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, required_text};
use crate::ids::DeviceId;
use crate::limits::MAX_TITLE_CHARS;
use crate::principal::Principal;

/// An X25519 public key published by a mobile device; the private half never
/// leaves its hardware keystore (§6.2).
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
        // One rendering only: refuse uppercase.
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
    /// None: reachable only while it holds a stream (CLI, TUI, service account).
    Stream,
    /// Web Push (RFC 8030/8291), for browsers and installed PWAs; the browser holds the keys (§6.2).
    WebPush {
        /// The push service endpoint the browser handed us.
        endpoint: Url,
        /// The subscription's `p256dh` key, as given by the browser.
        p256dh: String,
        /// The subscription's `auth` secret, as given by the browser.
        auth: String,
    },
    /// UnifiedPush, self-hosted end to end (§7.1bis).
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
    /// Whether a third party carries the payload, so the device must publish an X25519
    /// key (§6.2). False for Web Push, which RFC 8291 already encrypts.
    #[must_use]
    pub const fn requires_device_key(&self) -> bool {
        matches!(self, Self::UnifiedPush { .. } | Self::Apns { .. })
    }

    /// The label used in metrics and logs (§13).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::WebPush { .. } => "webpush",
            Self::UnifiedPush { .. } => "unifiedpush",
            Self::Apns { .. } => "apns",
        }
    }

    /// The URL the server will `POST` to, when the device chose it.
    #[must_use]
    pub const fn endpoint(&self) -> Option<&Url> {
        match self {
            Self::WebPush { endpoint, .. } | Self::UnifiedPush { endpoint } => Some(endpoint),
            Self::Stream | Self::Apns { .. } => None,
        }
    }
}

/// Whether the server may `POST` to a client-chosen endpoint: HTTPS only and no
/// non-public host literal (SSRF). DNS resolution is checked by the transport.
pub fn check_push_endpoint(endpoint: &Url) -> Result<()> {
    let refused = DomainError::Malformed {
        field: "device.endpoint",
        reason: "expected an https URL to a public host",
    };
    if endpoint.scheme() != "https" {
        return Err(refused);
    }
    match endpoint.host() {
        Some(url::Host::Domain(name))
            if !name.eq_ignore_ascii_case("localhost")
                && !name.to_ascii_lowercase().ends_with(".localhost") =>
        {
            Ok(())
        }
        Some(url::Host::Ipv4(ip)) if is_public_address(IpAddr::V4(ip)) => Ok(()),
        Some(url::Host::Ipv6(ip)) if is_public_address(IpAddr::V6(ip)) => Ok(()),
        _ => Err(refused),
    }
}

/// Whether an address is publicly routable (not loopback, private, link-local, CGNAT,
/// documentation, multicast, reserved… per the IANA special-purpose registries), nor an
/// IPv6 prefix that embeds or tunnels IPv4 (NAT64, 6to4, Teredo); IPv4-mapped IPv6 is judged as IPv4.
#[must_use]
pub fn is_public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                // IETF protocol assignments (192.0.0.0/24) and the deprecated
                // 6to4 relay anycast (192.88.99.0/24).
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99)
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_address(IpAddr::V4(v4));
            }
            let segments = v6.segments();
            let [first, second, ..] = segments;
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local (fc00::/7), link-local (fe80::/10) and the
                // deprecated site-local (fec0::/10).
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first & 0xffc0) == 0xfec0
                // Documentation: 2001:db8::/32 and 3fff::/20.
                || (first == 0x2001 && second == 0x0db8)
                || (first & 0xfff0) == 0x3ff0
                // Discard-only (100::/64).
                || segments[..4] == [0x0100, 0, 0, 0]
                // SRv6 segment identifiers (5f00::/16), never routed publicly.
                || first == 0x5f00
                // IETF protocol assignments (2001::/23): Teredo, benchmarking,
                // ORCHID and friends. No push service lives in any of them.
                || (first == 0x2001 && second < 0x0200)
                // IPv4-compatible (::a.b.c.d, ::/96) and IPv4-translated
                // (::ffff:0:a.b.c.d): both embed an IPv4 address that a stack or
                // a translator may still deliver to.
                || segments[..6] == [0; 6]
                || segments[..6] == [0, 0, 0, 0, 0xffff, 0]
                // Prefixes that translate or tunnel to an IPv4 address: a gateway
                // on the path would carry the request to whatever IPv4 is embedded,
                // private ones included. Refused whole rather than judged by the
                // embedded address — no push service lives behind one.
                // NAT64 well-known (64:ff9b::/96) and local-use (64:ff9b:1::/48).
                || (first == 0x0064 && second == 0xff9b)
                // 6to4 (2002::/16). Teredo (2001::/32) is in 2001::/23 above.
                || first == 0x2002)
        }
    }
}

/// An enrolled device.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Identity.
    pub id: DeviceId,
    /// Whose it is; service accounts hold logical devices with no push transport (§5.2bis).
    pub owner: Principal,
    /// What the owner calls it in the device list.
    pub label: String,
    /// How to reach it.
    pub transport: PushTransport,
    /// Its X25519 public key, when the transport needs one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<DevicePublicKey>,
    /// How far it has read; `None` means it has never synced.
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
    /// Enrol a device (§8, `POST /devices`); relayed transports must come with a public key.
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
        if let Some(endpoint) = transport.endpoint() {
            check_push_endpoint(endpoint)?;
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

    /// Revoke it (§8, §15), dropping the push subscription, sync cursor and public key together.
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

    /// Advance the sync cursor, monotonically (stale cursors never rewind it).
    pub fn advance(&mut self, cursor: Cursor, now: OffsetDateTime) {
        self.last_seen_at = Some(now);
        if self.cursor.is_none_or(|current| cursor > current) {
            self.cursor = Some(cursor);
        }
    }
}

#[cfg(test)]
mod tests {
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
        // Web Push needs no application key (§6.2).
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

    // --- T18 · Device revocation ------------------------------------------

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
        // These are metric label values (§13): renaming breaks dashboards.
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
    fn a_push_endpoint_must_be_https_to_a_public_host() {
        for good in [
            "https://push.example/abc",
            "https://8.8.8.8/x",
            "https://[2606:4700::1111]/x",
        ] {
            assert!(
                check_push_endpoint(&Url::parse(good).unwrap()).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "http://push.example/abc",
            "https://localhost/x",
            "https://api.localhost/x",
            "https://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://172.16.0.1/x",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/x",
            "https://0.0.0.0/x",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://[fe80::1]/x",
            "https://[::ffff:10.0.0.1]/x",
            // IPv4 reached through a translator or a tunnel: NAT64 (well-known
            // and local-use), 6to4 wrapping 10.0.0.1, Teredo.
            "https://[64:ff9b::a00:1]/x",
            "https://[64:ff9b:1::a00:1]/x",
            "https://[2002:a00:1::1]/x",
            "https://[2001:0:4136:e378::1]/x",
            "file:///etc/passwd",
        ] {
            assert!(
                check_push_endpoint(&Url::parse(bad).unwrap()).is_err(),
                "{bad}"
            );
        }

        let internal = PushTransport::UnifiedPush {
            endpoint: Url::parse("https://169.254.169.254/").unwrap(),
        };
        assert!(enrol(internal, Some(key())).is_err());
    }

    #[test]
    fn every_iana_special_purpose_range_is_refused() {
        for special in [
            "192.0.0.8",
            "192.88.99.1",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::a00:1",
            "::8.8.8.8",
            "::ffff:0:a00:1",
            "fec0::1",
            "100::1",
            "2001:2::1",
            "2001:10::1",
            "2001:db8::1",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            let ip: IpAddr = special.parse().unwrap();
            assert!(!is_public_address(ip), "{special}");
        }
        for public in [
            "8.8.8.8",
            "192.0.1.1",
            "192.88.98.1",
            "2606:4700::1111",
            "2001:4860::8888",
        ] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(is_public_address(ip), "{public}");
        }
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
