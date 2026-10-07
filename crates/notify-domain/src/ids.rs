//! Typed identifiers, parsed strictly: minted ids are prefixed ULIDs (`ntf_01J…`,
//! time-ordered); named ids are canonical lowercase slugs chosen by operators.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, sanitize};
use crate::limits::{MAX_EXTERNAL_ID_CHARS, MAX_KEY_CHARS};

macro_rules! ulid_newtype {
    ($(#[$meta:meta])* $name:ident, $prefix:literal, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
        // A prefixed string on the wire, never the raw ULID the struct holds.
        #[cfg_attr(feature = "openapi", schema(value_type = String))]
        pub struct $name(Ulid);

        impl $name {
            /// The textual prefix every rendering of this id carries.
            pub const PREFIX: &'static str = $prefix;

            /// Mint a fresh id.
            #[must_use]
            pub fn generate() -> Self {
                Self(Ulid::new())
            }

            /// Wrap an existing ULID, for adapters rehydrating from storage.
            #[must_use]
            pub const fn from_ulid(ulid: Ulid) -> Self {
                Self(ulid)
            }

            /// The underlying ULID, for adapters that persist 128 bits.
            #[must_use]
            pub const fn as_ulid(self) -> Ulid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", Self::PREFIX, self.0)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(raw: &str) -> Result<Self> {
                let body = raw.strip_prefix(Self::PREFIX).ok_or(DomainError::Malformed {
                    field: $field,
                    reason: concat!("expected an id prefixed with `", $prefix, "`"),
                })?;
                Ulid::from_string(body)
                    .map(Self)
                    .map_err(|_| DomainError::Malformed {
                        field: $field,
                        reason: "not a valid ULID",
                    })
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

ulid_newtype!(
    /// A notification, stable across every revision it accumulates (design §2.2).
    NotificationId,
    "ntf_",
    "notification_id"
);

ulid_newtype!(
    /// One immutable revision of a notification.
    RevisionId,
    "rev_",
    "revision_id"
);

ulid_newtype!(
    /// One recorded interaction: an acknowledgement, a reply, a pick (design §2.3).
    InteractionId,
    "int_",
    "interaction_id"
);

ulid_newtype!(
    /// One delivery or read receipt (design §7.3).
    ReceiptId,
    "rcp_",
    "receipt_id"
);

ulid_newtype!(
    /// An uploaded attachment, referenced by a payload rather than embedded (§2.3).
    AttachmentId,
    "att_",
    "attachment_id"
);

ulid_newtype!(
    /// A human, created on first OIDC login.
    UserId,
    "usr_",
    "user_id"
);

ulid_newtype!(
    /// An API key. Identifies the key, never its secret (design §5.2).
    ApiKeyId,
    "key_",
    "api_key_id"
);

ulid_newtype!(
    /// An enrolled device: a browser, a desktop app, a phone (design §6.2).
    DeviceId,
    "dev_",
    "device_id"
);

ulid_newtype!(
    /// One outbound delivery chain (§7.1bis), sent as `X-Weebo-Delivery` and reused
    /// across retries as the consumer's idempotency key.
    DeliveryId,
    "dlv_",
    "delivery_id"
);

/// A canonical name: lowercase ASCII alphanumerics separated by `-`, `_` or `.`,
/// starting and ending on an alphanumeric.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Slug(String);

impl Slug {
    /// Parse and normalise, reporting rejections against `field`.
    pub fn parse(raw: &str, field: &'static str) -> Result<Self> {
        let value = sanitize(raw, TextKind::SingleLine).to_ascii_lowercase();

        if value.is_empty() {
            return Err(DomainError::Empty { field });
        }
        let len = value.chars().count();
        if len > MAX_KEY_CHARS {
            return Err(DomainError::TooLong {
                field,
                max: MAX_KEY_CHARS,
                actual: len,
            });
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(DomainError::Malformed {
                field,
                reason: "only ASCII letters, digits, `-`, `_` and `.` are allowed",
            });
        }
        let edges_are_alphanumeric = value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && value
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if !edges_are_alphanumeric {
            return Err(DomainError::Malformed {
                field,
                reason: "must start and end with a letter or a digit",
            });
        }

        Ok(Self(value))
    }

    /// The normalised text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! slug_newtype {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
        #[cfg_attr(feature = "openapi", schema(value_type = String))]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(Slug);

        impl $name {
            /// Parse and normalise.
            pub fn parse(raw: &str) -> Result<Self> {
                Slug::parse(raw, $field).map(Self)
            }

            /// The normalised text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(raw: &str) -> Result<Self> {
                Self::parse(raw)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

slug_newtype!(
    /// A channel, named by the operator: `ci-infra`, `cluster-ovh-1`.
    ChannelId,
    "channel_id"
);

slug_newtype!(
    /// A service account, named so a timeline can say who published (§5.2bis).
    ServiceAccountId,
    "service_account_id"
);

slug_newtype!(
    /// The publisher-chosen key an `ack` or `choice` action is addressed by, stable
    /// across revisions (interactions dedup per user and key, §2.3).
    ActionKey,
    "action.key"
);

slug_newtype!(
    /// One option of a `choice` action.
    ChoiceOptionKey,
    "action.options.key"
);

/// A publisher-supplied identifier for idempotent upserts (§2.2), looser than a
/// [`Slug`]; refused if sanitisation would change it.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ExternalId(String);

impl ExternalId {
    /// Parse, rejecting anything that would not round-trip.
    pub fn parse(raw: &str) -> Result<Self> {
        const FIELD: &str = "external_id";

        let value = sanitize(raw, TextKind::SingleLine);
        if value.is_empty() {
            return Err(DomainError::Empty { field: FIELD });
        }
        if value != raw {
            return Err(DomainError::Malformed {
                field: FIELD,
                reason: "must not contain control characters or leading/trailing whitespace",
            });
        }
        let len = value.chars().count();
        if len > MAX_EXTERNAL_ID_CHARS {
            return Err(DomainError::TooLong {
                field: FIELD,
                max: MAX_EXTERNAL_ID_CHARS,
                actual: len,
            });
        }
        Ok(Self(value))
    }

    /// The identifier as supplied.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ExternalId {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self> {
        Self::parse(raw)
    }
}

impl<'de> Deserialize<'de> for ExternalId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn minted_ids_round_trip_through_their_prefixed_form() {
        let id = NotificationId::generate();
        let text = id.to_string();
        assert!(text.starts_with("ntf_"));
        assert_eq!(text.parse::<NotificationId>().unwrap(), id);
    }

    #[test]
    fn minted_ids_reject_a_foreign_prefix() {
        let device = DeviceId::generate().to_string();
        // Id spaces must not alias.
        assert!(device.parse::<NotificationId>().is_err());
    }

    #[test]
    fn minted_ids_sort_by_creation_time() {
        // ULIDs order by millisecond, so the test works at millisecond granularity.
        let epoch = std::time::UNIX_EPOCH;
        let first = NotificationId::from_ulid(Ulid::from_datetime(epoch));
        let second = NotificationId::from_ulid(Ulid::from_datetime(
            epoch + core::time::Duration::from_secs(1),
        ));
        assert!(first.to_string() < second.to_string());
    }

    #[test]
    fn every_id_type_round_trips_through_json() {
        // Every id type must round-trip through serde.
        macro_rules! assert_round_trip {
            ($name:ident) => {{
                let id = $name::generate();
                let json = serde_json::to_string(&id).unwrap();
                assert!(json.starts_with(&format!("\"{}", $name::PREFIX)));
                assert_eq!(serde_json::from_str::<$name>(&json).unwrap(), id);
                assert_eq!($name::from_ulid(id.as_ulid()), id);
            }};
        }

        assert_round_trip!(NotificationId);
        assert_round_trip!(RevisionId);
        assert_round_trip!(InteractionId);
        assert_round_trip!(ReceiptId);
        assert_round_trip!(AttachmentId);
        assert_round_trip!(UserId);
        assert_round_trip!(ApiKeyId);
        assert_round_trip!(DeviceId);
    }

    #[test]
    fn named_ids_round_trip_through_json() {
        for raw in ["ci-infra", "bot-argocd", "ack-incident"] {
            let json = format!("\"{raw}\"");
            assert_eq!(
                serde_json::from_str::<ChannelId>(&json).unwrap().as_str(),
                raw
            );
        }
        assert!(serde_json::from_str::<ServiceAccountId>("\"-bad\"").is_err());
        assert!(serde_json::from_str::<ExternalId>("\" bad \"").is_err());
        assert_eq!(
            serde_json::from_str::<ExternalId>("\"run-4521\"")
                .unwrap()
                .as_str(),
            "run-4521"
        );
    }

    #[test]
    fn slugs_normalise_case() {
        assert_eq!(ChannelId::parse("Ci-Infra").unwrap().as_str(), "ci-infra");
    }

    #[test]
    fn slugs_reject_separators_at_the_edges_and_foreign_characters() {
        for bad in ["-ci", "ci-", "ci infra", "ci/infra", "ci:infra", ""] {
            assert!(ChannelId::parse(bad).is_err(), "`{bad}` should be refused");
        }
    }

    #[test]
    fn external_ids_keep_the_publisher_spelling() {
        let id = ExternalId::parse("deploy/prod#88").unwrap();
        assert_eq!(id.as_str(), "deploy/prod#88");
    }

    #[test]
    fn external_ids_refuse_anything_sanitising_would_rewrite() {
        // Sanitising would address a different notification.
        let err = ExternalId::parse("run-\u{1b}[2J4521").unwrap_err();
        assert_eq!(err.code(), "field.malformed");
        assert!(ExternalId::parse(" run-4521 ").is_err());
    }
}
