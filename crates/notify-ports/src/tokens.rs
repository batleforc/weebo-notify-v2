//! Verifying the tokens people and jobs arrive with (design §5.1, §5.3).
//!
//! Two different jobs behind one small surface:
//!
//! * **User tokens** are OIDC access tokens, validated against the issuer's JWKS.
//!   No session is stored anywhere (design §5.1), so this is the entire
//!   authentication path for a human: no lookup, nothing to replicate, and an IdP
//!   outage degrades to "nobody can log in fresh" rather than "nobody can use the
//!   product".
//! * **CI tokens** are federated: a job presents the ID token its forge minted, and
//!   we exchange it for a short-lived token of ours (design §5.3). No long-lived
//!   secret ever sits in a pipeline.
//!
//! The hardening on that exchange is a contract, not an implementation detail, so
//! it is stated on the trait: audience must name us, subject globs are anchored,
//! and a `jti` is single-use.

use notify_domain::{ChannelId, Scope};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;

/// The claims we act on, once a token has been verified.
///
/// Not the whole token: a verifier hands back what authorisation needs and drops
/// the rest, so no downstream code can start depending on a claim a different IdP
/// will not send.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedClaims {
    /// The `iss` claim.
    pub issuer: Url,
    /// The `sub` claim — the stable identity (design §5.1).
    pub subject: String,
    /// The `email` claim, when the IdP releases one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The `name` claim, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The `groups` claim, mapped to channel access and to the platform role
    /// (design §5.4, §11).
    ///
    /// Empty is a supported answer, not a broken one: Dex does not send groups at
    /// all, and the required behaviour is graceful degradation to individual
    /// bindings rather than a crash or a default grant (scenario T29).
    #[serde(default)]
    pub groups: Vec<String>,
    /// When the token stops being valid.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Validating an OIDC access token.
#[async_trait::async_trait]
pub trait TokenVerifier: Send + Sync {
    /// Verify signature, issuer, audience and expiry, and return the claims.
    ///
    /// Everything that fails — bad signature, wrong issuer, expired, unknown key —
    /// comes back as [`notify_domain::DomainError`] rather than as a port error:
    /// a bad token is the caller's problem and a 401, not an outage.
    async fn verify(&self, token: &str) -> PortResult<Result<VerifiedClaims, TokenRejection>>;

    /// Whether the JWKS cache is warm, for `/readyz` (design §13).
    async fn health(&self) -> PortResult<()>;
}

/// Why a token was refused.
///
/// Distinguished so the log can say which, while the response says only `401`:
/// telling a caller *why* their token failed is a gift to somebody probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRejection {
    /// Signature did not verify, or the key is unknown.
    BadSignature,
    /// The `iss` claim is not the configured issuer.
    WrongIssuer,
    /// The `aud` claim does not name us.
    WrongAudience,
    /// Past its `exp`.
    Expired,
    /// Not a JWT, or missing a required claim.
    Malformed,
}

impl TokenRejection {
    /// A stable label for logs and metrics. Never sent to the caller.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadSignature => "bad_signature",
            Self::WrongIssuer => "wrong_issuer",
            Self::WrongAudience => "wrong_audience",
            Self::Expired => "expired",
            Self::Malformed => "malformed",
        }
    }
}

/// A trust policy: which CI jobs may act, and as what (design §5.3, §11).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPolicy {
    /// The forge that vouches for the job.
    pub issuer: Url,
    /// The `subject` pattern, e.g. `repo:weebo/*`.
    ///
    /// Globs are matched **anchored, over the whole string**. Unanchored, the very
    /// natural-looking `repo:weebo/*` also matches `repo:weebo-evil/x`, and the
    /// mistake is invisible in review (design §5.3, scenario T11).
    pub subject: String,
    /// The channels it may act in — patterns, e.g. `ci-*`.
    pub channels: Vec<String>,
    /// What it may do there.
    pub scopes: Vec<Scope>,
}

/// A minted CI token.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintedToken {
    /// The token itself, Ed25519-signed by us.
    pub token: String,
    /// When it expires — 15 minutes at most (design §5.3).
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// The channels it resolved to, so the caller can be told what it actually got
    /// rather than having to decode its own token to find out.
    pub channels: Vec<ChannelId>,
    /// The scopes it resolved to.
    pub scopes: Vec<Scope>,
}

/// Exchanging a forge's ID token for one of ours (design §5.3).
#[async_trait::async_trait]
pub trait FederationVerifier: Send + Sync {
    /// Verify an incoming CI token against the issuer's JWKS.
    ///
    /// Two checks beyond an ordinary verification, both of which have been real
    /// vulnerabilities elsewhere:
    ///
    /// * the `aud` must name us, so a token minted for another service cannot be
    ///   replayed here even when issuer and subject match;
    /// * the `jti` is remembered until its `exp`, so one ID token exchanges exactly
    ///   once (scenario T11).
    async fn verify_ci_token(
        &self,
        token: &str,
    ) -> PortResult<Result<VerifiedClaims, TokenRejection>>;

    /// Mint our own short-lived token for claims that matched a policy.
    async fn mint(
        &self,
        claims: &VerifiedClaims,
        channels: &[ChannelId],
        scopes: &[Scope],
    ) -> PortResult<MintedToken>;
}
