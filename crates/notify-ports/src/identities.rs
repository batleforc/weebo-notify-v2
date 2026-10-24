//! Who exists: users, service accounts and API keys (§5), as three ports with
//! three different writers. Secret hashes stay in [`StoredApiKey`], never in a DTO.

use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyPrefix, AttachmentId, ChannelId, Principal, ServiceAccount,
    ServiceAccountId, User, UserId,
};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// What looking a user up by email found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailMatch {
    /// Nobody has that address — yet; they may not have logged in.
    Nobody,
    /// Exactly one user.
    One(Box<User>),
    /// More than one user has that address, so it identifies nobody.
    Ambiguous,
}

/// Users.
#[async_trait::async_trait]
pub trait UserRepository: Send + Sync {
    /// Fetch by our own id.
    async fn get(&self, id: UserId) -> PortResult<Option<User>>;

    /// Fetch by OIDC subject — the lookup every authenticated request makes.
    async fn find_by_subject(&self, subject: &str) -> PortResult<Option<User>>;

    /// Fetch by email (case-insensitive), for individual `email` bindings in channel
    /// config (§11). `users.email` is not unique: several matches are [`EmailMatch::Ambiguous`].
    async fn find_by_email(&self, email: &str) -> PortResult<EmailMatch>;

    /// Whether `avatar` is some user's current avatar, so `GET /avatars/{id}` cannot
    /// read any other object in the shared store (§2.4).
    async fn is_avatar(&self, avatar: AttachmentId) -> PortResult<bool>;

    /// Create on first login, or refresh the claims of an existing user. Protecting
    /// locally edited fields is the use case's job (§2.4).
    /// Up to `limit` users whose name or email contains `text`, case-insensitively.
    async fn lookup(&self, text: &str, limit: u32) -> PortResult<Vec<User>>;

    async fn upsert(&self, user: &User) -> PortResult<()>;
}

/// Service accounts.
#[async_trait::async_trait]
pub trait ServiceAccountRepository: Send + Sync {
    /// Fetch one.
    async fn get(&self, id: &ServiceAccountId) -> PortResult<Option<ServiceAccount>>;

    /// Every service account, for the key listing's ownership checks.
    async fn list(&self) -> PortResult<Vec<ServiceAccount>>;

    /// One page of them, in creation order, for `GET /serviceaccounts` and the console.
    async fn list_page(&self, page: PageRequest) -> PortResult<Page<ServiceAccount>>;

    /// Create or update one.
    async fn upsert(&self, account: &ServiceAccount) -> PortResult<()>;

    /// Every GitOps-declared account, for reconciliation (design §11).
    async fn list_managed(&self) -> PortResult<Vec<ServiceAccount>>;
}

/// An API key as stored, with its secret hash; kept out of the domain so no
/// response entity can leak a credential.
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
    /// Look a key up by its indexed prefix (one read, not an Argon2 pass per key).
    async fn find_by_prefix(&self, prefix: &ApiKeyPrefix) -> PortResult<Option<StoredApiKey>>;

    /// Fetch the public half by id.
    async fn get(&self, id: ApiKeyId) -> PortResult<Option<ApiKey>>;

    /// The keys a principal owns, secrets excluded (T12).
    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<ApiKey>>;

    /// The keys a channel owns (`ApiKeyOwner::Channel`), secrets excluded.
    async fn list_for_channel(&self, channel: &ChannelId) -> PortResult<Vec<ApiKey>>;

    /// Store a newly created key.
    async fn insert(&self, stored: &StoredApiKey) -> PortResult<()>;

    /// Record a successful use. Best-effort on the hot path; adapters may coalesce it.
    async fn touch(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<()>;

    /// Revoke a key, effective immediately. Returns whether one was there.
    async fn revoke(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<bool>;

    /// Set, or clear, a key's own publish ceiling (§8.2). The caller has already
    /// fetched and authorised the key, so no existence check.
    async fn set_publish_limit(
        &self,
        id: ApiKeyId,
        publish_per_minute: Option<u32>,
    ) -> PortResult<()>;

    /// Revoke every key of a principal at once, on service-account deactivation (§8, T25).
    async fn revoke_all_for(&self, owner: &Principal, at: OffsetDateTime) -> PortResult<u64>;
}

/// Hashing and verifying key secrets: Argon2id in production, a stub in tests,
/// so production cost parameters never get lowered for test speed.
#[async_trait::async_trait]
pub trait SecretHasher: Send + Sync {
    /// Hash a freshly generated secret for storage.
    async fn hash(&self, secret: &str) -> PortResult<String>;

    /// Check a presented secret against a stored hash, in constant time. A mismatch
    /// is `Ok(false)`, not an error.
    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool>;
}

/// Minting the secret half of a new API key; behind a port because randomness
/// is non-deterministic.
pub trait KeyGenerator: Send + Sync {
    /// Produce a fresh `(prefix, secret)` pair. The secret must come from a CSPRNG
    /// and never be logged.
    fn generate(&self) -> (ApiKeyPrefix, String);
}
