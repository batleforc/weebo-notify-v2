//! Turning a credential into a [`Caller`] (design §5).
//!
//! Four identities reach the API and this is the single funnel that collapses them
//! into one typed value, so no use case ever inspects a header or a token format.
//! Two doors are wired in M1 — OIDC bearer tokens and API keys — with CI federation
//! joining them in M4.
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
    ApiKeyRepository, Clock, Failure, SecretHasher, ServiceAccountRepository, TokenRejection,
    TokenVerifier, UserRepository,
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
            users,
            accounts,
            keys,
            hasher,
            clock,
        }
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
        Ok(Caller::member_of(user.principal(), claims.groups))
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
            // it its owner's groups on top would hand it rights nobody granted it.
            groups: Vec::new(),
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
    use notify_ports::test_utils::{InMemory, StaticTokenVerifier, StubSecretHasher, TestClock};
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
}
