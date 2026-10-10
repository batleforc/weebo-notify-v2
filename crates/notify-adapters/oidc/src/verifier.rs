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
use tokio::sync::{Mutex, MutexGuard, RwLock};

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
    /// The last time a fetch was attempted: throttling on attempts keeps a down IdP from being hammered.
    last_attempt: Option<OffsetDateTime>,
    /// When `keys` was last replaced by a successful fetch: what
    /// [`OidcConfig::key_max_age`] and [`OidcConfig::stale_key_grace`] count from.
    last_success: Option<OffsetDateTime>,
}

/// Validates OIDC access tokens against the issuer's published keys.
pub struct OidcVerifier {
    config: OidcConfig,
    source: Arc<dyn JwksSource>,
    clock: Arc<dyn Clock>,
    cache: RwLock<Cache>,
    /// Held across a fetch so a cold-cache burst makes one IdP request.
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
        let guard = self.refreshing.lock().await;
        self.refresh_holding(now, guard).await
    }

    /// [`Self::refresh_if_due`], for a caller that already holds the guard.
    async fn refresh_holding(
        &self,
        now: OffsetDateTime,
        _guard: MutexGuard<'_, ()>,
    ) -> PortResult<()> {
        // Re-read under the guard: whoever we queued behind may have just done it.
        if let Some(last) = self.cache.read().await.last_attempt
            && now - last < self.config.min_refresh_interval
        {
            return Ok(());
        }

        let fetched = self.source.fetch().await;
        let mut cache = self.cache.write().await;
        cache.last_attempt = Some(now);

        let set = match fetched {
            Ok(set) => set,
            Err(error) => {
                // Past the grace a withdrawn key must stop verifying, outage or
                // not: refusing logins is the lesser failure next to accepting a
                // key the issuer took back.
                if cache.last_success.is_some_and(|at| {
                    now - at >= self.config.key_max_age + self.config.stale_key_grace
                }) && !cache.keys.is_empty()
                {
                    tracing::error!(%error, "the issuer key set is past its grace; dropping it");
                    cache.keys.clear();
                }
                return Err(error);
            }
        };
        cache.keys = usable_keys(&set);
        cache.last_success = Some(now);
        tracing::debug!(keys = cache.keys.len(), "refreshed the issuer key set");
        Ok(())
    }

    /// How long ago the cached set was fetched, if it ever was.
    async fn keys_age(&self, now: OffsetDateTime) -> Option<time::Duration> {
        self.cache.read().await.last_success.map(|at| now - at)
    }

    /// Whether `nbf` is still ahead of the injected clock, past the leeway.
    fn not_yet_valid(&self, nbf: Option<i64>) -> bool {
        nbf.is_some_and(|nbf| {
            OffsetDateTime::from_unix_timestamp(nbf)
                .map_or(true, |nbf| nbf - self.config.leeway > self.clock.now())
        })
    }

    /// The key this token names (access and federated tokens alike), refreshing once if
    /// the `kid` is unknown; no algorithm from the header.
    async fn key_for(&self, token: &str) -> PortResult<Result<VerifyingKey, TokenRejection>> {
        let Ok(header) = decode_header(token) else {
            return Ok(Err(TokenRejection::Malformed));
        };
        // No `kid`, no lookup: trying every key would build a slow-signature oracle.
        let Some(kid) = header.kid else {
            return Ok(Err(TokenRejection::Malformed));
        };

        let now = self.clock.now();
        if let Some(age) = self.keys_age(now).await
            && age >= self.config.key_max_age
        {
            // Stale-while-revalidate. Whoever takes the guard refreshes; everyone
            // else carries on with the set they have instead of queueing behind a
            // fetch that can take the whole `http_timeout` — which, on a slow IdP,
            // stalled every authenticated request once an hour.
            //
            // Not past the grace, though: a set that old is only still here
            // because nobody has tried to replace it yet, and serving it while
            // the attempt is in flight would stretch the withdrawn-key window.
            let refreshed = match self.refreshing.try_lock() {
                Ok(guard) => Some(self.refresh_holding(now, guard).await),
                Err(_) if age < self.config.key_max_age + self.config.stale_key_grace => None,
                Err(_) => Some(self.refresh_if_due(now).await),
            };
            if let Some(Err(error)) = refreshed {
                // Still throttled by `min_refresh_interval`, so an outage costs the
                // issuer one request per interval. The stale set keeps serving until
                // the refresh drops it at the end of its grace.
                tracing::warn!(%error, "could not refresh an expired issuer key set");
            }
        }

        if let Some(known) = self.key(&kid).await {
            return Ok(Ok(known));
        }

        // Unknown `kid`: rotation or forgery; one refresh per interval serves the first only.
        self.refresh_if_due(now).await?;
        Ok(self.key(&kid).await.ok_or(TokenRejection::BadSignature))
    }

    /// Verify a CI job's ID token (design §5.3): same keys and hardening as [`TokenVerifier::verify`],
    /// but `aud` is left to [`notify_domain::TrustPolicies::decide`], `jti` is kept (T11), and no groups.
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

        let mut claims = match decode::<RawFederatedClaims>(token, &key.decoding, &validation) {
            Ok(data) if self.not_yet_valid(data.claims.nbf) => {
                return Ok(Err(TokenRejection::NotYetValid));
            }
            Ok(data) => match data.claims.into_federated() {
                Ok(claims) => claims,
                Err(rejection) => return Ok(Err(rejection)),
            },
            Err(error) => return Ok(Err(rejection_for(error.kind()))),
        };

        // Reported with the leeway included, because that is how long this check
        // keeps passing — and so how long the replay guard must remember the `jti`.
        // Remembering it only until `exp` let the same token be exchanged again for
        // the length of the leeway once the purge had swept the id.
        claims.accepted_until += self.config.leeway;
        if claims.accepted_until < self.clock.now() {
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
        // `exp` stays required but is checked against the injected clock below.
        validation.validate_exp = false;
        validation.required_spec_claims =
            HashSet::from(["exp".to_owned(), "iss".to_owned(), "aud".to_owned()]);

        // Refuse other JWT kinds (logout tokens, DPoP); `JWT` or no `typ` is accepted.
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
            Ok(data) => match data.claims.into_verified(
                &self.config.groups_claim,
                self.config.trust_unverified_email,
            ) {
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
            // Throttled or unusable key set: this replica cannot authenticate anybody.
            return Err(PortError::unavailable(DEPENDENCY, DiscoveryError::Cold));
        }
        Ok(())
    }
}

/// Keep only signature keys: symmetric keys (algorithm confusion) and `use: "enc"` keys are dropped.
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

/// What this key may sign with: the JWK `alg` if present, otherwise bounded by the key type.
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
        // Symmetric, or a type we do not know: refuse.
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

/// The accepted spellings of the configured issuer: with and without a trailing slash.
/// Providers disagree (Dex vs Authentik); a trailing separator cannot name a different issuer.
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
    async fn a_withdrawn_key_stops_verifying_once_the_set_outlives_its_max_age() {
        let fixture = Fixture::new();
        fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap()
            .unwrap();

        // The compromise response: the issuer publishes a set without key-1.
        fixture
            .source
            .rotate_to(jwk_set(&[&Signer::generate("key-2")]));
        fixture.clock.advance(Duration::minutes(61));

        let refused = fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap();
        assert_eq!(refused.unwrap_err(), TokenRejection::BadSignature);
        assert_eq!(
            fixture.source.fetches(),
            2,
            "expiry alone triggered a fetch"
        );
    }

    #[tokio::test]
    async fn an_expired_set_serves_through_an_outage_for_its_grace_and_no_longer() {
        let fixture = Fixture::new();
        fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap()
            .unwrap();
        fixture.source.fails.store(true, Ordering::SeqCst);

        fixture.clock.advance(Duration::minutes(61));
        assert!(
            fixture
                .verifier
                .verify(&fixture.token(&fixture.valid_claims()))
                .await
                .unwrap()
                .is_ok(),
            "an IdP blip must not log everybody out"
        );

        fixture.clock.advance(Duration::hours(7));
        assert!(
            !matches!(
                fixture
                    .verifier
                    .verify(&fixture.token(&fixture.valid_claims()))
                    .await,
                Ok(Ok(_))
            ),
            "past the grace the stale set is dropped"
        );
    }

    #[tokio::test]
    async fn an_expired_set_keeps_serving_while_somebody_else_refreshes_it() {
        let fixture = Fixture::new();
        fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap()
            .unwrap();
        fixture.clock.advance(Duration::minutes(61));

        // A refresh in flight on a slow IdP: the guard is taken and not released.
        let in_flight = fixture.verifier.refreshing.lock().await;
        let token = fixture.token(&fixture.valid_claims());
        let served = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.verifier.verify(&token),
        )
        .await;
        assert!(
            matches!(served, Ok(Ok(Ok(_)))),
            "a request queued behind somebody else's fetch instead of using the stale set"
        );

        // Past the grace the stale set is not served while the fetch decides its
        // fate: the request waits for the outcome.
        fixture.clock.advance(Duration::hours(2));
        let token = fixture.token(&fixture.valid_claims());
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fixture.verifier.verify(&token),
        )
        .await;
        assert!(
            waited.is_err(),
            "a set past its grace served without a refresh"
        );
        drop(in_flight);
    }

    #[tokio::test]
    async fn a_zero_grace_drops_an_expired_set_on_the_first_failed_refresh() {
        let mut fixture = Fixture::new();
        fixture.verifier.config.stale_key_grace = Duration::ZERO;
        fixture
            .verifier
            .verify(&fixture.token(&fixture.valid_claims()))
            .await
            .unwrap()
            .unwrap();
        fixture.source.fails.store(true, Ordering::SeqCst);

        fixture.clock.advance(Duration::minutes(61));
        let token = fixture.token(&fixture.valid_claims());
        assert!(!matches!(fixture.verifier.verify(&token).await, Ok(Ok(_))));
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
    async fn a_federated_token_is_remembered_for_as_long_as_the_leeway_accepts_it() {
        let fixture = Fixture::new();
        let mut claims = fixture.valid_claims();
        let exp = fixture.clock.now() + Duration::minutes(5);
        claims["exp"] = json!(exp.unix_timestamp());
        claims["jti"] = json!("job-1");
        let token = fixture.token(&claims);

        let verified = fixture
            .verifier
            .verify_federated(&token)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            verified.accepted_until,
            exp + fixture.verifier.config.leeway,
            "the replay guard forgets the jti at this instant, so it must be the last \
             one at which the token still verifies"
        );

        // Past `exp`, inside the leeway: still accepted, which is exactly why the
        // id must still be remembered.
        fixture
            .clock
            .advance(Duration::minutes(5) + Duration::seconds(30));
        assert!(
            fixture
                .verifier
                .verify_federated(&token)
                .await
                .unwrap()
                .is_ok()
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
        // Authentik's issuer and `iss` really end in a slash.
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
        // Only a trailing separator is forgiven, not a different path.
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
        // Kept, this key would let anyone sign HS256 tokens with the public key.
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
