//! The accounts behind a [`Principal`]: users, service accounts and API keys (§5, §11).
//! No secret material lives here, so no response DTO can leak one.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::channel::Scope;
use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, optional_text, required_text};
use crate::ids::{ApiKeyId, ChannelId, ServiceAccountId, UserId};
use crate::limits::{MAX_BODY_CHARS, MAX_TITLE_CHARS};
use crate::origin::Origin;
use crate::principal::{ApiKeyOwner, Principal};
use crate::profile::{Profile, ProfileEdit};

/// A human, created on first OIDC login (§5.1), keyed by `oidc_subject`.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Our identifier for them.
    pub id: UserId,
    /// The `sub` claim, scoped to the configured issuer; stable across email and name changes.
    pub oidc_subject: String,
    /// Their email, when the IdP releases one; used by `email` channel bindings (§11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// What clients show next to their revisions and receipts (mirrors `profile.display_name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What they chose for themselves (design §2.4).
    #[serde(default)]
    pub profile: Profile,
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
        let display_name = optional_text(
            display_name,
            TextKind::SingleLine,
            MAX_TITLE_CHARS,
            "user.display_name",
        )?;

        Ok(Self {
            id,
            // IdP claims are untrusted text like any other.
            oidc_subject: required_text(
                oidc_subject,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "user.oidc_subject",
            )?,
            email: optional_text(email, TextKind::SingleLine, MAX_TITLE_CHARS, "user.email")?,
            profile: Profile {
                display_name: display_name.clone(),
                ..Profile::default()
            },
            display_name,
            created_at: now,
        })
    }

    /// Refresh what the identity provider owns, returning whether anything changed
    /// (it runs on every request, so callers only write on `true`). An absent claim
    /// changes nothing: wiping an email would silently revoke `email` bindings (§11, T29).
    ///
    /// # An unverified claim is an erasure
    ///
    /// `email_unverified` says the token carried an address the verifier dropped.
    /// That is the provider declining to vouch for any address, not a client
    /// asking for less — so the stored one goes, or an email binding would keep
    /// granting access on the strength of a verification that no longer holds.
    ///
    /// # Errors
    ///
    /// Refuses claims that fail the hygiene rules, as `from_claims` does.
    pub fn sync_from_claims(
        &mut self,
        email: Option<&str>,
        email_unverified: bool,
        display_name: Option<&str>,
    ) -> Result<bool> {
        let mut changed = false;

        if email_unverified && email.is_none() {
            changed |= self.email.take().is_some();
        } else if email.is_some() {
            let email = optional_text(email, TextKind::SingleLine, MAX_TITLE_CHARS, "user.email")?;
            if email != self.email {
                self.email = email;
                changed = true;
            }
        }

        // A display name its owner claimed stays theirs (§2.4).
        if display_name.is_some() {
            let incoming = optional_text(
                display_name,
                TextKind::SingleLine,
                MAX_TITLE_CHARS,
                "user.display_name",
            )?;
            if self.profile.follow_claims(incoming.as_deref()) {
                self.display_name.clone_from(&self.profile.display_name);
                changed = true;
            }
        }

        Ok(changed)
    }

    /// Apply an edit the owner made, and keep the shown name in step.
    ///
    /// # Errors
    ///
    /// Refuses anything [`Profile::edit`] refuses.
    pub fn edit_profile(&mut self, edit: ProfileEdit) -> Result<()> {
        self.profile.edit(edit)?;
        self.display_name.clone_from(&self.profile.display_name);
        Ok(())
    }

    /// This user as a principal.
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal::User { id: self.id }
    }
}

/// A named machine identity (§5.2bis), the recommended owner for anything durable.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccount {
    /// Its name, and the id used everywhere it appears.
    pub id: ServiceAccountId,
    /// What clients show — "published by ArgoCD", not by an opaque id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What it is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// GitOps-declared or API-created (§11).
    pub origin: Origin,
    /// Who created it through the API (and may deactivate it); `None` for managed accounts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<UserId>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Set on deactivation, which revokes its keys and cuts its streams immediately (§8, T25).
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub disabled_at: Option<OffsetDateTime>,
}

impl ServiceAccount {
    /// Create one; `created_by` is `None` for reconciler-declared accounts.
    pub fn new(
        id: ServiceAccountId,
        display_name: Option<&str>,
        description: Option<&str>,
        origin: Origin,
        created_by: Option<UserId>,
        now: OffsetDateTime,
    ) -> Result<Self> {
        Ok(Self {
            id,
            created_by,
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

    /// Whether this caller may deactivate it.
    ///
    /// # Errors
    ///
    /// Refuses a managed account outright (§11), and somebody else's with
    /// [`DomainError::ScopeMissing`].
    pub fn ensure_may_disable(&self, caller: UserId) -> Result<()> {
        self.origin.ensure_mutable_through_api("service_account")?;

        if self.created_by == Some(caller) {
            Ok(())
        } else {
            Err(DomainError::ScopeMissing { scope: "manage" })
        }
    }
}

/// The public half of an API key: everything `GET /apikeys` may show (§5.2, T12).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    /// Identity of the key.
    pub id: ApiKeyId,
    /// The indexed lookup prefix used to find the key in one read.
    pub prefix: ApiKeyPrefix,
    /// A label so a human can tell two keys apart before revoking one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Who it acts as (§5.2).
    pub owner: ApiKeyOwner,
    /// What it may do.
    pub scopes: Vec<Scope>,
    /// Which channels it may do it in; empty means none.
    pub channels: Vec<ChannelId>,
    /// This key's own publish ceiling, narrowing the channel's (§8.2); set by platform admins only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish_per_minute: Option<u32>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Optional expiry; rotation is left to operator policy (§5.2).
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub expires_at: Option<OffsetDateTime>,
    /// Last successful authentication.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_used_at: Option<OffsetDateTime>,
    /// Set on revocation, which takes effect immediately.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub revoked_at: Option<OffsetDateTime>,
}

impl ApiKey {
    /// Whether this key may still be used; expiry and revocation get distinct codes (§5.2, T12).
    pub fn ensure_usable(&self, now: OffsetDateTime) -> Result<()> {
        if self.revoked_at.is_some_and(|at| now >= at) {
            return Err(DomainError::ApiKeyRevoked);
        }
        if self.expires_at.is_some_and(|at| now >= at) {
            return Err(DomainError::ApiKeyExpired);
        }
        Ok(())
    }

    /// Whether this key carries `scope` for `channel` (both are required).
    #[must_use]
    pub fn grants(&self, channel: &ChannelId, scope: Scope) -> bool {
        self.scopes.contains(&scope) && self.channels.contains(channel)
    }

    /// The principal a request carrying this key acts as (§5.2).
    #[must_use]
    pub fn principal(&self) -> Principal {
        self.owner.principal(self.id)
    }
}

/// The indexed lookup prefix of a key: the `<prefix>` of `wnk_<prefix>_<secret>`.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKeyPrefix(String);

impl ApiKeyPrefix {
    /// How long a prefix is.
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

/// A presented API key split into lookup prefix and secret; the cheap shape check
/// runs before any database or Argon2 work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedApiKey {
    /// The indexed lookup half.
    pub prefix: ApiKeyPrefix,
    /// The half that is hashed and compared. Never logged, never stored.
    pub secret: String,
}

impl PresentedApiKey {
    /// The scheme every key is written in (§5.2).
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
            publish_per_minute: None,
            created_at: now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        }
    }

    // --- T12 · API keys ---------------------------------------------------

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
            Some(UserId::generate()),
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

    #[test]
    fn an_absent_email_keeps_the_stored_one_and_an_unverified_one_clears_it() {
        let mut user = User::from_claims(
            UserId::generate(),
            "ak-user-42",
            Some("alice@weebo.poc"),
            None,
            now(),
        )
        .unwrap();

        assert!(!user.sync_from_claims(None, false, None).unwrap());
        assert_eq!(user.email.as_deref(), Some("alice@weebo.poc"));

        assert!(user.sync_from_claims(None, true, None).unwrap());
        assert_eq!(user.email, None);
        assert!(
            !user.sync_from_claims(None, true, None).unwrap(),
            "clearing what is already clear is not a change worth a write"
        );
    }
}
