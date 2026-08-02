//! The accounts behind a [`Principal`]: humans, bots, and the keys they act with.
//!
//! [`Principal`] answers "who is this request", which is all an authorisation check
//! needs. These types answer "who exists", which is what the identity endpoints and
//! the GitOps reconciler work on (design §5, §11).
//!
//! One thing is deliberately absent: **no secret material lives here**. An
//! [`ApiKey`] carries its prefix, its scopes and its dates, never the hash of its
//! secret — that stays in the persistence adapter behind the `SecretHasher` port.
//! An entity that is serialised into API responses should not be able to leak a
//! credential even if somebody adds a field to a DTO without thinking.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::channel::Scope;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text, required_text};
use crate::ids::{ApiKeyId, ChannelId, ServiceAccountId, UserId};
use crate::limits::{MAX_BODY_CHARS, MAX_TITLE_CHARS};
use crate::origin::Origin;
use crate::principal::{ApiKeyOwner, Principal};

/// A human, created on first OIDC login (design §5.1).
///
/// The `oidc_subject` is the identity; email and display name are claims that may
/// change, and the full profile — avatar, timezone, locale, and the rule that a
/// locally edited field stops being overwritten by later syncs — arrives with M2
/// (design §2.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Our identifier for them.
    pub id: UserId,
    /// The `sub` claim, scoped to the configured issuer. Stable across email and
    /// display-name changes, which is exactly why it and not the email is the key.
    pub oidc_subject: String,
    /// Their email, when the IdP releases one. Used by the individual `email`
    /// bindings in channel config (design §11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// What clients show next to their revisions and receipts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// First login.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl User {
    /// Create from OIDC claims.
    pub fn from_claims(
        id: UserId,
        oidc_subject: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Self> {
        Ok(Self {
            id,
            // Claims are third-party text reaching our terminals and browsers like
            // any other: an IdP is trusted to authenticate, not to be well-behaved.
            oidc_subject: required_text(
                oidc_subject,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "user.oidc_subject",
            )?,
            email: optional_text(email, TextKind::SingleLine, MAX_TITLE_CHARS, "user.email")?,
            display_name: optional_text(
                display_name,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "user.display_name",
            )?,
            created_at: now,
        })
    }

    /// This user as a principal.
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal::User { id: self.id }
    }
}

/// A named machine identity (design §5.2bis).
///
/// The recommended shape for anything durable: a team bot running on a person's
/// key dies the day that person is offboarded, which is a bad way to find out how
/// the deploy notifications worked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccount {
    /// Its name, and the id used everywhere it appears.
    pub id: ServiceAccountId,
    /// What clients show — "published by ArgoCD", not by an opaque id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What it is for. Read by whoever finds it in an audit log in six months.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// GitOps-declared or API-created (design §11).
    pub origin: Origin,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Set on deactivation. Deactivating revokes its keys and cuts its streams
    /// immediately (design §8, scenario T25) — it is not a soft "please stop".
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub disabled_at: Option<OffsetDateTime>,
}

impl ServiceAccount {
    /// Create one.
    pub fn new(
        id: ServiceAccountId,
        display_name: Option<&str>,
        description: Option<&str>,
        origin: Origin,
        now: OffsetDateTime,
    ) -> Result<Self> {
        Ok(Self {
            id,
            display_name: optional_text(
                display_name,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "service_account.display_name",
            )?,
            description: optional_text(
                description,
                TextKind::MultiLine,
                MAX_BODY_CHARS,
                "service_account.description",
            )?,
            origin,
            created_at: now,
            disabled_at: None,
        })
    }

    /// This account as a principal.
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal::ServiceAccount {
            id: self.id.clone(),
        }
    }

    /// Refuse to act as a deactivated account.
    pub fn ensure_active(&self) -> Result<()> {
        if self.disabled_at.is_some() {
            return Err(DomainError::ServiceAccountDisabled);
        }
        Ok(())
    }

    /// Deactivate. Idempotent, so a retried request cannot move the timestamp.
    pub fn disable(&mut self, now: OffsetDateTime) {
        self.disabled_at.get_or_insert(now);
    }
}

/// The public half of an API key: everything `GET /apikeys` may show (design §5.2).
///
/// The secret is returned exactly once, at creation, and is not reconstructible
/// from this — by design and by test (scenario T12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    /// Identity of the key.
    pub id: ApiKeyId,
    /// The lookup prefix, indexed so authentication is one indexed read rather
    /// than a hash comparison against every key on the instance.
    pub prefix: ApiKeyPrefix,
    /// A label so a human can tell two keys apart before revoking one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Who it acts as (design §5.2).
    pub owner: ApiKeyOwner,
    /// What it may do.
    pub scopes: Vec<Scope>,
    /// Which channels it may do it in. Empty means none: a key with no channel
    /// list is inert, never universal.
    pub channels: Vec<ChannelId>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Optional expiry. Absent is allowed on purpose: rotation is a policy an
    /// operator may want, not one the product imposes (design §5.2).
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub expires_at: Option<OffsetDateTime>,
    /// Last successful authentication. What makes "is anything still using this?"
    /// answerable before a revocation.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_used_at: Option<OffsetDateTime>,
    /// Set on revocation, which takes effect immediately.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub revoked_at: Option<OffsetDateTime>,
}

impl ApiKey {
    /// Whether this key may still be used, and why not if it may not.
    ///
    /// Expiry and revocation get distinct codes so a client can say "your key
    /// expired, here is how to rotate it" instead of a flat 401 (design §5.2,
    /// scenario T12).
    pub fn ensure_usable(&self, now: OffsetDateTime) -> Result<()> {
        if self.revoked_at.is_some_and(|at| now >= at) {
            return Err(DomainError::ApiKeyRevoked);
        }
        if self.expires_at.is_some_and(|at| now >= at) {
            return Err(DomainError::ApiKeyExpired);
        }
        Ok(())
    }

    /// Whether this key carries `scope` for `channel`.
    ///
    /// Both halves are required: a key with `publish` everywhere but no channel
    /// grant publishes nowhere.
    #[must_use]
    pub fn grants(&self, channel: &ChannelId, scope: Scope) -> bool {
        self.scopes.contains(&scope) && self.channels.contains(channel)
    }

    /// The principal a request carrying this key acts as (design §5.2).
    #[must_use]
    pub fn principal(&self) -> Principal {
        self.owner.principal(self.id)
    }
}

/// The indexed lookup prefix of a key: the `<prefix>` of `wnk_<prefix>_<secret>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKeyPrefix(String);

impl ApiKeyPrefix {
    /// How long a prefix is. Short enough to read aloud, long enough that two keys
    /// colliding is a non-event rather than a lookup ambiguity.
    pub const LEN: usize = 8;

    /// Parse a prefix out of a caller-supplied value.
    pub fn parse(raw: &str) -> Result<Self> {
        let malformed = DomainError::Malformed {
            field: "api_key.prefix",
            reason: "expected 8 lowercase alphanumeric characters",
        };
        if raw.len() != Self::LEN
            || !raw
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return Err(malformed);
        }
        Ok(Self(raw.to_owned()))
    }

    /// The prefix text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ApiKeyPrefix {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A presented API key, split into the part that finds the row and the part that
/// proves ownership of it.
///
/// Parsing is separated from verification on purpose: the shape check is cheap and
/// happens before any database or Argon2 work, so a flood of malformed keys costs
/// almost nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedApiKey {
    /// The indexed lookup half.
    pub prefix: ApiKeyPrefix,
    /// The half that is hashed and compared. Never logged, never stored.
    pub secret: String,
}

impl PresentedApiKey {
    /// The scheme every key is written in (design §5.2).
    pub const SCHEME: &'static str = "wnk_";

    /// Split `wnk_<prefix>_<secret>`.
    pub fn parse(raw: &str) -> Result<Self> {
        let malformed = DomainError::Malformed {
            field: "api_key",
            reason: "expected the form `wnk_<prefix>_<secret>`",
        };
        let body = raw.strip_prefix(Self::SCHEME).ok_or(malformed.clone())?;
        let (prefix, secret) = body.split_once('_').ok_or(malformed.clone())?;
        if secret.is_empty() {
            return Err(malformed);
        }
        Ok(Self {
            prefix: ApiKeyPrefix::parse(prefix)?,
            secret: secret.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use time::Duration as TimeDuration;

    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap()
    }

    fn key() -> ApiKey {
        ApiKey {
            id: ApiKeyId::generate(),
            prefix: ApiKeyPrefix::parse("ci4xa9k2").unwrap(),
            label: None,
            owner: ApiKeyOwner::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            },
            scopes: vec![Scope::Publish],
            channels: vec![ChannelId::parse("ci-infra").unwrap()],
            created_at: now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        }
    }

    // --- T12 · API keys (design §3.4) -------------------------------------

    #[test]
    fn critical_t12_a_publish_only_key_can_neither_subscribe_nor_manage() {
        let key = key();
        let channel = ChannelId::parse("ci-infra").unwrap();

        assert!(key.grants(&channel, Scope::Publish));
        assert!(!key.grants(&channel, Scope::Subscribe));
        assert!(!key.grants(&channel, Scope::Manage));
    }

    #[test]
    fn critical_t12_a_key_grants_nothing_outside_its_channel_list() {
        let key = key();
        let elsewhere = ChannelId::parse("cluster-ovh-1").unwrap();
        assert!(
            !key.grants(&elsewhere, Scope::Publish),
            "scopes are meaningless without a channel to apply them to"
        );
    }

    #[test]
    fn critical_t12_expiry_and_revocation_are_told_apart() {
        let mut expired = key();
        expired.expires_at = Some(now());
        assert_eq!(
            expired.ensure_usable(now()).unwrap_err().code(),
            "apikey.expired"
        );
        assert!(
            expired
                .ensure_usable(now() - TimeDuration::seconds(1))
                .is_ok()
        );

        let mut revoked = key();
        revoked.revoked_at = Some(now());
        assert_eq!(
            revoked.ensure_usable(now()).unwrap_err().code(),
            "apikey.revoked"
        );

        // Revocation is the more urgent thing to say when both are true.
        let mut both = key();
        both.expires_at = Some(now());
        both.revoked_at = Some(now());
        assert_eq!(
            both.ensure_usable(now()).unwrap_err().code(),
            "apikey.revoked"
        );

        assert!(key().ensure_usable(now()).is_ok());
    }

    #[test]
    fn critical_t12_the_public_half_of_a_key_carries_no_secret() {
        // Whatever a DTO does with this, there is nothing here to leak.
        let rendered = serde_json::to_string(&key()).unwrap();
        assert!(rendered.contains("ci4xa9k2"));
        assert!(!rendered.contains("secret") && !rendered.contains("hash"));
    }

    #[test]
    fn a_presented_key_splits_into_lookup_and_proof() {
        let presented = PresentedApiKey::parse("wnk_ci4xa9k2_s3cr3tv4lu3").unwrap();
        assert_eq!(presented.prefix.as_str(), "ci4xa9k2");
        assert_eq!(presented.secret, "s3cr3tv4lu3");

        for bad in [
            "ci4xa9k2_s3cr3t",      // no scheme
            "wnk_ci4xa9k2",         // no secret
            "wnk_ci4xa9k2_",        // empty secret
            "wnk_SHORT_s3cr3t",     // prefix is not the right shape
            "wnk_ci4xa9k2x_s3cr3t", // prefix is the wrong length
        ] {
            assert!(PresentedApiKey::parse(bad).is_err(), "`{bad}` parsed");
        }
    }

    #[test]
    fn a_disabled_service_account_stops_acting_immediately() {
        let mut bot = ServiceAccount::new(
            ServiceAccountId::parse("bot-argocd").unwrap(),
            Some("ArgoCD"),
            None,
            Origin::Dynamic,
            now(),
        )
        .unwrap();

        assert!(bot.ensure_active().is_ok());
        bot.disable(now());
        assert_eq!(
            bot.ensure_active().unwrap_err().code(),
            "service_account.disabled"
        );

        bot.disable(now() + TimeDuration::hours(1));
        assert_eq!(
            bot.disabled_at,
            Some(now()),
            "disabling twice is disabling once"
        );
    }

    #[test]
    fn oidc_claims_are_sanitised_like_any_other_third_party_text() {
        let user = User::from_claims(
            UserId::generate(),
            "ak-user-42",
            Some("alice@weebo.poc"),
            Some("\u{1b}[31mAlice\u{1b}[0m"),
            now(),
        )
        .unwrap();

        assert_eq!(user.display_name.as_deref(), Some("Alice"));
        assert_eq!(user.principal(), Principal::User { id: user.id });

        // An IdP is trusted to authenticate, not to be well-behaved.
        assert!(User::from_claims(UserId::generate(), "\u{1b}[2J", None, None, now()).is_err());
    }
}
