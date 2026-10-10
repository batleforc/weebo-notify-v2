//! Verifying the tokens people and jobs arrive with (§5.1, §5.3): stateless OIDC
//! access tokens for users, and forge ID tokens exchanged for short-lived CI tokens.

use notify_domain::{ChannelId, FederatedClaims, FederatedGrant, Scope};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;

/// The claims we act on once a token has been verified.
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
    /// The token *did* carry an `email`, and it was dropped as unverified.
    ///
    /// Not the same as no claim at all: absent means "this client asked for
    /// fewer scopes" and leaves the stored address alone, while this means the
    /// provider now vouches for no address, so the stored one — which email
    /// bindings grant access by (§11) — has to go.
    #[serde(default)]
    pub email_unverified: bool,
    /// The `name` claim, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The `groups` claim, mapped to channel access and platform role (§5.4, §11);
    /// may be empty (Dex sends none, T29).
    #[serde(default)]
    pub groups: Vec<String>,
    /// When the token stops being valid.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Validating an OIDC access token.
#[async_trait::async_trait]
pub trait TokenVerifier: Send + Sync {
    /// Verify signature, issuer, audience and expiry, and return the claims. A bad
    /// token is `Ok(Err(_))`, a 401, not an outage.
    async fn verify(&self, token: &str) -> PortResult<Result<VerifiedClaims, TokenRejection>>;

    /// Whether the JWKS cache is warm, for `/readyz` (design §13).
    async fn health(&self) -> PortResult<()>;
}

/// Why a token was refused; logged, never told to the caller (who only gets `401`).
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
    /// Before its `nbf`.
    NotYetValid,
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
            Self::NotYetValid => "not_yet_valid",
            Self::Malformed => "malformed",
        }
    }
}

impl MintedToken {
    /// Build the response for `grants`: the channels, and the union of the scopes —
    /// what the caller is told it got, not what authorises it.
    #[must_use]
    pub fn new(
        token: String,
        expires_at: OffsetDateTime,
        grants: &[(ChannelId, Vec<Scope>)],
    ) -> Self {
        let mut scopes: Vec<Scope> = grants.iter().flat_map(|(_, s)| s.iter().copied()).collect();
        scopes.sort_unstable();
        scopes.dedup();
        Self {
            token,
            expires_at,
            channels: grants.iter().map(|(channel, _)| channel.clone()).collect(),
            scopes,
        }
    }
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
    /// The channels it resolved to.
    pub channels: Vec<ChannelId>,
    /// The scopes it resolved to.
    pub scopes: Vec<Scope>,
}

/// Exchanging a forge's ID token for one of ours (§5.3). The verifier only checks
/// signatures; [`notify_domain::TrustPolicies::decide`] makes the decision.
#[async_trait::async_trait]
pub trait FederationVerifier: Send + Sync {
    /// Verify an incoming CI token against its issuer's JWKS and report its claims.
    /// Does **not** filter on `aud`: the domain refuses.
    async fn verify_ci_token(
        &self,
        token: &str,
    ) -> PortResult<Result<FederatedClaims, TokenRejection>>;

    /// Mint our own short-lived token for a matched grant, carrying each resolved
    /// channel with its own scopes (never the union everywhere).
    async fn mint(
        &self,
        grant: &FederatedGrant,
        grants: &[(ChannelId, Vec<Scope>)],
    ) -> PortResult<MintedToken>;

    /// Verify a token we minted; lives beside [`FederationVerifier::mint`] because
    /// it uses the same key material.
    async fn verify_minted(&self, token: &str) -> PortResult<Result<MintedClaims, TokenRejection>>;
}

/// What one of our own minted tokens says, once verified: resolved channels, not
/// the policy that produced them (§11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedClaims {
    /// The forge that vouched for the job, kept for the audit log.
    pub issuer: Url,
    /// The original `subject`, which becomes the `CiJob` principal (§5.1).
    pub subject: String,
    /// Exactly the channels this token may act in, each with its scopes.
    pub grants: Vec<(ChannelId, Vec<Scope>)>,
    /// When it stops working. Fifteen minutes at most (design §5.3).
    pub expires_at: OffsetDateTime,
}

/// Single-use `jti` tracking (§5.3, T11). Not an idempotency store: a replayed
/// `jti` must be refused, never answered with another token.
#[async_trait::async_trait]
pub trait ReplayGuard: Send + Sync {
    /// Atomically record a token id; `false` means already seen and must be refused.
    /// `until` is the incoming token's expiry plus the verifier's leeway.
    async fn remember(&self, jti: &str, until: OffsetDateTime) -> PortResult<bool>;

    /// Drop entries past their expiry. Leader-only (design §11).
    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64>;
}
