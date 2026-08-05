//! The [`FederationVerifier`]: several forges in, one short-lived token out
//! (design §5.3).
//!
//! # Why this lives beside the OIDC verifier
//!
//! A forge's ID token is an OIDC token. Everything §5.1's verifier already refuses
//! to do — trust the header's `alg`, refetch a key set on demand, follow a
//! discovery document that names a different issuer — applies here unchanged, and
//! §4 puts "Authentik **and federated CI issuers**" in this crate for that reason.
//! So a [`OidcVerifier`] is built per trusted forge and the hardening is inherited
//! rather than re-implemented.
//!
//! # The issuer allowlist is the trust policies, and that is a security property
//!
//! Selecting which JWKS to fetch means reading `iss` out of a token before its
//! signature is checked. That is unavoidable — you cannot verify a signature
//! without first knowing whose key to use — and it is only safe because the set of
//! issuers is **fixed at startup from the configured trust policies**. An issuer
//! nobody wrote into `[[trust_policy]]` is refused before any network call, so a
//! forged `iss` cannot make this process fetch a URL of an attacker's choosing.
//! Without that, the endpoint would be a server-side request forgery primitive
//! reachable without a credential.
//!
//! # Minting
//!
//! Ed25519, fifteen minutes, and the resolved channels written into the token
//! (§5.3). The key is configuration rather than something generated at boot: a
//! replica that mints what its neighbour cannot verify is an intermittent failure
//! that looks like a flaky pipeline (§11 `[ha]`).

use std::collections::HashMap;
use std::sync::Arc;

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use notify_domain::{ChannelId, FederatedClaims, FederatedGrant, Scope};
use notify_ports::{
    Clock, FederationVerifier, MintedClaims, MintedToken, PortError, PortResult, TokenRejection,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use url::Url;

use crate::config::OidcConfig;
use crate::verifier::OidcVerifier;

/// How long a minted token lives. The ceiling §5.3 sets, and the value: a CI job
/// that outlives it re-exchanges, which costs one request.
const TOKEN_TTL: Duration = Duration::minutes(15);

/// What this instance calls itself in the tokens it signs.
const SELF_ISSUER: &str = "weebo-notify";

/// Everything needed to sign and check our own tokens.
pub struct FederationTokens {
    /// One verifier per issuer named by a trust policy. Fixed at startup — see the
    /// module docs.
    issuers: HashMap<String, OidcVerifier>,
    encoding: EncodingKey,
    decoding: DecodingKey,
    audience: String,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for FederationTokens {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FederationTokens")
            .field("issuers", &self.issuers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl FederationTokens {
    /// Build from the signing key and the issuers the policies trust.
    ///
    /// # Errors
    ///
    /// If the key is not a usable Ed25519 PKCS#8 PEM, or an issuer is not a URL.
    /// Both are operator mistakes, and both are better reported before the socket is
    /// bound than on the first pipeline that tries to publish.
    pub fn new(
        signing_key_pem: &[u8],
        issuers: &[String],
        audience: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> PortResult<Self> {
        let encoding = EncodingKey::from_ed_pem(signing_key_pem)
            .map_err(|error| PortError::corrupt("federation.signing_key", error))?;
        let decoding = DecodingKey::from_ed_pem(&public_pem(signing_key_pem)?)
            .map_err(|error| PortError::corrupt("federation.signing_key", error))?;

        let mut verifiers = HashMap::new();
        for issuer in issuers {
            let url = Url::parse(issuer)
                .map_err(|error| PortError::corrupt("trust_policy.issuer", error))?;
            // `client_id` is what an OIDC config calls its audience, and this one
            // never uses it: `verify_federated` leaves `aud` to the domain (M4.1).
            let config = OidcConfig::new(url, SELF_ISSUER);
            verifiers.insert(
                issuer.trim_end_matches('/').to_owned(),
                OidcVerifier::new(config, clock.clone())?,
            );
        }

        Ok(Self {
            issuers: verifiers,
            encoding,
            decoding,
            audience: audience.into(),
            clock,
        })
    }

    /// Which forge a token claims to come from, before anything is verified.
    ///
    /// Reads `iss` from the unverified payload for one purpose only — choosing a
    /// key set — and returns `None` unless that issuer is one an operator wrote
    /// into a trust policy. See the module docs: this is the check that keeps an
    /// unauthenticated endpoint from fetching arbitrary URLs.
    fn issuer_of(&self, token: &str) -> Option<&OidcVerifier> {
        let payload = token.split('.').nth(1)?;
        let decoded = base64_url_decode(payload)?;
        let claims: UnverifiedIssuer = serde_json::from_slice(&decoded).ok()?;
        self.issuers.get(claims.iss.trim_end_matches('/'))
    }
}

/// Just enough of a payload to route it, and nothing that is acted on.
#[derive(Deserialize)]
struct UnverifiedIssuer {
    iss: String,
}

/// The payload of one of our own tokens.
#[derive(Debug, Serialize, Deserialize)]
struct SelfSigned {
    iss: String,
    /// The forge that vouched for the job, kept so the audit log can say where the
    /// authority came from.
    forge: String,
    sub: String,
    aud: String,
    exp: i64,
    channels: Vec<String>,
    scopes: Vec<String>,
}

#[async_trait::async_trait]
impl FederationVerifier for FederationTokens {
    async fn verify_ci_token(
        &self,
        token: &str,
    ) -> PortResult<Result<FederatedClaims, TokenRejection>> {
        let Some(verifier) = self.issuer_of(token) else {
            // Same answer as a bad signature, deliberately: distinguishing "we do
            // not trust that forge" from "your signature is wrong" tells whoever is
            // probing which issuers are configured.
            return Ok(Err(TokenRejection::WrongIssuer));
        };
        verifier.verify_federated(token).await
    }

    async fn mint(
        &self,
        grant: &FederatedGrant,
        channels: &[ChannelId],
        scopes: &[Scope],
    ) -> PortResult<MintedToken> {
        let expires_at = self.clock.now() + TOKEN_TTL;
        let payload = SelfSigned {
            iss: SELF_ISSUER.to_owned(),
            forge: grant.issuer.clone(),
            sub: grant.subject.clone(),
            aud: self.audience.clone(),
            exp: expires_at.unix_timestamp(),
            channels: channels.iter().map(ToString::to_string).collect(),
            scopes: scopes
                .iter()
                .copied()
                .map(scope_name)
                .map(str::to_owned)
                .collect(),
        };

        let token = encode(&Header::new(Algorithm::EdDSA), &payload, &self.encoding)
            .map_err(|error| PortError::corrupt("federation.signing_key", error))?;

        Ok(MintedToken {
            token,
            expires_at,
            channels: channels.to_vec(),
            scopes: scopes.to_vec(),
        })
    }

    async fn verify_minted(&self, token: &str) -> PortResult<Result<MintedClaims, TokenRejection>> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[SELF_ISSUER]);
        validation.set_audience(&[self.audience.as_str()]);
        // Checked below against the injected clock, like every other deadline here.
        validation.validate_exp = false;

        let Ok(data) = decode::<SelfSigned>(token, &self.decoding, &validation) else {
            // Every failure is the same answer: this is called on **every** bearer
            // request as the first of two attempts (§5.1 auth path), and an OIDC
            // access token failing here is the ordinary case, not an incident.
            return Ok(Err(TokenRejection::BadSignature));
        };
        let payload = data.claims;

        let Ok(expires_at) = OffsetDateTime::from_unix_timestamp(payload.exp) else {
            return Ok(Err(TokenRejection::Malformed));
        };
        if expires_at <= self.clock.now() {
            return Ok(Err(TokenRejection::Expired));
        }

        let Ok(issuer) = Url::parse(&payload.forge) else {
            return Ok(Err(TokenRejection::Malformed));
        };
        let mut channels = Vec::with_capacity(payload.channels.len());
        for id in &payload.channels {
            let Ok(channel) = ChannelId::parse(id) else {
                return Ok(Err(TokenRejection::Malformed));
            };
            channels.push(channel);
        }
        let mut scopes = Vec::with_capacity(payload.scopes.len());
        for name in &payload.scopes {
            let Some(scope) = scope_from(name) else {
                // A scope this build does not know is not "ignore it": a token
                // minted by a newer replica must not silently authorise less.
                return Ok(Err(TokenRejection::Malformed));
            };
            scopes.push(scope);
        }

        Ok(Ok(MintedClaims {
            issuer,
            subject: payload.sub,
            channels,
            scopes,
            expires_at,
        }))
    }
}

/// The scope names written into a token.
///
/// Spelled out rather than derived from `serde`: these strings are a wire format
/// that outlives a process — a token minted before a rename is presented after it —
/// so the mapping is somewhere a rename has to touch.
const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Publish => "publish",
        Scope::Subscribe => "subscribe",
        Scope::Manage => "manage",
    }
}

fn scope_from(name: &str) -> Option<Scope> {
    match name {
        "publish" => Some(Scope::Publish),
        "subscribe" => Some(Scope::Subscribe),
        "manage" => Some(Scope::Manage),
        _ => None,
    }
}

/// The SPKI PEM of the public half, which is what a verifier loads.
///
/// Derived from the private key rather than configured separately: two files that
/// have to match are two files that can stop matching, and the pairing is not
/// something an operator should have to get right.
fn public_pem(private_pem: &[u8]) -> PortResult<Vec<u8>> {
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::DecodePrivateKey as _;
    use ed25519_dalek::pkcs8::spki::EncodePublicKey as _;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

    let pem = core::str::from_utf8(private_pem)
        .map_err(|error| PortError::corrupt("federation.signing_key", error))?;
    let signing = SigningKey::from_pkcs8_pem(pem)
        .map_err(|error| PortError::corrupt("federation.signing_key", error))?;

    Ok(signing
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|error| PortError::corrupt("federation.signing_key", error))?
        .into_bytes())
}

/// Base64url without padding, as JWT parts are encoded.
fn base64_url_decode(raw: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_ports::SystemClock;

    use super::*;

    /// A key built from a fixed scalar, so nothing here is a secret a scanner or a
    /// reader could mistake for one — the convention M3.5 established for
    /// `push-webpush` after gitleaks found a real-looking key in its tests.
    fn signing_pem() -> Vec<u8> {
        use ed25519_dalek::SigningKey;
        use ed25519_dalek::pkcs8::EncodePrivateKey as _;
        use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

        SigningKey::from_bytes(&[7u8; 32])
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .as_bytes()
            .to_vec()
    }

    fn tokens() -> FederationTokens {
        FederationTokens::new(
            &signing_pem(),
            &["https://forgejo.weebo.poc".to_owned()],
            "weebo-notify",
            Arc::new(SystemClock),
        )
        .unwrap()
    }

    fn grant() -> FederatedGrant {
        FederatedGrant {
            issuer: "https://forgejo.weebo.poc".to_owned(),
            subject: "repo:weebo/infra:ref:refs/heads/main".to_owned(),
            allowances: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_minted_token_reads_back_as_exactly_what_was_minted() {
        let tokens = tokens();
        let channels = vec![ChannelId::parse("ci-infra").unwrap()];

        let minted = tokens
            .mint(&grant(), &channels, &[Scope::Publish])
            .await
            .unwrap();
        let claims = tokens.verify_minted(&minted.token).await.unwrap().unwrap();

        assert_eq!(claims.subject, "repo:weebo/infra:ref:refs/heads/main");
        assert_eq!(claims.channels, channels);
        assert_eq!(claims.scopes, vec![Scope::Publish]);
        assert_eq!(
            claims.issuer.as_str(),
            "https://forgejo.weebo.poc/",
            "the forge that vouched travels with the token, for the audit log"
        );
    }

    #[tokio::test]
    async fn a_token_signed_by_somebody_else_is_refused() {
        let ours = tokens();
        let theirs = FederationTokens::new(
            &{
                use ed25519_dalek::SigningKey;
                use ed25519_dalek::pkcs8::EncodePrivateKey as _;
                use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
                SigningKey::from_bytes(&[9u8; 32])
                    .to_pkcs8_pem(LineEnding::LF)
                    .unwrap()
                    .as_bytes()
                    .to_vec()
            },
            &[],
            "weebo-notify",
            Arc::new(SystemClock),
        )
        .unwrap();

        let minted = theirs
            .mint(
                &grant(),
                &[ChannelId::parse("ci-infra").unwrap()],
                &[Scope::Publish],
            )
            .await
            .unwrap();

        assert!(ours.verify_minted(&minted.token).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn an_oidc_access_token_is_not_mistaken_for_one_of_ours() {
        // The first of the two attempts every bearer request makes (§5.1): it has to
        // decline quietly rather than fail the request.
        let tokens = tokens();

        assert!(
            tokens
                .verify_minted("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.not-a-signature")
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_issuer_no_policy_names_is_refused_before_any_network_call() {
        // The SSRF guard: `iss` is attacker-supplied, and it selects a URL to fetch.
        let tokens = tokens();
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"iss":"https://forge.attacker.example"}"#,
        );
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.signature");

        assert_eq!(
            tokens.verify_ci_token(&token).await.unwrap().unwrap_err(),
            TokenRejection::WrongIssuer
        );
    }

    #[test]
    fn every_scope_survives_a_round_trip_through_the_wire_name() {
        for scope in [Scope::Publish, Scope::Subscribe, Scope::Manage] {
            assert_eq!(scope_from(scope_name(scope)), Some(scope));
        }
    }
}
