//! Who is acting (§5): one typed value for all four identities, answering
//! attribution (§5.2) and whether the actor is a human reader (§5.2bis).

use core::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{DomainError, Result};
use crate::hygiene::{TextKind, required_text};
use crate::ids::{ApiKeyId, ChannelId, ServiceAccountId, UserId};
use crate::limits::MAX_TITLE_CHARS;

/// The discriminant of a [`Principal`], as it appears in an `author` object (§8.1).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A human authenticated through OIDC (design §5.1).
    User,
    /// A named machine identity (design §5.2bis).
    ServiceAccount,
    /// An anonymous key belonging to a channel rather than to anyone (design §5.2).
    ChannelKey,
    /// A CI job holding a short-lived federated token (design §5.3).
    CiJob,
}

impl fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::ServiceAccount => "service_account",
            Self::ChannelKey => "channel_key",
            Self::CiJob => "ci_job",
        })
    }
}

/// The identity behind a request, after authentication.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// A human, whether they arrived with an OIDC token or with a personal API key.
    User {
        /// Their account.
        id: UserId,
    },
    /// A bot with a name of its own.
    ServiceAccount {
        /// Its account.
        id: ServiceAccountId,
    },
    /// A key attached to a channel and to nobody else; use a service account when attribution matters (§5.2).
    ChannelKey {
        /// The channel that owns the key.
        channel: ChannelId,
        /// The key itself. Never the secret.
        key: ApiKeyId,
    },
    /// A CI job, identified by the claims of the token it exchanged (design §5.3).
    CiJob {
        /// The OIDC issuer that vouched for the job.
        issuer: Url,
        /// The `subject` claim, e.g. `repo:weebo/infra:ref:refs/heads/main`.
        subject: String,
    },
}

impl Principal {
    /// Build a CI job principal, sanitising the subject like any publisher text.
    pub fn ci_job(issuer: Url, subject: &str) -> Result<Self> {
        let subject = required_text(
            subject,
            TextKind::SingleLine,
            MAX_TITLE_CHARS,
            "principal.subject",
        )?;
        Ok(Self::CiJob { issuer, subject })
    }

    /// Which of the four this is.
    #[must_use]
    pub fn kind(&self) -> PrincipalKind {
        match self {
            Self::User { .. } => PrincipalKind::User,
            Self::ServiceAccount { .. } => PrincipalKind::ServiceAccount,
            Self::ChannelKey { .. } => PrincipalKind::ChannelKey,
            Self::CiJob { .. } => PrincipalKind::CiJob,
        }
    }

    /// A stable textual reference, unique within its kind (audit log, `author.id`).
    #[must_use]
    pub fn reference(&self) -> String {
        match self {
            Self::User { id } => id.to_string(),
            Self::ServiceAccount { id } => id.to_string(),
            Self::ChannelKey { key, .. } => key.to_string(),
            Self::CiJob { subject, .. } => subject.clone(),
        }
    }

    /// Whether this principal is a person; only humans count as readers (§5.2bis, T25).
    #[must_use]
    pub fn is_human(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// The user behind this principal, when there is one.
    #[must_use]
    pub fn as_user(&self) -> Option<UserId> {
        match self {
            Self::User { id } => Some(*id),
            _ => None,
        }
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelKey { channel, key } => write!(f, "channel_key {key} of {channel}"),
            other => write!(f, "{} {}", other.kind(), other.reference()),
        }
    }
}

/// A principal plus the display label clients show next to revisions, interactions and receipts (§2.4, §8.1).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// The identity itself.
    #[serde(flatten)]
    pub principal: Principal,
    /// The name to show; absent for principals without a profile (clients fall back to the reference).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl Author {
    /// An author with no profile attached yet.
    #[must_use]
    pub fn anonymous(principal: Principal) -> Self {
        Self {
            principal,
            display_name: None,
        }
    }

    /// An author with a display name, sanitised like any other text.
    pub fn named(principal: Principal, display_name: &str) -> Result<Self> {
        let display_name = required_text(
            display_name,
            TextKind::SingleLine,
            MAX_TITLE_CHARS,
            "author.display_name",
        )?;
        Ok(Self {
            principal,
            display_name: Some(display_name),
        })
    }

    /// What a client shows: the display name, or the reference.
    #[must_use]
    pub fn label(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.principal.reference())
    }
}

/// The owner of an API key, which decides publish attribution (§5.2).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "owner", rename_all = "snake_case")]
pub enum ApiKeyOwner {
    /// A personal key, for one human's scripting; never for team bots (§5.2).
    User {
        /// The owning user.
        id: UserId,
    },
    /// A service account key — the recommended shape for anything durable.
    ServiceAccount {
        /// The owning service account.
        id: ServiceAccountId,
    },
    /// A channel key: owned by the channel, attributable to nothing finer.
    Channel {
        /// The owning channel.
        id: ChannelId,
    },
}

impl ApiKeyOwner {
    /// The principal a request carrying this key acts as (design §5.2).
    #[must_use]
    pub fn principal(&self, key: ApiKeyId) -> Principal {
        match self {
            Self::User { id } => Principal::User { id: *id },
            Self::ServiceAccount { id } => Principal::ServiceAccount { id: id.clone() },
            Self::Channel { id } => Principal::ChannelKey {
                channel: id.clone(),
                key,
            },
        }
    }
}

/// Refuse an author that does not match the authenticated principal.
pub fn ensure_matches(author: &Author, principal: &Principal) -> Result<()> {
    if &author.principal == principal {
        Ok(())
    } else {
        Err(DomainError::Malformed {
            field: "author",
            reason: "does not match the authenticated principal",
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    fn user() -> UserId {
        UserId::generate()
    }

    #[test]
    fn key_attribution_follows_the_owner() {
        let key = ApiKeyId::generate();

        let owner = ApiKeyOwner::User { id: user() };
        assert_eq!(owner.principal(key).kind(), PrincipalKind::User);

        let owner = ApiKeyOwner::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        assert_eq!(owner.principal(key).kind(), PrincipalKind::ServiceAccount);

        // A channel key is attributable to the key and nothing more.
        let owner = ApiKeyOwner::Channel {
            id: ChannelId::parse("ci-infra").unwrap(),
        };
        let principal = owner.principal(key);
        assert_eq!(principal.kind(), PrincipalKind::ChannelKey);
        assert_eq!(principal.reference(), key.to_string());
    }

    #[test]
    fn only_users_count_as_humans() {
        assert!(Principal::User { id: user() }.is_human());
        assert!(
            !Principal::ServiceAccount {
                id: ServiceAccountId::parse("bot-argocd").unwrap()
            }
            .is_human(),
            "a bot that read a channel is not a person who was informed"
        );
    }

    #[test]
    fn an_author_falls_back_to_its_reference() {
        let principal = Principal::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        assert_eq!(Author::anonymous(principal.clone()).label(), "bot-argocd");
        assert_eq!(
            Author::named(principal, "ArgoCD").unwrap().label(),
            "ArgoCD"
        );
    }

    #[test]
    fn a_ci_job_is_identified_by_its_claims() {
        let issuer = Url::parse("https://forgejo.weebo.poc").unwrap();
        let job =
            Principal::ci_job(issuer.clone(), "repo:weebo/infra:ref:refs/heads/main").unwrap();

        assert_eq!(job.kind(), PrincipalKind::CiJob);
        assert_eq!(job.reference(), "repo:weebo/infra:ref:refs/heads/main");
        assert!(
            !job.is_human(),
            "a pipeline is not somebody who was informed"
        );
        assert_eq!(job.as_user(), None);

        // Subject text is publisher-influenced and sanitised.
        assert!(Principal::ci_job(issuer.clone(), "\u{1b}[2J").is_err());
        assert!(Principal::ci_job(issuer, "   ").is_err());
    }

    #[test]
    fn a_principal_renders_readably_for_logs_and_audit() {
        let key = ApiKeyId::generate();
        let channel = ChannelId::parse("ci-infra").unwrap();
        let anonymous = Principal::ChannelKey { channel, key };
        assert_eq!(
            anonymous.to_string(),
            format!("channel_key {key} of ci-infra"),
            "an audit line has to say which key, and of what"
        );

        let bot = Principal::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        assert_eq!(bot.to_string(), "service_account bot-argocd");

        let id = user();
        assert_eq!(Principal::User { id }.to_string(), format!("user {id}"));
        assert_eq!(Principal::User { id }.as_user(), Some(id));
    }

    #[test]
    fn an_author_round_trips_through_json() {
        // Authors ride along in every stream event and every stored revision.
        let author = Author::named(
            Principal::ServiceAccount {
                id: ServiceAccountId::parse("bot-argocd").unwrap(),
            },
            "ArgoCD",
        )
        .unwrap();

        let json = serde_json::to_string(&author).unwrap();
        assert!(json.contains("\"kind\":\"service_account\""));
        assert_eq!(serde_json::from_str::<Author>(&json).unwrap(), author);

        // No display name means no key on the wire, and clients fall back.
        let bare = Author::anonymous(Principal::User { id: user() });
        assert!(
            !serde_json::to_string(&bare)
                .unwrap()
                .contains("display_name")
        );
    }

    #[test]
    fn a_display_name_is_sanitised_like_any_other_text() {
        let author =
            Author::named(Principal::User { id: user() }, "\u{1b}[31mAlice\u{1b}[0m").unwrap();
        assert_eq!(author.label(), "Alice");
        assert!(Author::named(Principal::User { id: user() }, "\u{1b}[2J").is_err());
    }

    #[test]
    fn an_author_cannot_impersonate_another_principal() {
        let mine = Principal::User { id: user() };
        let theirs = Principal::User { id: user() };
        assert!(ensure_matches(&Author::anonymous(mine.clone()), &mine).is_ok());
        assert!(ensure_matches(&Author::anonymous(theirs), &mine).is_err());
    }
}
