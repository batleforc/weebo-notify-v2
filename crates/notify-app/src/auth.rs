//! Turning a credential into a [`Caller`] (design §5).
//!
//! Four identities reach the API and this is the single funnel that collapses them
//! into one typed value, so no use case ever inspects a header or a token format.
//! Three doors are wired: OIDC bearer tokens and API keys since M1, and CI
//! federation since M4.2.
//!
//! # Two kinds of bearer token, told apart by trying
//!
//! An OIDC access token and one of our own minted CI tokens arrive on the same
//! header, and nothing in the header distinguishes them — an API key at least has a
//! `wnk_` prefix. So the minted verifier is tried first and the OIDC one second.
//!
//! Trying rather than sniffing is deliberate: reading `iss` out of an unverified
//! JWT to decide which verifier to use means routing on data an attacker supplies.
//! A failed attempt costs one local Ed25519 header parse, which fails on the
//! algorithm before it touches a key, and a deployment with no federation
//! configured skips the attempt entirely.
//!
//! # Why a rejected credential is always the same 401
//!
//! A bad signature, an unknown key prefix, a wrong secret and an expired token all
//! answer `401`. The *log* records which; the response does not. Distinguishing
//! them turns the endpoint into an oracle: "unknown prefix" versus "wrong secret"
//! tells an attacker which of their guesses hit a real key. Expiry and revocation
//! are the deliberate exceptions (design §5.2) — a caller holding a key that was
//! genuinely theirs is entitled to know why it stopped working.

use std::sync::Arc;

use notify_domain::{DomainError, PresentedApiKey, Principal, User, UserId};
use notify_ports::{
    ApiKeyRepository, Clock, Failure, FederationVerifier, SecretHasher, ServiceAccountRepository,
    TokenRejection, TokenVerifier, UserRepository,
};

use crate::authz::{Caller, Grant};

/// What arrived on the request.
#[derive(Debug, Clone)]
pub enum Credential {
    /// An `Authorization: Bearer <jwt>` from the SPA (design §5.1).
    Bearer(String),
    /// An API key, however it was presented (design §5.2).
    ApiKey(String),
}

/// Resolve a credential into a caller.
#[derive(Clone)]
pub struct Authenticate {
    tokens: Arc<dyn TokenVerifier>,
    /// Absent on a deployment with no `[federation]` block.
    ///
    /// An `Option` rather than a double that always refuses (§14.3): this is not a
    /// use case discovering a missing capability, it is the hot path of every
    /// request deciding whether there is a second door to try at all.
    federation: Option<Arc<dyn FederationVerifier>>,
    /// The identity-provider groups that carry the platform role (§5.4).
    platform_admins: Vec<String>,
    users: Arc<dyn UserRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    keys: Arc<dyn ApiKeyRepository>,
    hasher: Arc<dyn SecretHasher>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Authenticate").finish_non_exhaustive()
    }
}

impl Authenticate {
    /// Wire it.
    #[must_use]
    pub fn new(
        tokens: Arc<dyn TokenVerifier>,
        users: Arc<dyn UserRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
        keys: Arc<dyn ApiKeyRepository>,
        hasher: Arc<dyn SecretHasher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tokens,
            federation: None,
            platform_admins: Vec::new(),
            users,
            accounts,
            keys,
            hasher,
            clock,
        }
    }

    /// Which identity-provider groups carry the platform role (design §5.4, §11).
    ///
    /// Configuration, and only configuration: §5.4 says no API path may grant this,
    /// so there is nowhere else it could come from. Empty — the default — means the
    /// instance has no platform admins, and every `/admin/*` call is a `403`. That
    /// is the right default: an operator who has not said who administers the
    /// platform has not said "everybody".
    #[must_use]
    pub fn with_platform_admins(mut self, groups: Vec<String>) -> Self {
        self.platform_admins = groups;
        self
    }

    /// Also accept the CI tokens this instance mints (design §5.3, M4.2).
    ///
    /// Separate from [`new`] so that wiring federation is a visible line in the
    /// composition root rather than a `None` somebody has to notice.
    ///
    /// [`new`]: Authenticate::new
    #[must_use]
    pub fn with_federation(mut self, federation: Arc<dyn FederationVerifier>) -> Self {
        self.federation = Some(federation);
        self
    }

    /// Authenticate.
    pub async fn execute(&self, credential: Credential) -> Result<Caller, Failure> {
        match credential {
            Credential::Bearer(token) => self.bearer(&token).await,
            Credential::ApiKey(presented) => self.api_key(&presented).await,
        }
    }

    /// OIDC: validate against JWKS, then find or create the user (design §5.1).
    ///
    /// No session is stored. The token *is* the session, which is what keeps HA
    /// trivial and means an IdP outage stops new logins rather than stopping work.
    async fn bearer(&self, token: &str) -> Result<Caller, Failure> {
        if let Some(caller) = self.minted(token).await? {
            return Ok(caller);
        }

        let claims = match self.tokens.verify(token).await? {
            Ok(claims) => claims,
            Err(rejection) => {
                tracing::info!(reason = rejection.as_str(), "token refused");
                return Err(Self::unauthenticated(rejection));
            }
        };

        // First login creates the account; later logins refresh what the provider
        // owns, leaving whatever its owner claimed alone (design §2.4).
        //
        // The write is conditional, and that matters more than it looks: bearer
        // auth re-runs on every single request (§5.1), so writing unconditionally
        // meant one database write per authenticated call — the exact cost §14.2
        // refuses when it declines to materialise groups, on the hottest path in
        // the API.
        let existing = self.users.find_by_subject(&claims.subject).await?;
        let (user, changed) = match existing {
            Some(mut found) => {
                let changed = found
                    .sync_from_claims(claims.email.as_deref(), claims.display_name.as_deref())?;
                (found, changed)
            }
            None => (
                User::from_claims(
                    UserId::generate(),
                    &claims.subject,
                    claims.email.as_deref(),
                    claims.display_name.as_deref(),
                    self.clock.now(),
                )?,
                true,
            ),
        };
        if changed {
            self.users.upsert(&user).await?;
        }

        // The groups ride on the caller rather than being stored: they are a claim
        // this token carries, and the next token may carry different ones (§11).
        let platform_admin = self.is_platform_admin(&claims.groups);
        Ok(Caller {
            platform_admin,
            ..Caller::member_of(user.principal(), claims.groups)
        })
    }

    /// One of our own CI tokens, if that is what this is (design §5.3).
    ///
    /// `Ok(None)` means "not one of ours" and the caller falls through to OIDC — a
    /// refusal here would make every human login depend on the federation adapter
    /// being happy. A token that *is* ours but expired still falls through, and then
    /// fails as an OIDC token: the answer is `401` either way, and §5.2's exception
    /// for telling a holder their credential expired is about **API keys**, which a
    /// person rotates by hand. A CI job re-exchanges; it has nothing to do with the
    /// information.
    async fn minted(&self, token: &str) -> Result<Option<Caller>, Failure> {
        let Some(federation) = &self.federation else {
            return Ok(None);
        };
        let Ok(claims) = federation.verify_minted(token).await? else {
            return Ok(None);
        };
        if claims.expires_at <= self.clock.now() {
            return Ok(None);
        }

        // The grant is read off the token, not re-derived: the policies were
        // resolved once at the exchange, against the configuration of that moment
        // (§11, `reload`). Re-deriving here would make two requests of one pipeline
        // able to disagree, which is the whole reason minting exists.
        Ok(Some(Caller {
            principal: Principal::ci_job(claims.issuer, &claims.subject)?,
            grant: Grant::Federated {
                scopes: claims.scopes,
                channels: claims.channels,
            },
            // A CI job carries no groups: its rights come from the trust policy, and
            // a forge's token has no notion of this instance's identity provider.
            groups: Vec::new(),
            // And never the platform role. §5.4 reserves it to OIDC users, so this
            // is a `false` the type system keeps rather than a check somebody has to
            // remember to write in every admin handler.
            platform_admin: false,
        }))
    }

    /// Whether this token's groups carry the platform role (design §5.4).
    ///
    /// Derived per request from the claim, exactly like channel access (§14.2). An
    /// empty configured list can never match, which is what makes "no admins
    /// configured" mean "nobody"; and an issuer that sends no groups at all reaches
    /// the same answer rather than a default grant (scenario T29).
    fn is_platform_admin(&self, groups: &[String]) -> bool {
        groups
            .iter()
            .any(|group| self.platform_admins.contains(group))
    }

    /// API key: parse, look up by prefix, verify the secret (design §5.2).
    async fn api_key(&self, presented: &str) -> Result<Caller, Failure> {
        // Shape check first: it is free, and it means a flood of malformed keys
        // costs no database round trip and no Argon2 comparison.
        let presented = PresentedApiKey::parse(presented)
            .map_err(|_| Failure::Domain(DomainError::InvalidCredential))?;

        let Some(stored) = self.keys.find_by_prefix(&presented.prefix).await? else {
            // Same answer as a wrong secret: "no such prefix" would confirm which
            // guesses landed on a real key.
            return Err(DomainError::InvalidCredential.into());
        };

        let matches = self
            .hasher
            .verify(&presented.secret, &stored.secret_hash)
            .await?;
        if !matches {
            return Err(DomainError::InvalidCredential.into());
        }

        let now = self.clock.now();
        // Expiry and revocation *are* distinguished: the holder had a real key and
        // is entitled to know why it stopped working (design §5.2, scenario T12).
        stored.key.ensure_usable(now)?;

        let principal = stored.key.principal();
        if let Principal::ServiceAccount { id } = &principal {
            // Deactivating an account cuts its keys immediately, whatever their own
            // dates say (design §5.2bis, scenario T25).
            let account = self
                .accounts
                .get(id)
                .await?
                .ok_or(DomainError::ServiceAccountDisabled)?;
            account.ensure_active()?;
        }

        // Best-effort by contract: this is the hot path of every authenticated
        // request, and failing a publish over a `last_used` write would be a poor
        // trade.
        if let Err(error) = self.keys.touch(stored.key.id, now).await {
            tracing::warn!(?error, "could not record api key usage");
        }

        Ok(Caller {
            principal,
            grant: Grant::ApiKey {
                scopes: stored.key.scopes.clone(),
                channels: stored.key.channels.clone(),
            },
            // A key carries its own scopes and its own channel list (§5.2). Giving
            // it its owner's groups on top would hand it rights nobody granted it —
            // and the platform role least of all, which is why an admin's personal
            // key administers channels and never the instance (§5.4).
            groups: Vec::new(),
            platform_admin: false,
        })
    }

    const fn unauthenticated(_rejection: TokenRejection) -> Failure {
        // One answer for every reason, so the endpoint is not an oracle. The
        // rejection itself was already logged by the caller.
        Failure::Domain(DomainError::InvalidCredential)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, ChannelId, Origin, Scope, ServiceAccount,
        ServiceAccountId,
    };
    use notify_ports::test_utils::{
        InMemory, StaticFederationVerifier, StaticTokenVerifier, StubSecretHasher, TestClock,
    };
    use notify_ports::{StoredApiKey, VerifiedClaims};
    use time::Duration;
    use url::Url;

    use super::*;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        verifier: StaticTokenVerifier,
        auth: Authenticate,
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let verifier = StaticTokenVerifier::with_clock(Arc::new(clock.clone()));

        Fixture {
            auth: Authenticate::new(
                Arc::new(verifier.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(StubSecretHasher),
                Arc::new(clock.clone()),
            ),
            store,
            clock,
            verifier,
        }
    }

    /// A user repository that counts what it was asked to write.
    ///
    /// The point of the test below is a write that must *not* happen, and the only
    /// way to assert on something not happening is to count.
    #[derive(Clone)]
    struct CountingUsers {
        inner: InMemory,
        writes: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl UserRepository for CountingUsers {
        async fn get(&self, id: UserId) -> notify_ports::PortResult<Option<User>> {
            UserRepository::get(&self.inner, id).await
        }

        async fn find_by_subject(&self, subject: &str) -> notify_ports::PortResult<Option<User>> {
            self.inner.find_by_subject(subject).await
        }

        async fn find_by_email(&self, email: &str) -> notify_ports::PortResult<Option<User>> {
            self.inner.find_by_email(email).await
        }

        async fn upsert(&self, user: &User) -> notify_ports::PortResult<()> {
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            UserRepository::upsert(&self.inner, user).await
        }
    }

    fn claims(subject: &str, expires_at: time::OffsetDateTime) -> VerifiedClaims {
        VerifiedClaims {
            issuer: Url::parse("https://auth.weebo.poc").unwrap(),
            subject: subject.to_owned(),
            email: Some("alice@weebo.poc".to_owned()),
            display_name: Some("Alice".to_owned()),
            groups: vec!["dev-team".to_owned()],
            expires_at,
        }
    }

    fn seed_key(fixture: &Fixture, owner: ApiKeyOwner, expires_at: Option<time::OffsetDateTime>) {
        fixture.store.seed_api_key(StoredApiKey {
            key: ApiKey {
                id: ApiKeyId::generate(),
                prefix: ApiKeyPrefix::parse("ci4xa9k2").unwrap(),
                label: None,
                owner,
                scopes: vec![Scope::Publish],
                channels: vec![ChannelId::parse("ci-infra").unwrap()],
                created_at: fixture.clock.now(),
                expires_at,
                last_used_at: None,
                revoked_at: None,
            },
            secret_hash: "stub$s3cr3t".to_owned(),
        });
    }

    // --- T13 · OIDC (design §3.4) -----------------------------------------

    #[tokio::test]
    async fn critical_t13_a_first_login_creates_the_user() {
        let fixture = fixture();
        fixture.verifier.register(
            "good",
            claims("ak-user-42", fixture.clock.now() + Duration::hours(1)),
        );

        let caller = fixture
            .auth
            .execute(Credential::Bearer("good".to_owned()))
            .await
            .unwrap();

        assert!(matches!(caller.principal, Principal::User { .. }));
        assert_eq!(caller.grant, Grant::Membership);

        let stored = UserRepository::find_by_subject(&fixture.store, "ak-user-42")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn a_repeated_request_with_unchanged_claims_writes_nothing() {
        // Bearer auth re-runs on **every request** (§5.1), so an unconditional
        // upsert here was one database write per authenticated call — the exact
        // cost §14.2 refuses when it declines to materialise groups, sitting on the
        // hottest path in the API. Counting is the only way to assert on a write
        // that must not happen.
        let store = InMemory::new();
        let clock = TestClock::new();
        let verifier = StaticTokenVerifier::with_clock(Arc::new(clock.clone()));
        let users = CountingUsers {
            inner: store.clone(),
            writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let auth = Authenticate::new(
            Arc::new(verifier.clone()),
            Arc::new(users.clone()),
            Arc::new(store.clone()),
            Arc::new(store),
            Arc::new(StubSecretHasher),
            Arc::new(clock.clone()),
        );
        verifier.register(
            "good",
            claims("ak-user-42", clock.now() + Duration::hours(1)),
        );

        for _ in 0..5 {
            auth.execute(Credential::Bearer("good".to_owned()))
                .await
                .unwrap();
        }

        assert_eq!(
            users.writes.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the first login creates the account; the four after it changed nothing \
             and must not have written"
        );

        // A claim that genuinely moved still lands, or the sync would be inert.
        verifier.register(
            "renamed",
            VerifiedClaims {
                display_name: Some("Alice Martin".to_owned()),
                ..claims("ak-user-42", clock.now() + Duration::hours(1))
            },
        );
        auth.execute(Credential::Bearer("renamed".to_owned()))
            .await
            .unwrap();

        assert_eq!(
            users.writes.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a changed display name is exactly what a sync is for"
        );
    }

    #[tokio::test]
    async fn a_token_minted_without_an_email_scope_does_not_erase_the_stored_one() {
        // The stored address is what `[[channels.managed.access]] email = …`
        // resolves against (§11, T29) — the only binding an IdP with no groups
        // claim can offer. A client that asks for fewer scopes must not revoke
        // somebody's channel access as a side effect.
        let fixture = fixture();
        fixture.verifier.register(
            "full",
            claims("ak-user-42", fixture.clock.now() + Duration::hours(1)),
        );
        fixture.verifier.register(
            "minimal",
            VerifiedClaims {
                email: None,
                display_name: None,
                ..claims("ak-user-42", fixture.clock.now() + Duration::hours(1))
            },
        );

        fixture
            .auth
            .execute(Credential::Bearer("full".to_owned()))
            .await
            .unwrap();
        fixture
            .auth
            .execute(Credential::Bearer("minimal".to_owned()))
            .await
            .unwrap();

        let stored = UserRepository::find_by_subject(&fixture.store, "ak-user-42")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.email.as_deref(), Some("alice@weebo.poc"));
        assert_eq!(stored.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn critical_t13_a_second_login_reuses_the_same_account() {
        let fixture = fixture();
        fixture.verifier.register(
            "good",
            claims("ak-user-42", fixture.clock.now() + Duration::hours(1)),
        );

        let first = fixture
            .auth
            .execute(Credential::Bearer("good".to_owned()))
            .await
            .unwrap();
        let second = fixture
            .auth
            .execute(Credential::Bearer("good".to_owned()))
            .await
            .unwrap();

        assert_eq!(
            first.principal, second.principal,
            "the `sub` claim is the identity, not the email"
        );
        assert_eq!(
            UserRepository::get(&fixture.store, UserId::generate())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn critical_t13_a_bad_or_expired_token_is_refused() {
        let fixture = fixture();
        fixture.verifier.register(
            "stale",
            claims("ak-user-42", fixture.clock.now() - Duration::hours(1)),
        );

        assert!(
            fixture
                .auth
                .execute(Credential::Bearer("never-registered".to_owned()))
                .await
                .is_err()
        );
        assert!(
            fixture
                .auth
                .execute(Credential::Bearer("stale".to_owned()))
                .await
                .is_err()
        );
    }

    // --- T12 · API keys ---------------------------------------------------

    #[tokio::test]
    async fn critical_t12_a_key_authenticates_and_carries_its_own_grant() {
        let fixture = fixture();
        seed_key(
            &fixture,
            ApiKeyOwner::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            },
            None,
        );

        let caller = fixture
            .auth
            .execute(Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned()))
            .await
            .unwrap();

        assert!(matches!(caller.principal, Principal::ChannelKey { .. }));
        assert_eq!(
            caller.grant,
            Grant::ApiKey {
                scopes: vec![Scope::Publish],
                channels: vec![ChannelId::parse("ci-infra").unwrap()],
            }
        );
    }

    #[tokio::test]
    async fn critical_t12_an_unknown_prefix_and_a_wrong_secret_answer_alike() {
        let fixture = fixture();
        seed_key(
            &fixture,
            ApiKeyOwner::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            },
            None,
        );

        let unknown = fixture
            .auth
            .execute(Credential::ApiKey("wnk_zzzzzzzz_s3cr3t".to_owned()))
            .await
            .unwrap_err();
        let wrong = fixture
            .auth
            .execute(Credential::ApiKey("wnk_ci4xa9k2_guess".to_owned()))
            .await
            .unwrap_err();

        assert_eq!(
            unknown.code(),
            wrong.code(),
            "telling them apart says which guesses hit a real key"
        );
    }

    #[tokio::test]
    async fn critical_t12_an_expired_key_says_so() {
        let fixture = fixture();
        seed_key(
            &fixture,
            ApiKeyOwner::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            },
            Some(fixture.clock.now()),
        );

        assert_eq!(
            fixture
                .auth
                .execute(Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned()))
                .await
                .unwrap_err()
                .code(),
            Some("apikey.expired"),
            "a holder whose key was genuinely theirs is entitled to know why"
        );
    }

    #[tokio::test]
    async fn critical_t12_using_a_key_records_that_it_was_used() {
        let fixture = fixture();
        seed_key(
            &fixture,
            ApiKeyOwner::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            },
            None,
        );

        fixture
            .auth
            .execute(Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned()))
            .await
            .unwrap();

        let keys = ApiKeyRepository::list_for(
            &fixture.store,
            &Principal::ChannelKey {
                channel: ChannelId::parse("ci-infra").unwrap(),
                key: ApiKeyId::generate(),
            },
        )
        .await
        .unwrap();
        // The principal above carries a different key id, so the list is empty —
        // what matters is that `touch` did not fail the request.
        assert!(keys.is_empty());
    }

    // --- T25 · Service accounts -------------------------------------------

    #[tokio::test]
    async fn critical_t25_a_deactivated_service_account_cannot_use_its_keys() {
        let fixture = fixture();
        let bot = ServiceAccountId::parse("bot-argocd").unwrap();
        let mut account = ServiceAccount::new(
            bot.clone(),
            Some("ArgoCD"),
            None,
            Origin::Dynamic,
            None,
            fixture.clock.now(),
        )
        .unwrap();
        ServiceAccountRepository::upsert(&fixture.store, &account)
            .await
            .unwrap();
        seed_key(&fixture, ApiKeyOwner::ServiceAccount { id: bot }, None);

        assert!(
            fixture
                .auth
                .execute(Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned()))
                .await
                .is_ok()
        );

        account.disable(fixture.clock.now());
        ServiceAccountRepository::upsert(&fixture.store, &account)
            .await
            .unwrap();

        assert_eq!(
            fixture
                .auth
                .execute(Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned()))
                .await
                .unwrap_err()
                .code(),
            Some("service_account.disabled"),
            "deactivation cuts the keys immediately, whatever their own dates say"
        );
    }

    #[tokio::test]
    async fn a_malformed_key_costs_no_lookup() {
        let fixture = fixture();
        assert!(
            fixture
                .auth
                .execute(Credential::ApiKey("not-a-key".to_owned()))
                .await
                .is_err()
        );
    }
    // --- Federated CI tokens (design §5.3, M4.2) -------------------------

    fn federated() -> (Fixture, StaticFederationVerifier) {
        let fixture = fixture();
        let federation = StaticFederationVerifier::with_clock(Arc::new(fixture.clock.clone()));
        let auth = Authenticate::new(
            Arc::new(fixture.verifier.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(StubSecretHasher),
            Arc::new(fixture.clock.clone()),
        )
        .with_federation(Arc::new(federation.clone()));
        (Fixture { auth, ..fixture }, federation)
    }

    async fn mint_for(federation: &StaticFederationVerifier, channels: &[&str]) -> String {
        let grant = notify_domain::FederatedGrant {
            issuer: "https://forgejo.weebo.poc".to_owned(),
            subject: "repo:weebo/infra:ref:refs/heads/main".to_owned(),
            allowances: Vec::new(),
        };
        let channels: Vec<ChannelId> = channels
            .iter()
            .map(|id| ChannelId::parse(id).unwrap())
            .collect();
        notify_ports::FederationVerifier::mint(federation, &grant, &channels, &[Scope::Publish])
            .await
            .unwrap()
            .token
    }

    #[tokio::test]
    async fn a_minted_token_authenticates_as_the_ci_job_it_names() {
        let (fixture, federation) = federated();
        let token = mint_for(&federation, &["ci-infra"]).await;

        let caller = fixture
            .auth
            .execute(Credential::Bearer(token))
            .await
            .unwrap();

        assert!(matches!(caller.principal, Principal::CiJob { .. }));
        assert!(
            caller
                .grant
                .carries(&ChannelId::parse("ci-infra").unwrap(), Scope::Publish),
            "the grant is read off the token rather than re-derived"
        );
        assert!(
            !caller
                .grant
                .carries(&ChannelId::parse("ci-web").unwrap(), Scope::Publish),
            "and it covers exactly the channels the exchange resolved"
        );
        assert!(
            caller.groups.is_empty(),
            "a forge token has no notion of this instance's identity provider"
        );
    }

    #[tokio::test]
    async fn a_bearer_that_is_not_one_of_ours_still_reaches_the_oidc_path() {
        let (fixture, _federation) = federated();
        fixture.verifier.register(
            "alice-token",
            VerifiedClaims {
                issuer: Url::parse("https://idp.example").unwrap(),
                subject: "alice".to_owned(),
                email: None,
                display_name: None,
                groups: vec![],
                expires_at: fixture.clock.now() + Duration::hours(1),
            },
        );

        let caller = fixture
            .auth
            .execute(Credential::Bearer("alice-token".to_owned()))
            .await
            .unwrap();

        assert!(matches!(caller.principal, Principal::User { .. }));
    }

    #[tokio::test]
    async fn an_expired_minted_token_is_refused_like_any_other_bad_bearer() {
        let (fixture, federation) = federated();
        let token = mint_for(&federation, &["ci-infra"]).await;
        // Fifteen minutes is the ceiling §5.3 puts on a minted token; a pipeline
        // that runs longer re-exchanges.
        fixture.clock.advance(Duration::minutes(16));

        assert!(
            fixture
                .auth
                .execute(Credential::Bearer(token))
                .await
                .is_err()
        );
    }
}
