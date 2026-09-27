//! The [`TokenVerifier`] itself: cache, rotation, and the checks that matter.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use notify_domain::FederatedClaims;
use notify_ports::{Clock, PortError, PortResult, TokenRejection, TokenVerifier, VerifiedClaims};
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};

use crate::claims::{RawClaims, RawFederatedClaims};
use crate::config::OidcConfig;
use crate::jwks::{DEPENDENCY, DiscoveryError, HttpJwksSource, JwksSource};

/// A usable signing key, with the algorithms the *key set* says it may be used for.
#[derive(Clone)]
struct VerifyingKey {
    decoding: DecodingKey,
    /// Never derived from the token header — see the crate docs.
    algorithms: Vec<Algorithm>,
}

/// What we know about the issuer's keys right now.
#[derive(Default)]
struct Cache {
    keys: HashMap<String, VerifyingKey>,
    /// The last time a fetch was *attempted*, successful or not.
    ///
    /// Throttling on the attempt rather than on the success is the whole point: an
    /// IdP that is down must not be asked again by every request that arrives while
    /// it is down.
    last_attempt: Option<OffsetDateTime>,
}

/// Validates OIDC access tokens against the issuer's published keys.
pub struct OidcVerifier {
    config: OidcConfig,
    source: Arc<dyn JwksSource>,
    clock: Arc<dyn Clock>,
    cache: RwLock<Cache>,
    /// Held across a fetch so a burst of requests arriving on a cold cache produces
    /// one request to the IdP rather than one per caller.
    refreshing: Mutex<()>,
}

impl core::fmt::Debug for OidcVerifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("issuer", &self.config.issuer.as_str())
            .finish_non_exhaustive()
    }
}

impl OidcVerifier {
    /// Wire one against a live issuer.
    pub fn new(config: OidcConfig, clock: Arc<dyn Clock>) -> PortResult<Self> {
        let source = Arc::new(HttpJwksSource::new(&config)?);
        Ok(Self::with_source(config, source, clock))
    }

    /// Wire one against any key source — the seam the tests use.
    #[must_use]
    pub fn with_source(
        config: OidcConfig,
        source: Arc<dyn JwksSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            source,
            clock,
            cache: RwLock::new(Cache::default()),
            refreshing: Mutex::new(()),
        }
    }

    async fn key(&self, kid: &str) -> Option<VerifyingKey> {
        self.cache.read().await.keys.get(kid).cloned()
    }

    /// Fetch the key set, unless we asked too recently.
    async fn refresh_if_due(&self, now: OffsetDateTime) -> PortResult<()> {
        let _guard = self.refreshing.lock().await;

        // Re-read under the guard: whoever we queued behind may have just done it.
        if let Some(last) = self.cache.read().await.last_attempt
            && now - last < self.config.min_refresh_interval
        {
            return Ok(());
        }

        let fetched = self.source.fetch().await;
        let mut cache = self.cache.write().await;
        cache.last_attempt = Some(now);

        let set = fetched?;
        cache.keys = usable_keys(&set);
        tracing::debug!(keys = cache.keys.len(), "refreshed the issuer key set");
        Ok(())
    }

    /// The key this token names, refreshing once if the `kid` is unknown.
    ///
    /// Shared by the two verifications this type performs — a human's access token
    /// and a CI job's ID token (design §5.3). They differ in what they do with the
    /// payload and not at all in how a key is found, and every rule that matters is
    /// here: no algorithm from the header, one refresh per interval, and an unknown
    /// `kid` after that refresh is a refusal rather than a retry.
    /// Whether `nbf` is still ahead of the injected clock, past the leeway.
    fn not_yet_valid(&self, nbf: Option<i64>) -> bool {
        nbf.is_some_and(|nbf| {
            OffsetDateTime::from_unix_timestamp(nbf)
                .map_or(true, |nbf| nbf - self.config.leeway > self.clock.now())
        })
    }

    async fn key_for(&self, token: &str) -> PortResult<Result<VerifyingKey, TokenRejection>> {
        let Ok(header) = decode_header(token) else {
            return Ok(Err(TokenRejection::Malformed));
        };
        // A JWKS-backed issuer always stamps a `kid`; without one there is nothing
        // to look up, and trying every key in turn is how a slow-signature oracle
        // gets built by accident.
        let Some(kid) = header.kid else {
            return Ok(Err(TokenRejection::Malformed));
        };

        if let Some(known) = self.key(&kid).await {
            return Ok(Ok(known));
        }

        // An unknown kid is the normal shape of a key rotation, and also the normal
        // shape of a forged token. One refresh per interval serves the first without
        // letting the second reach the issuer.
        self.refresh_if_due(self.clock.now()).await?;
        Ok(self.key(&kid).await.ok_or(TokenRejection::BadSignature))
    }

    /// Verify a CI job's ID token (design §5.3).
    ///
    /// Same keys and same hardening as [`TokenVerifier::verify`]; three differences,
    /// all of them because a federated token is *reported* rather than *accepted*:
    ///
    /// * **the audience is not validated here.** It is carried out to
    ///   [`notify_domain::TrustPolicies::decide`], which refuses a token minted for
    ///   somebody else. One place decides, and it is the place with the tests
    ///   (M4.1);
    /// * **`jti` is extracted**, because the replay guard needs it and dropping it
    ///   would make the anti-replay rule unimplementable (scenario T11);
    /// * **no groups.** A forge has no notion of this instance's identity provider,
    ///   and a claim called `groups` in a CI token would be somebody else's word for
    ///   somebody else's directory.
    pub async fn verify_federated(
        &self,
        token: &str,
    ) -> PortResult<Result<FederatedClaims, TokenRejection>> {
        let key = match self.key_for(token).await? {
            Ok(key) => key,
            Err(rejection) => return Ok(Err(rejection)),
        };
        let Some(first) = key.algorithms.first().copied() else {
            return Ok(Err(TokenRejection::BadSignature));
        };

        let mut validation = Validation::new(first);
        validation.algorithms.clone_from(&key.algorithms);
        validation.set_issuer(&accepted_issuers(&self.config.issuer));
        validation.validate_aud = false;
        validation.validate_exp = false;
        validation.required_spec_claims =
            HashSet::from(["exp".to_owned(), "iss".to_owned(), "aud".to_owned()]);

        let claims = match decode::<RawFederatedClaims>(token, &key.decoding, &validation) {
            Ok(data) if self.not_yet_valid(data.claims.nbf) => {
                return Ok(Err(TokenRejection::NotYetValid));
            }
            Ok(data) => match data.claims.into_federated() {
                Ok(claims) => claims,
                Err(rejection) => return Ok(Err(rejection)),
            },
            Err(error) => return Ok(Err(rejection_for(error.kind()))),
        };

        if claims.expires_at + self.config.leeway < self.clock.now() {
            return Ok(Err(TokenRejection::Expired));
        }
        Ok(Ok(claims))
    }
}

#[async_trait::async_trait]
impl TokenVerifier for OidcVerifier {
    async fn verify(&self, token: &str) -> PortResult<Result<VerifiedClaims, TokenRejection>> {
        let key = match self.key_for(token).await? {
            Ok(key) => key,
            Err(rejection) => return Ok(Err(rejection)),
        };

        let Some(first) = key.algorithms.first().copied() else {
            return Ok(Err(TokenRejection::BadSignature));
        };
        let mut validation = Validation::new(first);
        validation.algorithms.clone_from(&key.algorithms);
        validation.set_issuer(&accepted_issuers(&self.config.issuer));
        validation.set_audience(&self.config.audiences);
        // Expiry is checked below against the injected clock rather than here
        // against the wall clock. Every other deadline in this codebase is the
        // clock port's to decide, and a token's should not be the one exception
        // nobody can write a test for. `exp` stays required, so a token without one
        // is still refused.
        validation.validate_exp = false;
        validation.required_spec_claims =
            HashSet::from(["exp".to_owned(), "iss".to_owned(), "aud".to_owned()]);

        // Another kind of JWT the same issuer signs — a logout token, a DPoP proof —
        // is not an access token, whatever its `aud` says. `JWT` and no `typ` at
        // all stay accepted: that is what most issuers stamp on access tokens.
        let typ = decode_header(token).ok().and_then(|header| header.typ);
        if let Some(typ) = typ
            && !["jwt", "at+jwt", "application/at+jwt"].contains(&typ.to_ascii_lowercase().as_str())
        {
            return Ok(Err(TokenRejection::Malformed));
        }

        let claims = match decode::<RawClaims>(token, &key.decoding, &validation) {
            Ok(data) if self.not_yet_valid(data.claims.nbf) => {
                return Ok(Err(TokenRejection::NotYetValid));
            }
            Ok(data) => match data.claims.into_verified(&self.config.groups_claim) {
                Ok(claims) => claims,
                Err(rejection) => return Ok(Err(rejection)),
            },
            Err(error) => return Ok(Err(rejection_for(error.kind()))),
        };

        if claims.expires_at + self.config.leeway < self.clock.now() {
            return Ok(Err(TokenRejection::Expired));
        }

        Ok(Ok(claims))
    }

    async fn health(&self) -> PortResult<()> {
        if !self.cache.read().await.keys.is_empty() {
            return Ok(());
        }

        self.refresh_if_due(self.clock.now()).await?;

        if self.cache.read().await.keys.is_empty() {
            // Either the throttle held us back or the issuer published a key set we
            // cannot use. Both mean this replica cannot authenticate anybody.
            return Err(PortError::unavailable(DEPENDENCY, DiscoveryError::Cold));
        }
        Ok(())
    }
}

/// Keep the keys that can verify a signature, and drop the rest.
///
/// Two exclusions carry weight. **Symmetric keys** never appear in a legitimate
/// JWKS, and accepting one invites the confusion attack where an RSA public key is
/// replayed as an HMAC secret. **Encryption keys** (`use: "enc"`) are published for
/// a different purpose entirely; verifying with one is a category error the JWK
/// itself warned us about.
fn usable_keys(set: &JwkSet) -> HashMap<String, VerifyingKey> {
    let mut keys = HashMap::new();

    for jwk in &set.keys {
        let Some(kid) = jwk.common.key_id.clone() else {
            continue;
        };
        if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
            continue;
        }
        let Some(algorithms) = signature_algorithms(jwk) else {
            continue;
        };
        let Ok(decoding) = DecodingKey::from_jwk(jwk) else {
            tracing::warn!(kid, "the issuer published a key we cannot decode");
            continue;
        };

        keys.insert(
            kid,
            VerifyingKey {
                decoding,
                algorithms,
            },
        );
    }

    keys
}

/// What this key may sign with, according to the key set.
///
/// The `alg` on the JWK wins when it is there. When it is not — which is common,
/// the field is optional — the key *type* still bounds it: an EC P-256 key cannot
/// produce an RS256 signature whatever a header claims.
fn signature_algorithms(jwk: &Jwk) -> Option<Vec<Algorithm>> {
    if let Some(declared) = jwk.common.key_algorithm {
        return signature_algorithm(declared).map(|alg| vec![alg]);
    }

    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(vec![
            Algorithm::RS256,
            Algorithm::RS384,
            Algorithm::RS512,
            Algorithm::PS256,
            Algorithm::PS384,
            Algorithm::PS512,
        ]),
        AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
            EllipticCurve::P256 => Some(vec![Algorithm::ES256]),
            EllipticCurve::P384 => Some(vec![Algorithm::ES384]),
            _ => None,
        },
        AlgorithmParameters::OctetKeyPair(_) => Some(vec![Algorithm::EdDSA]),
        // `OctetKey` is symmetric and never legitimate in a published key set; the
        // wildcard is a key type this version of the library knows and we do not.
        // Both mean "we cannot say what this may sign with", which is a refusal.
        _ => None,
    }
}

/// Map a declared JWK algorithm to a signature algorithm, refusing the symmetric
/// and key-wrapping ones.
const fn signature_algorithm(declared: KeyAlgorithm) -> Option<Algorithm> {
    match declared {
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::RS384 => Some(Algorithm::RS384),
        KeyAlgorithm::RS512 => Some(Algorithm::RS512),
        KeyAlgorithm::PS256 => Some(Algorithm::PS256),
        KeyAlgorithm::PS384 => Some(Algorithm::PS384),
        KeyAlgorithm::PS512 => Some(Algorithm::PS512),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::ES384 => Some(Algorithm::ES384),
        KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
        _ => None,
    }
}

/// The spellings of the configured issuer a token's `iss` is allowed to carry.
///
/// `iss` is compared as a string — no scheme-and-host normalisation, by design —
/// and providers disagree about the last character of it. Dex announces
/// `http://host/dex`, Authentik `https://host/application/o/weebo-notify/`, and
/// [`Url`] preserves whichever one the operator wrote in the configuration. Every
/// other issuer comparison in this crate already trims **both** sides
/// ([`crate::jwks`] on the discovery document, [`crate::federation`] on a forge's
/// `iss`); this one trimmed only ours, so a provider whose issuer genuinely ends in
/// a slash had every token it minted refused as [`TokenRejection::WrongIssuer`] —
/// after discovery had just accepted the same URL. That is how the Authentik leg of
/// the compatibility matrix failed on its first real run.
///
/// Offering both spellings is a normalisation and not a widening: the two differ by
/// a trailing separator, which cannot make the identifier name a different issuer.
fn accepted_issuers(issuer: &url::Url) -> [String; 2] {
    let trimmed = issuer.as_str().trim_end_matches('/');
    [trimmed.to_owned(), format!("{trimmed}/")]
}

/// Why the library refused, in the vocabulary the port speaks.
fn rejection_for(kind: &jsonwebtoken::errors::ErrorKind) -> TokenRejection {
    use jsonwebtoken::errors::ErrorKind as Kind;

    match kind {
        Kind::ExpiredSignature => TokenRejection::Expired,
        Kind::InvalidIssuer => TokenRejection::WrongIssuer,
        Kind::InvalidAudience => TokenRejection::WrongAudience,
        Kind::InvalidSignature
        | Kind::InvalidAlgorithm
        | Kind::InvalidAlgorithmName
        | Kind::InvalidKeyFormat
        | Kind::InvalidEcdsaKey
        | Kind::InvalidRsaKey(_) => TokenRejection::BadSignature,
        _ => TokenRejection::Malformed,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use notify_ports::test_utils::TestClock;
    use serde_json::json;
    use time::Duration;
    use url::Url;

    use super::*;
    use crate::test_support::{Signer, jwk_set};

    const ISSUER: &str = "https://idp.example";

    /// A source that hands out whatever the test put in it, and counts the asks.
    #[derive(Debug)]
    struct FakeSource {
        set: std::sync::Mutex<JwkSet>,
        fetches: AtomicUsize,
        fails: std::sync::atomic::AtomicBool,
    }

    impl FakeSource {
        fn new(set: JwkSet) -> Arc<Self> {
            Arc::new(Self {
                set: std::sync::Mutex::new(set),
                fetches: AtomicUsize::new(0),
                fails: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }

        fn rotate_to(&self, set: JwkSet) {
            *self.set.lock().unwrap() = set;
        }
    }

    #[async_trait::async_trait]
    impl JwksSource for FakeSource {
        async fn fetch(&self) -> PortResult<JwkSet> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if self.fails.load(Ordering::SeqCst) {
                return Err(PortError::unavailable(
                    DEPENDENCY,
                    std::io::Error::other("the idp is down"),
                ));
            }
            Ok(self.set.lock().unwrap().clone())
        }
    }

    struct Fixture {
        verifier: OidcVerifier,
        source: Arc<FakeSource>,
        signer: Signer,
        clock: TestClock,
        /// The issuer as *configured*, which is not always how a token spells it.
        issuer: String,
    }

    impl Fixture {
        fn new() -> Self {
            Self::for_issuer(ISSUER)
        }

        /// One configured with exactly this issuer, trailing slash and all.
        fn for_issuer(issuer: &str) -> Self {
            let signer = Signer::generate("key-1");
            let source = FakeSource::new(jwk_set(&[&signer]));
            let clock = TestClock::default();
            let config = OidcConfig::new(Url::parse(issuer).unwrap(), "weebo-notify");
            let verifier = OidcVerifier::with_source(
                config,
                Arc::clone(&source) as Arc<dyn JwksSource>,
                Arc::new(clock.clone()),
            );

            Self {
                verifier,
                source,
                signer,
                clock,
                issuer: issuer.to_owned(),
            }
        }

        fn token(&self, claims: &serde_json::Value) -> String {
            self.signer.sign(claims)
        }

        fn valid_claims(&self) -> serde_json::Value {
            json!({
                "iss": self.issuer,
                "aud": "weebo-notify",
                "sub": "alice",
                "exp": (self.clock.now() + Duration::minutes(5)).unix_timestamp(),
                "email": "alice@weebo.local",
                "name": "Alice Martin",
                "groups": ["infra-admins"],
            })
        }
    }

    #[tokio::test]
    async fn a_token_signed_by_a_published_key_authenticates() {
        let fixture = Fixture::new();
        let token = fixture.token(&fixture.valid_claims());

        let claims = fixture.verifier.verify(&token).await.unwrap().unwrap();

        assert_eq!(claims.subject, "alice");
        assert_eq!(claims.groups, ["infra-admins"]);
        assert_eq!(fixture.source.fetches(), 1, "cold cache, one fetch");
    }

    #[tokio::test]
    async fn a_warm_cache_does_not_talk_to_the_issuer() {
        let fixture = Fixture::new();
        let token = fixture.token(&fixture.valid_claims());

        for _ in 0..5 {
            fixture.verifier.verify(&token).await.unwrap().unwrap();
        }

        assert_eq!(fixture.source.fetches(), 1);
    }

    #[tokio::test]
    async fn a_rotated_key_is_picked_up_once_the_interval_has_passed() {
        let fixture = Fixture::new();
        fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap()
            .unwrap();

        let rotated = Signer::generate("key-2");
        fixture.source.rotate_to(jwk_set(&[&rotated]));
        let after_rotation = rotated.sign(&fixture.valid_claims());

        // Immediately: the throttle holds, so the new kid is simply unknown.
        let too_soon = fixture.verifier.verify(&after_rotation).await.unwrap();
        assert_eq!(too_soon.unwrap_err(), TokenRejection::BadSignature);
        assert_eq!(fixture.source.fetches(), 1, "the throttle held");

        fixture.clock.advance(Duration::minutes(2));
        let claims = fixture
            .verifier
            .verify(&after_rotation)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(claims.subject, "alice");
        assert_eq!(fixture.source.fetches(), 2);
    }

    #[tokio::test]
    async fn a_flood_of_unknown_kids_costs_the_issuer_one_request_per_interval() {
        let fixture = Fixture::new();
        let stranger = Signer::generate("forged");
        let forged = stranger.sign(&fixture.valid_claims());

        for _ in 0..20 {
            let rejected = fixture.verifier.verify(&forged).await.unwrap();
            assert_eq!(rejected.unwrap_err(), TokenRejection::BadSignature);
        }

        assert_eq!(
            fixture.source.fetches(),
            1,
            "otherwise forged tokens are a denial of service aimed at the IdP"
        );
    }

    #[tokio::test]
    async fn expiry_is_measured_against_the_injected_clock() {
        let fixture = Fixture::new();
        let token = fixture.token(&fixture.valid_claims());
        fixture.verifier.verify(&token).await.unwrap().unwrap();

        // The token said five minutes; the clock is the only thing that has to move.
        fixture.clock.advance(Duration::minutes(10));

        let rejected = fixture.verifier.verify(&token).await.unwrap();
        assert_eq!(rejected.unwrap_err(), TokenRejection::Expired);
    }

    #[tokio::test]
    async fn a_token_is_refused_before_its_nbf_past_the_leeway() {
        let fixture = Fixture::new();
        let mut claims = fixture.valid_claims();

        claims["nbf"] = json!((fixture.clock.now() + Duration::seconds(30)).unix_timestamp());
        let skewed = fixture.verifier.verify(&fixture.token(&claims)).await;
        assert!(skewed.unwrap().is_ok(), "inside the leeway");

        claims["nbf"] = json!((fixture.clock.now() + Duration::minutes(3)).unix_timestamp());
        let early = fixture.verifier.verify(&fixture.token(&claims)).await;
        assert_eq!(early.unwrap().unwrap_err(), TokenRejection::NotYetValid);
    }

    #[tokio::test]
    async fn a_little_clock_skew_is_tolerated_rather_than_turned_into_a_login_loop() {
        let fixture = Fixture::new();
        let mut claims = fixture.valid_claims();
        claims["exp"] = json!((fixture.clock.now() - Duration::seconds(30)).unix_timestamp());

        let accepted = fixture
            .verifier
            .verify(&fixture.token(&claims))
            .await
            .unwrap();

        assert!(
            accepted.is_ok(),
            "a token from a host half a minute behind ours is a fact of deployment life"
        );
    }

    #[tokio::test]
    async fn a_token_with_no_expiry_at_all_is_refused() {
        let fixture = Fixture::new();
        let mut claims = fixture.valid_claims();
        claims.as_object_mut().unwrap().remove("exp");

        let rejected = fixture
            .verifier
            .verify(&fixture.token(&claims))
            .await
            .unwrap();

        assert_eq!(rejected.unwrap_err(), TokenRejection::Malformed);
    }

    #[tokio::test]
    async fn a_token_for_another_service_is_refused_even_though_it_verifies() {
        let fixture = Fixture::new();
        let mut claims = fixture.valid_claims();
        claims["aud"] = json!("some-other-api");

        let rejected = fixture
            .verifier
            .verify(&fixture.token(&claims))
            .await
            .unwrap();

        assert_eq!(rejected.unwrap_err(), TokenRejection::WrongAudience);
    }

    #[tokio::test]
    async fn a_token_from_another_issuer_is_refused() {
        let fixture = Fixture::new();
        let mut claims = fixture.valid_claims();
        claims["iss"] = json!("https://someone-else.example");

        let rejected = fixture
            .verifier
            .verify(&fixture.token(&claims))
            .await
            .unwrap();

        assert_eq!(rejected.unwrap_err(), TokenRejection::WrongIssuer);
    }

    #[tokio::test]
    async fn a_trailing_slash_is_not_a_different_issuer() {
        // Authentik's issuer really is `.../application/o/weebo-notify/`, and its
        // tokens really do carry that slash in `iss`. Discovery accepts the URL
        // either way; verification used to accept only the trimmed spelling, so the
        // whole Authentik leg failed as WrongIssuer on tokens from the very issuer
        // it had just discovered.
        for (configured, in_the_token) in [
            (
                "https://idp.example/application/o/weebo/",
                "https://idp.example/application/o/weebo/",
            ),
            (
                "https://idp.example/application/o/weebo",
                "https://idp.example/application/o/weebo/",
            ),
            (
                "https://idp.example/application/o/weebo/",
                "https://idp.example/application/o/weebo",
            ),
        ] {
            let fixture = Fixture::for_issuer(configured);
            let mut claims = fixture.valid_claims();
            claims["iss"] = json!(in_the_token);

            let accepted = fixture
                .verifier
                .verify(&fixture.token(&claims))
                .await
                .unwrap();

            assert!(
                accepted.is_ok(),
                "configured {configured} refused a token from {in_the_token}: {:?}",
                accepted.unwrap_err()
            );
        }
    }

    #[tokio::test]
    async fn a_neighbouring_path_under_the_same_host_is_still_another_issuer() {
        // The guard on the normalisation above: a trailing separator is all it may
        // forgive. Two Authentik applications on one host differ by their path, and
        // a token minted for the other one must not verify here.
        let fixture = Fixture::for_issuer("https://idp.example/application/o/weebo/");
        let mut claims = fixture.valid_claims();
        claims["iss"] = json!("https://idp.example/application/o/weebo-staging/");

        let rejected = fixture
            .verifier
            .verify(&fixture.token(&claims))
            .await
            .unwrap();

        assert_eq!(rejected.unwrap_err(), TokenRejection::WrongIssuer);
    }

    #[tokio::test]
    async fn something_that_is_not_a_jwt_is_malformed_and_costs_nothing() {
        let fixture = Fixture::new();

        let rejected = fixture.verifier.verify("not-a-token").await.unwrap();

        assert_eq!(rejected.unwrap_err(), TokenRejection::Malformed);
        assert_eq!(fixture.source.fetches(), 0, "no lookup, no fetch");
    }

    #[tokio::test]
    async fn an_idp_outage_is_a_port_error_rather_than_a_rejection() {
        let fixture = Fixture::new();
        fixture.source.fails.store(true, Ordering::SeqCst);

        let error = fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap_err();

        assert_eq!(
            error.dependency(),
            Some("jwks"),
            "a 401 here would tell a user their credentials are wrong when they are not"
        );
    }

    #[tokio::test]
    async fn readiness_is_about_having_keys_not_about_having_asked() {
        let fixture = Fixture::new();
        fixture.source.fails.store(true, Ordering::SeqCst);
        assert!(fixture.verifier.health().await.is_err());

        fixture.source.fails.store(false, Ordering::SeqCst);
        fixture.clock.advance(Duration::minutes(2));
        assert!(fixture.verifier.health().await.is_ok());
    }

    #[test]
    fn a_symmetric_key_in_a_key_set_is_dropped() {
        // The confusion attack in one line: were this key kept, an attacker who
        // knows the public key could sign HS256 tokens with it.
        let set: JwkSet = serde_json::from_value(json!({
            "keys": [{ "kty": "oct", "kid": "shared", "k": "c2VjcmV0" }]
        }))
        .unwrap();

        assert!(usable_keys(&set).is_empty());
    }

    #[test]
    fn an_encryption_key_is_not_a_verification_key() {
        let signer = Signer::generate("enc-1");
        let mut set = jwk_set(&[&signer]);
        set.keys[0].common.public_key_use = Some(PublicKeyUse::Encryption);

        assert!(usable_keys(&set).is_empty());
    }

    #[test]
    fn a_key_without_a_kid_is_not_indexable_and_is_dropped() {
        let signer = Signer::generate("no-kid");
        let mut set = jwk_set(&[&signer]);
        set.keys[0].common.key_id = None;

        assert!(usable_keys(&set).is_empty());
    }

    #[test]
    fn a_key_that_declares_no_algorithm_is_bounded_by_its_type() {
        let signer = Signer::generate("bare");
        let mut set = jwk_set(&[&signer]);
        set.keys[0].common.key_algorithm = None;

        let keys = usable_keys(&set);

        assert_eq!(
            keys["bare"].algorithms,
            [Algorithm::ES256],
            "a P-256 key cannot produce an RS256 signature, whatever a header says"
        );
    }
}
