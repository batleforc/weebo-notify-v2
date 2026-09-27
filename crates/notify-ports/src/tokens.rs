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

use notify_domain::{ChannelId, FederatedClaims, FederatedGrant, Scope};
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
    /// The channels it resolved to, so the caller can be told what it actually got
    /// rather than having to decode its own token to find out.
    pub channels: Vec<ChannelId>,
    /// The scopes it resolved to.
    pub scopes: Vec<Scope>,
}

/// Exchanging a forge's ID token for one of ours (design §5.3).
///
/// # What this port hands back, and why it changed in M4.1
///
/// It used to return [`VerifiedClaims`] — the shape the human login path uses —
/// while its own documentation promised the two checks scenario T11 requires. That
/// was not a small mismatch: `VerifiedClaims` carries neither `aud` nor `jti`, so
/// **an implementation of this trait could not have performed either check, and no
/// caller could have performed them afterwards.** The rules were written in prose
/// beside a type that made them unrepresentable.
///
/// It now returns [`FederatedClaims`], which carries both, and the decision itself
/// lives in [`notify_domain::TrustPolicies::decide`] where it is tested without a
/// network. A verifier's remaining job is the one thing only it can do: check the
/// signature against the issuer's JWKS and report the claims faithfully.
#[async_trait::async_trait]
pub trait FederationVerifier: Send + Sync {
    /// Verify an incoming CI token against its issuer's JWKS.
    ///
    /// Reports claims; does not decide. In particular it does **not** filter on
    /// `aud` — it reports the audiences, and the domain refuses. One place where
    /// "this token is not for us" is decided means one place where that can be got
    /// wrong, and it is a place with tests and no I/O.
    async fn verify_ci_token(
        &self,
        token: &str,
    ) -> PortResult<Result<FederatedClaims, TokenRejection>>;

    /// Mint our own short-lived token for a grant that matched a policy.
    ///
    /// The channels are the ones the grant resolved to, already intersected with
    /// what exists — a token naming `ci-*` would force every later request to
    /// re-run the pattern match, and the two runs would eventually disagree.
    ///
    /// Each channel travels with its own scopes: two policies granting different
    /// scopes on different channels must not add up to both everywhere.
    async fn mint(
        &self,
        grant: &FederatedGrant,
        grants: &[(ChannelId, Vec<Scope>)],
    ) -> PortResult<MintedToken>;

    /// Verify a token *we* minted, presented on a later request.
    ///
    /// On this port rather than on [`TokenVerifier`] because it is the same key
    /// material as [`mint`]: whoever signs is whoever checks, and splitting them
    /// across two adapters would mean two places that have to agree on an algorithm
    /// and a key.
    ///
    /// [`mint`]: FederationVerifier::mint
    async fn verify_minted(&self, token: &str) -> PortResult<Result<MintedClaims, TokenRejection>>;
}

/// What one of our own minted tokens says, once verified.
///
/// It carries the resolved channels rather than the policy that produced them, and
/// that is the whole point of minting: authorisation for the next fifteen minutes
/// is a fact written down once, not a pattern match re-run per request against
/// configuration that may have been reloaded since (design §11, `reload`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedClaims {
    /// The forge that vouched for the job, kept for the audit log — the authority
    /// behind this token did not originate here.
    pub issuer: Url,
    /// The `subject` claim of the original ID token, which becomes the `CiJob`
    /// principal (design §5.1).
    pub subject: String,
    /// Exactly the channels this token may act in, each with exactly what it may
    /// do there.
    pub grants: Vec<(ChannelId, Vec<Scope>)>,
    /// When it stops working. Fifteen minutes at most (design §5.3).
    pub expires_at: OffsetDateTime,
}

/// Remembering that a one-shot token has been used (design §5.3, scenario T11).
///
/// Deliberately **not** [`IdempotencyStore`]: that one replays the stored response
/// on a repeat, which is right for a publish a client retried after a timeout and
/// exactly wrong here. Replaying would hand a second copy of the minted token to
/// whoever presented the leaked ID token again — turning "worth one exchange" into
/// "worth as many as you like, all identical". A replayed `jti` is refused.
///
/// [`IdempotencyStore`]: crate::idempotency::IdempotencyStore
#[async_trait::async_trait]
pub trait ReplayGuard: Send + Sync {
    /// Record a token id, and say whether it was new.
    ///
    /// `false` means it has been seen: the caller refuses. The check and the record
    /// are one call because two calls are a race — two jobs presenting the same
    /// leaked token at once would both read "unseen" and both be minted a token.
    ///
    /// `until` is the incoming token's own expiry: past it a replay fails on expiry
    /// anyway, so nothing has to be remembered longer.
    async fn remember(&self, jti: &str, until: OffsetDateTime) -> PortResult<bool>;

    /// Drop entries past their expiry. Leader-only (design §11).
    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64>;
}
