//! Who exists: users, service accounts and API keys (design §5).
//!
//! Three small ports rather than one, because they have three different owners: the
//! OIDC login path writes users, the reconciler and the API write service accounts,
//! and only the key endpoints write keys. A single `IdentityRepository` would give
//! every one of them write access to all three.
//!
//! # Where the secret lives
//!
//! Nowhere in `notify-domain`. A key's hash is stored beside it and comes back
//! inside [`StoredApiKey`], which exists in this crate and never crosses into a
//! response DTO. Hashing and verification are their own port ([`SecretHasher`]) so
//! the domain owes nothing to Argon2, and so tests can run with a stub instead of
//! paying a deliberately expensive KDF on every case.

use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyPrefix, Principal, ServiceAccount, ServiceAccountId, User, UserId,
};
use time::OffsetDateTime;

use crate::error::PortResult;

/// Users.
#[async_trait::async_trait]
pub trait UserRepository: Send + Sync {
    /// Fetch by our own id.
    async fn get(&self, id: UserId) -> PortResult<Option<User>>;

    /// Fetch by OIDC subject — the lookup every authenticated request makes.
    async fn find_by_subject(&self, subject: &str) -> PortResult<Option<User>>;

    /// Fetch by email, for the individual `email` bindings in channel config
    /// (design §11).
    async fn find_by_email(&self, email: &str) -> PortResult<Option<User>>;

    /// Create on first login, or refresh the claims of an existing user.
    ///
    /// Overwriting claims wholesale is right for M1 and wrong from M2: once a user
    /// can edit their profile, a locally changed field must stop being overwritten
    /// by later syncs (design §2.4). That rule belongs to the use case, not here.
    async fn upsert(&self, user: &User) -> PortResult<()>;
}

/// Service accounts.
#[async_trait::async_trait]
pub trait ServiceAccountRepository: Send + Sync {
    /// Fetch one.
    async fn get(&self, id: &ServiceAccountId) -> PortResult<Option<ServiceAccount>>;

    /// Every service account, for `GET /serviceaccounts` and the console.
    async fn list(&self) -> PortResult<Vec<ServiceAccount>>;

    /// Create or update one.
    async fn upsert(&self, account: &ServiceAccount) -> PortResult<()>;

    /// Every GitOps-declared account, for reconciliation (design §11).
    async fn list_managed(&self) -> PortResult<Vec<ServiceAccount>>;
}

/// An API key as stored: the public half, plus the hash that proves ownership.
///
/// Lives here rather than in the domain so that no entity which can end up in a
/// response body has a field capable of leaking a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApiKey {
    /// Everything the API may show.
    pub key: ApiKey,
    /// The hash of the secret half, in whatever encoding [`SecretHasher`] produces.
    pub secret_hash: String,
}

/// API keys.
#[async_trait::async_trait]
pub trait ApiKeyRepository: Send + Sync {
    /// Look a key up by its indexed prefix.
    ///
    /// The prefix exists exactly so this is one indexed read: without it,
    /// authenticating would mean an Argon2 comparison against every key on the
    /// instance, which is a denial of service with extra steps.
    async fn find_by_prefix(&self, prefix: &ApiKeyPrefix) -> PortResult<Option<StoredApiKey>>;

    /// Fetch the public half by id.
    async fn get(&self, id: ApiKeyId) -> PortResult<Option<ApiKey>>;

    /// The keys a principal owns, secrets excluded — `GET /apikeys` shows metadata
    /// only, and there is no endpoint that can show a secret again (scenario T12).
    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<ApiKey>>;

    /// Store a newly created key.
    async fn insert(&self, stored: &StoredApiKey) -> PortResult<()>;

    /// Record a successful use.
    ///
    /// Best-effort by contract: this happens on the hot path of every authenticated
    /// request, and failing a publish because a `last_used` write lost a race would
    /// be a poor trade. Adapters may batch or coalesce it.
    async fn touch(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<()>;

    /// Revoke a key, effective immediately. Returns whether one was there.
    async fn revoke(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<bool>;

    /// Revoke every key of a principal at once — what deactivating a service
    /// account does (design §8, scenario T25).
    async fn revoke_all_for(&self, owner: &Principal, at: OffsetDateTime) -> PortResult<u64>;
}

/// Hashing and verifying key secrets.
///
/// A port of its own so the KDF is a wiring decision: Argon2id in production, a
/// stub in tests. Deliberately expensive functions and fast test suites are
/// otherwise in direct conflict, and the usual resolution — lowering the production
/// cost parameters until the tests are bearable — is the wrong one.
#[async_trait::async_trait]
pub trait SecretHasher: Send + Sync {
    /// Hash a freshly generated secret for storage.
    async fn hash(&self, secret: &str) -> PortResult<String>;

    /// Check a presented secret against a stored hash.
    ///
    /// Implementations compare in constant time. Returning `bool` rather than a
    /// `Result` for the mismatch case is deliberate: a wrong secret is an expected
    /// outcome, not an error worth a stack trace on every scan attempt.
    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool>;
}

/// Minting the secret half of a new API key.
///
/// Randomness is I/O in the sense that matters here: it is non-deterministic, so it
/// belongs behind a port rather than inside a use case that tests have to work
/// around. A double returns a fixed value and the assertions stay readable.
pub trait KeyGenerator: Send + Sync {
    /// Produce a fresh `(prefix, secret)` pair.
    ///
    /// The prefix is the indexed lookup half and need only be unique; the secret
    /// must come from a cryptographically secure source and never be logged.
    fn generate(&self) -> (ApiKeyPrefix, String);
}
