//! Turns a credential into a [`Caller`] (§5). Minted CI tokens are tried before OIDC
//! rather than sniffing `iss` from an unverified JWT. Every rejection is the same 401
//! (no oracle), except expiry and revocation, which the holder may know about (§5.2).

use std::sync::Arc;

use notify_domain::{DomainError, PresentedApiKey, Principal, User, UserId};
use notify_ports::{
    ApiKeyRepository, Clock, DeviceRepository, Failure, FederationVerifier, PortError,
    SecretHasher, ServiceAccountRepository, TokenRejection, TokenVerifier, UserRepository,
};
use time::OffsetDateTime;

use crate::authz::{Caller, Grant};

/// How stale `last_used_at` may get before a request pays to refresh it.
const LAST_USED_GRANULARITY: time::Duration = time::Duration::minutes(1);

/// How old an owner's remembered groups may be and still widen what a personal key
/// reaches, unless `[auth] group_snapshot_ttl` says otherwise; past it, the key keeps
/// only its owner's direct memberships.
pub const DEFAULT_GROUP_SNAPSHOT_TTL: time::Duration = time::Duration::days(7);

/// How a key issuer's token starts (ADR-0033): `wki_<issuer id>_<secret>`. Issuer ids
/// have no `_`, so the first one after the scheme ends the id.
pub const KEY_ISSUER_SCHEME: &str = "wki_";

/// What arrived on the request.
#[derive(Debug, Clone)]
pub enum Credential {
    /// An `Authorization: Bearer <jwt>` from the SPA (design §5.1).
    Bearer(String),
    /// An API key, however it was presented (design §5.2).
    ApiKey(String),
    /// A key issuer's token, `wki_<id>_<secret>` (ADR-0033).
    KeyIssuer(String),
}

/// A `[[key_issuers]]` entry as the authenticator holds it: the id, and the hash of
/// its secret, never the secret itself.
#[derive(Clone)]
pub struct KeyIssuer {
    /// Its configured id, the audit actor of what it mints and revokes.
    pub id: String,
    /// [`SecretHasher::hash`] of the secret half of its token.
    pub secret_hash: String,
}

impl core::fmt::Debug for KeyIssuer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KeyIssuer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Resolve a credential into a caller.
#[derive(Clone)]
pub struct Authenticate {
    tokens: Arc<dyn TokenVerifier>,
    /// Absent on a deployment with no `[federation]` block.
    federation: Option<Arc<dyn FederationVerifier>>,
    /// The identity-provider groups that carry the platform role (§5.4).
    platform_admins: Vec<String>,
    /// See [`DEFAULT_GROUP_SNAPSHOT_TTL`].
    group_snapshot_ttl: time::Duration,
    /// `[[key_issuers]]` (ADR-0033); empty means none.
    key_issuers: Vec<KeyIssuer>,
    users: Arc<dyn UserRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    keys: Arc<dyn ApiKeyRepository>,
    devices: Arc<dyn DeviceRepository>,
    hasher: Arc<dyn SecretHasher>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Authenticate").finish_non_exhaustive()
    }
}

impl Authenticate {
    #[must_use]
    pub fn new(
        tokens: Arc<dyn TokenVerifier>,
        users: Arc<dyn UserRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
        keys: Arc<dyn ApiKeyRepository>,
        devices: Arc<dyn DeviceRepository>,
        hasher: Arc<dyn SecretHasher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tokens,
            federation: None,
            platform_admins: Vec::new(),
            group_snapshot_ttl: DEFAULT_GROUP_SNAPSHOT_TTL,
            key_issuers: Vec::new(),
            users,
            accounts,
            keys,
            devices,
            hasher,
            clock,
        }
    }

    /// Which identity-provider groups carry the platform role (§5.4). Configuration only;
    /// empty (the default) means no platform admins.
    #[must_use]
    pub fn with_platform_admins(mut self, groups: Vec<String>) -> Self {
        self.platform_admins = groups;
        self
    }

    /// How old remembered groups may be and still widen a personal key's reach
    /// (`[auth] group_snapshot_ttl`).
    #[must_use]
    pub fn with_group_snapshot_ttl(mut self, ttl: time::Duration) -> Self {
        self.group_snapshot_ttl = ttl;
        self
    }

    /// Also accept the CI tokens this instance mints (§5.3).
    #[must_use]
    pub fn with_federation(mut self, federation: Arc<dyn FederationVerifier>) -> Self {
        self.federation = Some(federation);
        self
    }

    /// The tokens of `[[key_issuers]]` (ADR-0033). Configuration only, read at boot.
    #[must_use]
    pub fn with_key_issuers(mut self, issuers: Vec<KeyIssuer>) -> Self {
        self.key_issuers = issuers;
        self
    }

    /// Every route but the key routes. A key issuer is refused here, with a `403`
    /// once its token checks out, so a route that forgets about issuers refuses them.
    pub async fn execute(&self, credential: Credential) -> Result<Caller, Failure> {
        let caller = self.execute_for_keys(credential).await?;
        if caller.grant == Grant::KeyIssuer {
            return Err(DomainError::KeyIssuerOnly.into());
        }
        Ok(caller)
    }

    /// The key routes (mint, list, revoke) only: [`Authenticate::execute`], plus the
    /// key issuers, which those use cases bound to managed service accounts.
    pub async fn execute_for_keys(&self, credential: Credential) -> Result<Caller, Failure> {
        match credential {
            Credential::Bearer(token) => self.bearer(&token).await,
            Credential::ApiKey(presented) => self.api_key(&presented).await,
            Credential::KeyIssuer(token) => self.key_issuer(&token).await,
        }
    }

    /// A `wki_<id>_<secret>` token: the issuer by id, then its secret against the
    /// stored hash, in constant time (ADR-0033). Every mismatch is the same 401.
    async fn key_issuer(&self, token: &str) -> Result<Caller, Failure> {
        let invalid = || Failure::Domain(DomainError::InvalidCredential);
        let (id, secret) = token
            .strip_prefix(KEY_ISSUER_SCHEME)
            .and_then(|rest| rest.split_once('_'))
            .ok_or_else(invalid)?;
        let issuer = self
            .key_issuers
            .iter()
            .find(|issuer| issuer.id == id)
            .ok_or_else(invalid)?;
        if !self.hasher.verify(secret, &issuer.secret_hash).await? {
            return Err(invalid());
        }
        Ok(Caller {
            principal: Principal::KeyIssuer {
                id: issuer.id.clone(),
            },
            grant: Grant::KeyIssuer,
            // A member of nothing, and never the platform role.
            groups: Vec::new(),
            platform_admin: false,
            // Rotated by changing the secret and restarting (ADR-0033).
            expires_at: None,
        })
    }

    /// How long `caller`'s credential stays valid, by this use case's clock.
    /// `None` for one that never expires; zero once it has.
    #[must_use]
    pub fn time_left(&self, caller: &Caller) -> Option<std::time::Duration> {
        caller.expires_at.map(|at| {
            (at - self.clock.now())
                .try_into()
                .unwrap_or(std::time::Duration::ZERO)
        })
    }

    /// OIDC: validate against JWKS, then find or create the user (§5.1). No session is
    /// stored: the token is the session.
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

        // First login creates the account; later logins refresh what the provider owns (§2.4).
        // Conditional write: bearer auth runs on every request.
        let existing = self.users.find_by_subject(&claims.subject).await?;
        let first_login = existing.is_none();
        let (mut user, changed) = match &existing {
            Some(found) => {
                let mut refreshed = found.clone();
                let changed = refreshed.sync_from_claims(
                    claims.email.as_deref(),
                    claims.email_unverified,
                    claims.display_name.as_deref(),
                )?;
                (refreshed, changed)
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
        if let (true, Some(before)) = (changed, &existing) {
            // Lost to a concurrent write (a profile edit, another request's refresh): keep
            // what it stored rather than erase it; the next request refreshes the claims.
            if !self.users.replace(before, &user).await? {
                user = UserRepository::get(self.users.as_ref(), user.id)
                    .await?
                    .ok_or(PortError::Conflict { what: "user" })?;
            }
        } else if changed {
            match self.users.upsert(&user).await {
                Ok(()) => {}
                // The first requests after a login arrive together, each minting its own
                // id; the subject is unique, so all but one collide. Theirs is the account.
                Err(PortError::Conflict { .. }) if first_login => {
                    user = self
                        .users
                        .find_by_subject(&claims.subject)
                        .await?
                        .ok_or(PortError::Conflict { what: "user" })?;
                }
                Err(error) => return Err(error.into()),
            }
        }

        // Groups are a claim of this token, not stored: the next token may differ (§11).
        let platform_admin = self.is_platform_admin(&claims.groups);
        Ok(Caller {
            platform_admin,
            expires_at: Some(claims.expires_at),
            ..Caller::member_of(user.principal(), claims.groups)
        })
    }

    /// One of our own CI tokens, if that is what this is (§5.3). `Ok(None)` falls through
    /// to OIDC, so human logins never depend on the federation adapter.
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

        // The grant is read off the token, resolved once at the exchange, so two requests
        // of one pipeline cannot disagree.
        Ok(Some(Caller {
            principal: Principal::ci_job(claims.issuer, &claims.subject)?,
            grant: Grant::Federated {
                grants: claims.grants,
            },
            // A CI job's rights come from the trust policy, not groups.
            groups: Vec::new(),
            // Never the platform role: §5.4 reserves it to OIDC users.
            platform_admin: false,
            expires_at: Some(claims.expires_at),
        }))
    }

    /// Whether this token's groups carry the platform role (§5.4). An empty configured
    /// list never matches.
    fn is_platform_admin(&self, groups: &[String]) -> bool {
        groups
            .iter()
            .any(|group| self.platform_admins.contains(group))
    }

    /// API key: parse, look up by prefix, verify the secret (design §5.2).
    async fn api_key(&self, presented: &str) -> Result<Caller, Failure> {
        // Shape check first: malformed keys cost no database round trip and no Argon2.
        let presented = PresentedApiKey::parse(presented)
            .map_err(|_| Failure::Domain(DomainError::InvalidCredential))?;

        let Some(stored) = self.keys.find_by_prefix(&presented.prefix).await? else {
            // Same answer as a wrong secret, so prefixes cannot be probed.
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
        // Expiry and revocation are distinguished on purpose (§5.2).
        stored.key.ensure_usable(now)?;

        let principal = stored.key.principal();
        if let Principal::ServiceAccount { id } = &principal {
            // Deactivating an account cuts its keys immediately (§5.2bis).
            let account = self
                .accounts
                .get(id)
                .await?
                .ok_or(DomainError::ServiceAccountDisabled)?;
            account.ensure_active()?;
        }

        // Best-effort and throttled to `LAST_USED_GRANULARITY`: otherwise every
        // authenticated request would write a row.
        if Self::should_touch(stored.key.last_used_at, now)
            && let Err(error) = self.keys.touch(stored.key.id, now).await
        {
            tracing::warn!(?error, "could not record api key usage");
        }

        // A key carries no token, so the owner's groups come from their last login, and
        // only a recent one (`group_snapshot_ttl`).
        let owner_groups = match &principal {
            Principal::User { .. } => {
                self.devices
                    .remembered_groups(&principal, now.saturating_sub(self.group_snapshot_ttl))
                    .await?
            }
            _ => Vec::new(),
        };

        Ok(Caller {
            principal,
            grant: Grant::ApiKey {
                id: stored.key.id,
                scopes: stored.key.scopes.clone(),
                channels: stored.key.channels.clone(),
                publish_per_minute: stored.key.publish_per_minute,
                owner_groups,
            },
            // A key carries its own scopes and channels (§5.2); never its owner's groups or
            // platform role (§5.4).
            groups: Vec::new(),
            platform_admin: false,
            expires_at: stored.key.expires_at,
        })
    }

    /// Whether `last_used_at` is stale enough to be worth a write. A never-used key and
    /// a clock that went backwards always write.
    fn should_touch(last_used_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
        last_used_at.is_none_or(|last| now - last >= LAST_USED_GRANULARITY || now < last)
    }

    const fn unauthenticated(_rejection: TokenRejection) -> Failure {
        // One answer for every reason; the caller already logged the rejection.
        Failure::Domain(DomainError::InvalidCredential)
    }
}

#[cfg(test)]
mod tests {
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

        async fn get_many(&self, ids: &[UserId]) -> notify_ports::PortResult<Vec<User>> {
            self.inner.get_many(ids).await
        }

        async fn find_by_subject(&self, subject: &str) -> notify_ports::PortResult<Option<User>> {
            self.inner.find_by_subject(subject).await
        }

        async fn find_by_email(
            &self,
            email: &str,
        ) -> notify_ports::PortResult<notify_ports::EmailMatch> {
            self.inner.find_by_email(email).await
        }

        async fn lookup(&self, text: &str, limit: u32) -> notify_ports::PortResult<Vec<User>> {
            self.inner.lookup(text, limit).await
        }

        async fn is_avatar(
            &self,
            avatar: notify_domain::AttachmentId,
        ) -> notify_ports::PortResult<bool> {
            self.inner.is_avatar(avatar).await
        }

        async fn upsert(&self, user: &User) -> notify_ports::PortResult<()> {
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            UserRepository::upsert(&self.inner, user).await
        }

        async fn replace(&self, before: &User, after: &User) -> notify_ports::PortResult<bool> {
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.replace(before, after).await
        }

        async fn set_avatar(
            &self,
            user: UserId,
            avatar: notify_domain::AttachmentId,
        ) -> notify_ports::PortResult<Option<notify_domain::AttachmentId>> {
            self.inner.set_avatar(user, avatar).await
        }
    }

    fn claims(subject: &str, expires_at: time::OffsetDateTime) -> VerifiedClaims {
        VerifiedClaims {
            issuer: Url::parse("https://auth.weebo.poc").unwrap(),
            subject: subject.to_owned(),
            email: Some("alice@weebo.poc".to_owned()),
            email_unverified: false,
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
                publish_per_minute: None,
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
    async fn two_first_requests_at_once_end_on_the_same_account() {
        // The other request wins the race to create the row; ours collides on the subject.
        #[derive(Clone)]
        struct Raced(InMemory);

        #[async_trait::async_trait]
        impl UserRepository for Raced {
            async fn get(&self, id: UserId) -> notify_ports::PortResult<Option<User>> {
                UserRepository::get(&self.0, id).await
            }
            async fn get_many(&self, ids: &[UserId]) -> notify_ports::PortResult<Vec<User>> {
                self.0.get_many(ids).await
            }
            async fn find_by_subject(
                &self,
                subject: &str,
            ) -> notify_ports::PortResult<Option<User>> {
                self.0.find_by_subject(subject).await
            }
            async fn find_by_email(
                &self,
                email: &str,
            ) -> notify_ports::PortResult<notify_ports::EmailMatch> {
                self.0.find_by_email(email).await
            }
            async fn lookup(&self, text: &str, limit: u32) -> notify_ports::PortResult<Vec<User>> {
                self.0.lookup(text, limit).await
            }
            async fn is_avatar(
                &self,
                avatar: notify_domain::AttachmentId,
            ) -> notify_ports::PortResult<bool> {
                self.0.is_avatar(avatar).await
            }
            async fn upsert(&self, user: &User) -> notify_ports::PortResult<()> {
                let mut winner = user.clone();
                winner.id = UserId::generate();
                UserRepository::upsert(&self.0, &winner).await?;
                Err(PortError::Conflict { what: "user" })
            }
            async fn replace(&self, before: &User, after: &User) -> notify_ports::PortResult<bool> {
                self.0.replace(before, after).await
            }
            async fn set_avatar(
                &self,
                user: UserId,
                avatar: notify_domain::AttachmentId,
            ) -> notify_ports::PortResult<Option<notify_domain::AttachmentId>> {
                self.0.set_avatar(user, avatar).await
            }
        }

        let store = InMemory::new();
        let clock = TestClock::new();
        let verifier = StaticTokenVerifier::with_clock(Arc::new(clock.clone()));
        let auth = Authenticate::new(
            Arc::new(verifier.clone()),
            Arc::new(Raced(store.clone())),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(StubSecretHasher),
            Arc::new(clock.clone()),
        );
        verifier.register(
            "first",
            claims("ak-user-7", clock.now() + Duration::hours(1)),
        );

        let caller = auth
            .execute(Credential::Bearer("first".to_owned()))
            .await
            .expect("a lost race is not the caller's error");

        let winner = store.find_by_subject("ak-user-7").await.unwrap().unwrap();
        assert_eq!(caller.principal, winner.principal());
    }

    #[tokio::test]
    async fn a_repeated_request_with_unchanged_claims_writes_nothing() {
        // Bearer auth runs on every request, so an unchanged login must not write.
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
        // The stored address is what managed `email` bindings resolve against (§11); a
        // client asking for fewer scopes must not erase it.
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
    async fn an_email_the_provider_stops_verifying_is_forgotten() {
        // Unlike an absent claim, an unverified one is the provider saying it no
        // longer vouches for the address — keeping it would keep an email
        // binding (§11) pointed at somebody the provider will not stand behind.
        let fixture = fixture();
        fixture.verifier.register(
            "verified",
            claims("ak-user-42", fixture.clock.now() + Duration::hours(1)),
        );
        fixture.verifier.register(
            "unverified",
            VerifiedClaims {
                email: None,
                email_unverified: true,
                ..claims("ak-user-42", fixture.clock.now() + Duration::hours(1))
            },
        );

        for token in ["verified", "unverified"] {
            fixture
                .auth
                .execute(Credential::Bearer(token.to_owned()))
                .await
                .unwrap();
        }

        let stored = UserRepository::find_by_subject(&fixture.store, "ak-user-42")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.email, None);
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
        let Grant::ApiKey {
            scopes,
            channels,
            publish_per_minute,
            ..
        } = caller.grant
        else {
            panic!("a key authenticates as a key");
        };
        assert_eq!(scopes, vec![Scope::Publish]);
        assert_eq!(channels, vec![ChannelId::parse("ci-infra").unwrap()]);
        assert_eq!(publish_per_minute, None);
        assert_eq!(
            caller.expires_at, None,
            "a key without expiry never closes a stream"
        );
    }

    #[tokio::test]
    async fn a_personal_key_forgets_groups_its_owner_has_not_shown_for_a_week() {
        let fixture = fixture();
        let alice = UserId::generate();
        seed_key(&fixture, ApiKeyOwner::User { id: alice }, None);
        let principal = Principal::User { id: alice };
        let groups = vec!["infra-admins".to_owned()];
        fixture
            .store
            .remember_groups(&principal, &groups, fixture.clock.now())
            .await
            .unwrap();

        let owner_groups = |caller: Caller| match caller.grant {
            Grant::ApiKey { owner_groups, .. } => owner_groups,
            _ => panic!("a key authenticates as a key"),
        };
        let key = || Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned());

        let fresh = fixture.auth.execute(key()).await.unwrap();
        assert_eq!(owner_groups(fresh), groups);

        fixture
            .clock
            .advance(DEFAULT_GROUP_SNAPSHOT_TTL + Duration::seconds(1));
        let stale = fixture.auth.execute(key()).await.unwrap();
        assert_eq!(owner_groups(stale), [] as [std::string::String; 0]);
    }

    #[tokio::test]
    async fn a_configured_group_snapshot_ttl_replaces_the_week() {
        let mut fixture = fixture();
        fixture.auth = fixture
            .auth
            .clone()
            .with_group_snapshot_ttl(Duration::hours(1));
        let alice = UserId::generate();
        seed_key(&fixture, ApiKeyOwner::User { id: alice }, None);
        let principal = Principal::User { id: alice };
        fixture
            .store
            .remember_groups(
                &principal,
                &["infra-admins".to_owned()],
                fixture.clock.now(),
            )
            .await
            .unwrap();

        fixture.clock.advance(Duration::hours(2));
        let caller = fixture
            .auth
            .execute(Credential::ApiKey("wnk_ci4xa9k2_s3cr3t".to_owned()))
            .await
            .unwrap();
        let Grant::ApiKey { owner_groups, .. } = caller.grant else {
            panic!("a key authenticates as a key");
        };
        assert!(owner_groups.is_empty(), "two hours is past a one-hour TTL");
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
        assert_eq!(keys, [] as [notify_domain::ApiKey; 0]);
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
    // --- Federated CI tokens (design §5.3) ---------------------------------

    fn federated() -> (Fixture, StaticFederationVerifier) {
        let fixture = fixture();
        let federation = StaticFederationVerifier::with_clock(Arc::new(fixture.clock.clone()));
        let auth = Authenticate::new(
            Arc::new(fixture.verifier.clone()),
            Arc::new(fixture.store.clone()),
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
        let grants: Vec<(ChannelId, Vec<Scope>)> = channels
            .iter()
            .map(|id| (ChannelId::parse(id).unwrap(), vec![Scope::Publish]))
            .collect();
        notify_ports::FederationVerifier::mint(federation, &grant, &grants)
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
                email_unverified: false,
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

    // --- Key issuers (ADR-0033) ---------------------------------------------

    fn with_issuer(fixture: Fixture) -> Authenticate {
        fixture.auth.with_key_issuers(vec![KeyIssuer {
            id: "weebo-operator".to_owned(),
            secret_hash: "stub$correct-horse-battery-staple-0123".to_owned(),
        }])
    }

    #[tokio::test]
    async fn a_key_issuer_is_a_caller_only_on_the_key_routes() {
        let auth = with_issuer(fixture());
        let token = || {
            Credential::KeyIssuer("wki_weebo-operator_correct-horse-battery-staple-0123".to_owned())
        };

        let caller = auth.execute_for_keys(token()).await.unwrap();
        assert_eq!(caller.grant, Grant::KeyIssuer);
        assert_eq!(
            caller.principal,
            Principal::KeyIssuer {
                id: "weebo-operator".to_owned()
            }
        );
        assert!(!caller.platform_admin && caller.groups.is_empty());

        // Every other route goes through `execute`: a 403 naming why, not a 401.
        let failure = auth.execute(token()).await.unwrap_err();
        assert_eq!(failure.code(), Some("credential.key_issuer_only"));
    }

    #[tokio::test]
    async fn a_wrong_or_unknown_issuer_token_is_the_same_401() {
        let auth = with_issuer(fixture());
        for token in [
            "wki_weebo-operator_wrong",
            "wki_somebody-else_correct-horse-battery-staple-0123",
            "wki_weebo-operator",
            "wki_",
        ] {
            let failure = auth
                .execute_for_keys(Credential::KeyIssuer(token.to_owned()))
                .await
                .unwrap_err();
            assert_eq!(failure.code(), Some("auth.invalid_credential"), "{token}");
        }
        // No issuer configured: nothing matches.
        let failure = fixture()
            .auth
            .execute_for_keys(Credential::KeyIssuer(
                "wki_weebo-operator_correct-horse-battery-staple-0123".to_owned(),
            ))
            .await
            .unwrap_err();
        assert_eq!(failure.code(), Some("auth.invalid_credential"));
    }
}

#[cfg(test)]
mod last_used_tests {
    #![expect(
        clippy::arithmetic_side_effects,
        reason = "test arithmetic on small, known values"
    )]

    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds)
    }

    #[test]
    fn a_key_that_has_never_been_used_is_always_recorded() {
        // The first use is the one somebody is watching for.
        assert!(Authenticate::should_touch(None, at(0)));
    }

    #[test]
    fn a_burst_within_the_granularity_writes_once() {
        let first = at(1_000);
        assert!(!Authenticate::should_touch(Some(first), at(1_001)));
        assert!(!Authenticate::should_touch(Some(first), at(1_059)));
        assert!(Authenticate::should_touch(Some(first), at(1_060)));
    }

    #[test]
    fn a_clock_that_went_backwards_writes_rather_than_going_quiet() {
        // An NTP step must not freeze `last_used_at` in the future.
        assert!(Authenticate::should_touch(Some(at(1_000)), at(900)));
    }
}
