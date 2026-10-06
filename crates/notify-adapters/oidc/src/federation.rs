//! The [`FederationVerifier`] (design §5.3): forge ID tokens in, a short-lived Ed25519 token out.
//! Each forge gets an [`OidcVerifier`], inheriting its hardening. Security: the issuer allowlist is
//! fixed at startup from trust policies, so an unverified `iss` can never choose a URL to fetch (SSRF).

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

/// How long a minted token lives (§5.3's ceiling); a longer job re-exchanges.
const TOKEN_TTL: Duration = Duration::minutes(15);

/// What this instance calls itself in the tokens it signs.
const SELF_ISSUER: &str = "weebo-notify";

/// Everything needed to sign and check our own tokens.
pub struct FederationTokens {
    /// One verifier per trusted issuer, fixed at startup (the SSRF guard).
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
    /// Fails at startup if the key is not an Ed25519 PKCS#8 PEM or an issuer is not a URL.
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
            // Unused: `verify_federated` leaves `aud` to the domain.
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

    /// Which trusted forge a token claims, from its unverified `iss`; `None` for any issuer not in a policy.
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
    /// The forge that vouched for the job, for the audit log.
    forge: String,
    sub: String,
    aud: String,
    exp: i64,
    /// Each channel with its own scopes, kept paired (see `FederationVerifier::mint`).
    grants: Vec<(String, Vec<String>)>,
}

#[async_trait::async_trait]
impl FederationVerifier for FederationTokens {
    async fn verify_ci_token(
        &self,
        token: &str,
    ) -> PortResult<Result<FederatedClaims, TokenRejection>> {
        let Some(verifier) = self.issuer_of(token) else {
            // Same answer as a bad signature, so probing cannot enumerate trusted issuers.
            return Ok(Err(TokenRejection::WrongIssuer));
        };
        verifier.verify_federated(token).await
    }

    async fn mint(
        &self,
        grant: &FederatedGrant,
        grants: &[(ChannelId, Vec<Scope>)],
    ) -> PortResult<MintedToken> {
        let expires_at = self.clock.now() + TOKEN_TTL;
        let payload = SelfSigned {
            iss: SELF_ISSUER.to_owned(),
            forge: grant.issuer.clone(),
            sub: grant.subject.clone(),
            aud: self.audience.clone(),
            exp: expires_at.unix_timestamp(),
            grants: grants
                .iter()
                .map(|(channel, scopes)| {
                    (
                        channel.to_string(),
                        scopes
                            .iter()
                            .copied()
                            .map(scope_name)
                            .map(str::to_owned)
                            .collect(),
                    )
                })
                .collect(),
        };

        let token = encode(&Header::new(Algorithm::EdDSA), &payload, &self.encoding)
            .map_err(|error| PortError::corrupt("federation.signing_key", error))?;

        Ok(MintedToken::new(token, expires_at, grants))
    }

    async fn verify_minted(&self, token: &str) -> PortResult<Result<MintedClaims, TokenRejection>> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[SELF_ISSUER]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.validate_exp = false;

        let Ok(data) = decode::<SelfSigned>(token, &self.decoding, &validation) else {
            // Every failure is a quiet `None`: OIDC tokens land here first on every bearer request.
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
        let mut grants = Vec::with_capacity(payload.grants.len());
        for (id, names) in &payload.grants {
            let Ok(channel) = ChannelId::parse(id) else {
                return Ok(Err(TokenRejection::Malformed));
            };
            let mut scopes = Vec::with_capacity(names.len());
            for name in names {
                let Some(scope) = scope_from(name) else {
                    // An unknown scope fails: a newer replica's token must not silently authorise less.
                    return Ok(Err(TokenRejection::Malformed));
                };
                scopes.push(scope);
            }
            grants.push((channel, scopes));
        }

        Ok(Ok(MintedClaims {
            issuer,
            subject: payload.sub,
            grants,
            expires_at,
        }))
    }
}

/// The scope names written into a token; spelled out because they outlive a rename.
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

/// The SPKI PEM of the public half, derived so it can never mismatch the private key.
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

    /// A key from a fixed scalar, so nothing here looks like a secret to scanners.
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
        let grants = vec![
            (ChannelId::parse("ci-infra").unwrap(), vec![Scope::Publish]),
            (
                ChannelId::parse("infra-admin").unwrap(),
                vec![Scope::Manage],
            ),
        ];

        let minted = tokens.mint(&grant(), &grants).await.unwrap();
        let claims = tokens.verify_minted(&minted.token).await.unwrap().unwrap();

        assert_eq!(claims.subject, "repo:weebo/infra:ref:refs/heads/main");
        assert_eq!(
            claims.grants, grants,
            "each channel keeps its own scopes, not the union"
        );
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
                &[(ChannelId::parse("ci-infra").unwrap(), vec![Scope::Publish])],
            )
            .await
            .unwrap();

        assert!(ours.verify_minted(&minted.token).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn an_oidc_access_token_is_not_mistaken_for_one_of_ours() {
        // First of two attempts per bearer request (§5.1): must decline quietly.
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
        // The SSRF guard: `iss` is attacker-supplied and selects a URL.
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
