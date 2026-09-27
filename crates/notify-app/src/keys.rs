//! API keys: minting, listing, revoking, and a platform-admin's ceiling over one
//! (design §5.2, §8.2, D24).
//!
//! The secret is returned **once**, at creation, and there is no endpoint that can
//! show it again — only its Argon2 hash is stored. That is enforced by the shape of
//! the types rather than by discipline: [`notify_domain::ApiKey`] has no field that
//! could carry it (scenario T12).

use std::sync::Arc;

use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, ChannelId, DomainError, PresentedApiKey,
    Principal, Scope, ServiceAccount,
};
use notify_ports::{
    ApiKeyRepository, AuditEntry, AuditLog, Clock, Failure, KeyGenerator, SecretHasher,
    ServiceAccountRepository, StoredApiKey, actions,
};
use time::OffsetDateTime;

use crate::authz::{Authorizer, Caller, Grant};

/// Refused the same way a missing scope is: a `403` naming `manage`.
fn refuse() -> Failure {
    DomainError::ScopeMissing { scope: "manage" }.into()
}

/// Whether `caller` may administer the keys of `account`.
///
/// Its creator, or a platform admin. A managed account has no creator — the
/// configuration owns it (§11) — so for those the caller must also manage every
/// channel in question, the door that already applied before ownership was checked.
async fn administers_account(
    authorizer: &Authorizer,
    caller: &Caller,
    account: &ServiceAccount,
    channels: &[ChannelId],
) -> Result<bool, Failure> {
    if caller.platform_admin {
        return Ok(true);
    }
    match account.created_by {
        Some(creator) => Ok(caller.principal == Principal::User { id: creator }),
        None => manages_all(authorizer, caller, channels).await,
    }
}

async fn manages_all(
    authorizer: &Authorizer,
    caller: &Caller,
    channels: &[ChannelId],
) -> Result<bool, Failure> {
    for channel in channels {
        if authorizer
            .authorize(caller, channel, Scope::Manage)
            .await
            .is_err()
        {
            return Ok(false);
        }
    }
    Ok(!channels.is_empty())
}

/// Whether `caller` may see and revoke `key`: whoever it acts as, or — for a
/// channel's or a service account's key — whoever administers that owner.
async fn administers_key(
    authorizer: &Authorizer,
    accounts: &dyn ServiceAccountRepository,
    caller: &Caller,
    key: &ApiKey,
) -> Result<bool, Failure> {
    if key.principal() == caller.principal {
        return Ok(true);
    }
    // Administration is a person's session, not another credential (see
    // `CreateApiKey::execute`).
    if caller.grant != Grant::Membership {
        return Ok(false);
    }
    match &key.owner {
        ApiKeyOwner::User { .. } => Ok(false),
        ApiKeyOwner::Channel { id } => {
            manages_all(authorizer, caller, core::slice::from_ref(id)).await
        }
        ApiKeyOwner::ServiceAccount { id } => match accounts.get(id).await? {
            Some(account) => administers_account(authorizer, caller, &account, &key.channels).await,
            None => Ok(caller.platform_admin),
        },
    }
}

/// A freshly minted key: the record, plus the one and only sight of its secret.
#[derive(Debug, Clone)]
pub struct MintedApiKey {
    /// What `GET /apikeys` will show from now on.
    pub key: ApiKey,
    /// `wnk_<prefix>_<secret>`. Shown once; not recoverable.
    pub secret: String,
}

/// `POST /api/v1/apikeys` (design §8).
#[derive(Clone)]
pub struct CreateApiKey {
    keys: Arc<dyn ApiKeyRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    hasher: Arc<dyn SecretHasher>,
    generator: Arc<dyn KeyGenerator>,
    authorizer: Authorizer,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

/// What a caller asks for.
#[derive(Debug, Clone)]
pub struct NewApiKey {
    /// Who the key acts as (design §5.2).
    pub owner: ApiKeyOwner,
    /// A label, so a human can tell two keys apart before revoking one.
    pub label: Option<String>,
    /// What it may do.
    pub scopes: Vec<Scope>,
    /// Where it may do it.
    pub channels: Vec<ChannelId>,
    /// Optional expiry.
    pub expires_at: Option<OffsetDateTime>,
}

impl core::fmt::Debug for CreateApiKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CreateApiKey").finish_non_exhaustive()
    }
}

impl CreateApiKey {
    /// Wire it.
    #[must_use]
    pub fn new(
        keys: Arc<dyn ApiKeyRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
        hasher: Arc<dyn SecretHasher>,
        generator: Arc<dyn KeyGenerator>,
        authorizer: Authorizer,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            keys,
            accounts,
            hasher,
            generator,
            authorizer,
            audit,
            clock,
        }
    }

    /// Mint one.
    ///
    /// The caller must hold, in every channel the key names, at least the scopes the
    /// key will carry. Without that check, `manage` on one channel would be a way to
    /// mint publish rights on another.
    pub async fn execute(
        &self,
        caller: &Caller,
        request: NewApiKey,
    ) -> Result<MintedApiKey, Failure> {
        if request.channels.is_empty() {
            return Err(DomainError::Empty {
                field: "api_key.channels",
            }
            .into());
        }
        if request.scopes.is_empty() {
            return Err(DomainError::Empty {
                field: "api_key.scopes",
            }
            .into());
        }

        // Only a person's own session mints. A key or a CI token minting keys would
        // turn a one-day or fifteen-minute credential into a permanent one, and
        // outlive the revocation of the one it came from.
        if caller.grant != Grant::Membership {
            return Err(refuse());
        }

        // A personal key is for the caller and nobody else: minting one "for" Bob
        // would let its holder act as Bob — his sync, his devices, his silences.
        // A service account's key needs whoever administers that account.
        let needs_manage = match &request.owner {
            ApiKeyOwner::User { id } => {
                if caller.principal != (Principal::User { id: *id }) {
                    return Err(refuse());
                }
                false
            }
            ApiKeyOwner::ServiceAccount { id } => {
                let account = self.accounts.get(id).await?.ok_or_else(refuse)?;
                if !administers_account(&self.authorizer, caller, &account, &request.channels)
                    .await?
                {
                    return Err(refuse());
                }
                true
            }
            ApiKeyOwner::Channel { .. } => true,
        };

        for channel in &request.channels {
            if needs_manage {
                self.authorizer
                    .authorize(caller, channel, Scope::Manage)
                    .await?;
            }
            for scope in &request.scopes {
                self.authorizer.authorize(caller, channel, *scope).await?;
            }
        }

        let (prefix, secret) = self.generator.generate();
        let secret_hash = self.hasher.hash(&secret).await?;
        let now = self.clock.now();

        let key = ApiKey {
            id: ApiKeyId::generate(),
            prefix: prefix.clone(),
            label: request.label,
            owner: request.owner,
            scopes: request.scopes,
            channels: request.channels,
            // Never set at creation, whoever mints the key: only a platform-admin
            // may narrow it, and only after it exists (§8.2, D24).
            publish_per_minute: None,
            created_at: now,
            expires_at: request.expires_at,
            last_used_at: None,
            revoked_at: None,
        };

        self.keys
            .insert(&StoredApiKey {
                key: key.clone(),
                secret_hash,
            })
            .await?;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::APIKEY_CREATED,
                // The prefix, never the secret. An audit log that quotes a
                // credential is a credential store nobody thinks of as one.
                target: Some(prefix.to_string()),
                channel: key.channels.first().cloned(),
                detail: serde_json::to_value(&key.scopes).ok(),
                at: now,
            })
            .await?;

        Ok(MintedApiKey {
            secret: format!("{}{prefix}_{secret}", PresentedApiKey::SCHEME),
            key,
        })
    }
}

/// `GET /api/v1/apikeys` — metadata only (design §8).
#[derive(Clone)]
pub struct ListApiKeys {
    keys: Arc<dyn ApiKeyRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    authorizer: Authorizer,
}

impl core::fmt::Debug for ListApiKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListApiKeys").finish_non_exhaustive()
    }
}

impl ListApiKeys {
    /// Wire it.
    #[must_use]
    pub fn new(
        keys: Arc<dyn ApiKeyRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
        authorizer: Authorizer,
    ) -> Self {
        Self {
            keys,
            accounts,
            authorizer,
        }
    }

    /// The caller's own keys, those of the service accounts it administers, and
    /// those of the channels it manages.
    pub async fn execute(&self, caller: &Caller) -> Result<Vec<ApiKey>, Failure> {
        let mut keys = self.keys.list_for(&caller.principal).await?;
        for channel in self.authorizer.managed_channels(caller).await? {
            keys.extend(self.keys.list_for_channel(&channel).await?);
        }
        if caller.grant == Grant::Membership {
            for account in self.accounts.list().await? {
                for key in self.keys.list_for(&account.principal()).await? {
                    if administers_account(&self.authorizer, caller, &account, &key.channels)
                        .await?
                    {
                        keys.push(key);
                    }
                }
            }
        }
        Ok(keys)
    }
}

/// `DELETE /api/v1/apikeys/{id}` — revoke, effective immediately (design §8).
#[derive(Clone)]
pub struct RevokeApiKey {
    keys: Arc<dyn ApiKeyRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    authorizer: Authorizer,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for RevokeApiKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RevokeApiKey").finish_non_exhaustive()
    }
}

impl RevokeApiKey {
    /// Wire it.
    #[must_use]
    pub fn new(
        keys: Arc<dyn ApiKeyRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
        authorizer: Authorizer,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            keys,
            accounts,
            authorizer,
            audit,
            clock,
        }
    }

    /// Revoke a key the caller owns or administers.
    ///
    /// Whoever it acts as may always revoke it: that keeps the emergency path —
    /// "this leaked, kill it" — available to the person most likely to notice
    /// first. A channel's key is also revocable by that channel's managers, and a
    /// service account's by whoever administers the account; otherwise the admin
    /// who minted a channel key could never kill it.
    pub async fn execute(&self, caller: &Caller, id: ApiKeyId) -> Result<bool, Failure> {
        let Some(key) = self.keys.get(id).await? else {
            return Ok(false);
        };
        if !administers_key(&self.authorizer, self.accounts.as_ref(), caller, &key).await? {
            // Same answer as "no such key": whether an id exists is not something a
            // stranger gets to probe.
            return Ok(false);
        }

        let now = self.clock.now();
        let revoked = self.keys.revoke(id, now).await?;
        if revoked {
            self.audit
                .record(&AuditEntry {
                    actor: caller.principal.clone().into(),
                    action: actions::APIKEY_REVOKED,
                    target: Some(key.prefix.to_string()),
                    channel: key.channels.first().cloned(),
                    detail: None,
                    at: now,
                })
                .await?;
        }
        Ok(revoked)
    }
}

/// `PATCH /api/v1/apikeys/{id}/limit` — a platform-admin's ceiling on one key's
/// own publish rate (design §8.2, D24).
#[derive(Clone)]
pub struct SetApiKeyLimit {
    keys: Arc<dyn ApiKeyRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for SetApiKeyLimit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SetApiKeyLimit").finish_non_exhaustive()
    }
}

impl SetApiKeyLimit {
    /// Wire it.
    #[must_use]
    pub fn new(
        keys: Arc<dyn ApiKeyRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { keys, audit, clock }
    }

    /// Set, or clear (`None`), the caller's chosen key's ceiling.
    ///
    /// Platform-admin only, and deliberately not also the key's own owner the way
    /// [`RevokeApiKey`] is: a key that could raise its own ceiling would not be a
    /// ceiling. The role check comes before the lookup, unlike `RevokeApiKey`'s
    /// ownership check — there is no legitimate reason a non-admin would ever ask
    /// this question about *any* key, including their own, so there is nothing to
    /// gain by answering "no such key" first.
    ///
    /// `None` covers both a missing id and a caller without the role — same
    /// reasoning as `RevokeApiKey`, a different door.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: ApiKeyId,
        publish_per_minute: Option<u32>,
    ) -> Result<Option<ApiKey>, Failure> {
        caller.ensure_platform_admin()?;

        let Some(mut key) = self.keys.get(id).await? else {
            return Ok(None);
        };

        if key.publish_per_minute == publish_per_minute {
            // Nothing moved: the same rule `UpdateChannel` follows for its own
            // policy — no write, no audit entry claiming a change that did not
            // happen.
            return Ok(Some(key));
        }

        let now = self.clock.now();
        self.keys.set_publish_limit(id, publish_per_minute).await?;
        key.publish_per_minute = publish_per_minute;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::APIKEY_LIMIT_SET,
                target: Some(key.prefix.to_string()),
                channel: key.channels.first().cloned(),
                // The resulting value, not the request body — an audit log is read
                // to answer "what was it set to" (same reasoning as
                // `CHANNEL_POLICY_UPDATED`).
                detail: Some(serde_json::json!({ "publish_per_minute": publish_per_minute })),
                at: now,
            })
            .await?;

        Ok(Some(key))
    }
}

/// The alphabet both halves of a key are drawn from.
///
/// Lowercase alphanumeric, which is what `ApiKeyPrefix::parse` accepts and what the
/// old ULID-derived form happened to produce — so a key still looks like a key. It
/// deliberately excludes `_`, the separator `PresentedApiKey::parse` splits on.
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// How many characters of secret. 52 × log2(36) ≈ **268 bits**, comfortably past the
/// point where the number stops mattering, and the same length the previous
/// implementation produced.
const SECRET_LEN: usize = 52;

/// Where a key's two halves come from in production: the operating system's CSPRNG,
/// and nothing else.
///
/// A separate generator port exists so tests can be deterministic; this is the real
/// one.
///
/// # Why not the id generator
///
/// This used to concatenate two ULIDs and take a slice of a third, on the reasoning
/// that a ULID already carries 80 bits of randomness. That was true, and it was
/// still the wrong source: `ApiKeyId::generate()` is the *general-purpose
/// identifier* constructor, used all over the domain for values that are printed in
/// URLs and logs. Its contract is uniqueness, not unpredictability.
///
/// Nothing was broken — `Ulid::new()` draws from a CSPRNG today — but the coupling
/// meant a credential's entropy depended on an implementation detail of an id type
/// that no one editing it would think of as credential material. ULID libraries
/// commonly offer a *monotonic* mode in which the random tail increments within a
/// millisecond; adopting it would have made key secrets predictable from published
/// identifiers, silently and completely. Reading the CSPRNG directly costs three
/// lines and removes the question.
///
/// [`notify_ports::KeyGenerator`] already said the secret "must come from a
/// cryptographically secure source". This is that, said in code.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsRngKeyGenerator;

impl KeyGenerator for OsRngKeyGenerator {
    fn generate(&self) -> (ApiKeyPrefix, String) {
        let prefix = random_string(ApiKeyPrefix::LEN);
        let secret = random_string(SECRET_LEN);

        let prefix = ApiKeyPrefix::parse(&prefix).unwrap_or_else(|_| {
            // Unreachable: every byte of `random_string` is drawn from `ALPHABET`,
            // which is exactly what `parse` accepts. A fixed fallback would be worse
            // than a slightly odd prefix, so build a valid one rather than panic —
            // panicking is denied in this workspace.
            ApiKeyPrefix::parse("00000000").unwrap_or_else(|_| unreachable!())
        });

        (prefix, secret)
    }
}

/// `len` characters drawn uniformly from [`ALPHABET`].
///
/// Rejection sampling rather than `byte % 36`: 256 is not a multiple of 36, so the
/// modulo would make the first four letters about 14% likelier than the rest. That
/// costs a handful of bits on a value whose whole job is to be unguessable, and
/// discarding the 4 bytes in 256 that fall outside the last whole multiple costs
/// nothing anybody can measure.
fn random_string(len: usize) -> String {
    // The largest multiple of the alphabet that fits in a byte; anything at or above
    // it is redrawn.
    const CEILING: u8 = 252; // 36 × 7

    let mut out = String::with_capacity(len);
    let mut buffer = [0_u8; 64];

    while out.len() < len {
        // A failure here means the OS could not produce randomness, which is not a
        // condition a process can sensibly continue through — but this signature
        // cannot report it, so retry rather than silently emit a weak secret. In
        // practice `getrandom` on a running Linux kernel does not fail.
        if getrandom::fill(&mut buffer).is_err() {
            continue;
        }
        for &byte in &buffer {
            if out.len() == len {
                break;
            }
            if byte < CEILING {
                out.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Channel, Membership, ServiceAccountId, UserId};
    use notify_ports::test_utils::{CountingKeyGenerator, InMemory, StubSecretHasher, TestClock};

    use super::*;
    use crate::authz::Grant;

    struct Fixture {
        store: InMemory,
        create: CreateApiKey,
        list: ListApiKeys,
        revoke: RevokeApiKey,
        set_limit: SetApiKeyLimit,
        alice: Principal,
    }

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn fixture(scopes: Vec<Scope>) -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };

        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(alice.clone(), scopes).unwrap());
        store.seed_channel(channel);

        Fixture {
            create: CreateApiKey::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(StubSecretHasher),
                Arc::new(CountingKeyGenerator::new()),
                Authorizer::new(Arc::new(store.clone())),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            list: ListApiKeys::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Authorizer::new(Arc::new(store.clone())),
            ),
            revoke: RevokeApiKey::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Authorizer::new(Arc::new(store.clone())),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            set_limit: SetApiKeyLimit::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock),
            ),
            store,
            alice,
        }
    }

    fn platform_admin(principal: Principal) -> Caller {
        Caller {
            principal,
            grant: Grant::Membership,
            groups: Vec::new(),
            platform_admin: true,
            expires_at: None,
        }
    }

    fn request(owner: ApiKeyOwner, scopes: Vec<Scope>) -> NewApiKey {
        NewApiKey {
            owner,
            label: Some("CI".to_owned()),
            scopes,
            channels: vec![channel_id()],
            expires_at: None,
        }
    }

    // --- T12 · Keys (design §3.4) -----------------------------------------

    #[tokio::test]
    async fn critical_t12_the_secret_is_shown_once_and_never_again() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());

        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();

        assert!(minted.secret.starts_with("wnk_"));
        assert!(PresentedApiKey::parse(&minted.secret).is_ok());

        let listed = fixture.list.execute(&caller).await.unwrap();
        let rendered = serde_json::to_string(&listed).unwrap();
        assert!(
            !rendered.contains("test-secret"),
            "no endpoint may show a secret again"
        );
    }

    #[tokio::test]
    async fn critical_t12_a_key_cannot_grant_more_than_its_creator_holds() {
        // Alice can only subscribe here.
        let fixture = fixture(vec![Scope::Subscribe]);
        let caller = Caller::member(fixture.alice.clone());

        assert!(
            fixture
                .create
                .execute(
                    &caller,
                    request(
                        ApiKeyOwner::User {
                            id: match fixture.alice {
                                Principal::User { id } => id,
                                _ => unreachable!(),
                            }
                        },
                        vec![Scope::Publish],
                    ),
                )
                .await
                .is_err(),
            "otherwise a read-only member mints themselves a publisher"
        );
    }

    #[tokio::test]
    async fn a_channel_key_needs_manage_on_that_channel() {
        let fixture = fixture(vec![Scope::Publish, Scope::Subscribe]);
        let caller = Caller::member(fixture.alice.clone());

        assert!(
            fixture
                .create
                .execute(
                    &caller,
                    request(
                        ApiKeyOwner::Channel { id: channel_id() },
                        vec![Scope::Publish]
                    ),
                )
                .await
                .is_err(),
            "a channel key is administrative, not personal"
        );
    }

    #[tokio::test]
    async fn a_key_with_no_channels_or_no_scopes_is_refused() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());

        let no_channels = NewApiKey {
            channels: Vec::new(),
            ..request(
                ApiKeyOwner::Channel { id: channel_id() },
                vec![Scope::Publish],
            )
        };
        assert!(fixture.create.execute(&caller, no_channels).await.is_err());

        let no_scopes = request(ApiKeyOwner::Channel { id: channel_id() }, Vec::new());
        assert!(fixture.create.execute(&caller, no_scopes).await.is_err());
    }

    #[tokio::test]
    async fn revoking_is_audited_and_only_the_owner_may_do_it() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());

        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::User {
                        id: match fixture.alice {
                            Principal::User { id } => id,
                            _ => unreachable!(),
                        },
                    },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();

        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });
        assert!(
            !fixture
                .revoke
                .execute(&stranger, minted.key.id)
                .await
                .unwrap(),
            "whether an id exists is not something a stranger gets to probe"
        );

        assert!(
            fixture
                .revoke
                .execute(&caller, minted.key.id)
                .await
                .unwrap()
        );
        assert!(
            fixture
                .store
                .audit_entries()
                .iter()
                .any(|entry| entry.action.as_str() == "apikey.revoked")
        );
    }

    fn alice_id(fixture: &Fixture) -> UserId {
        match fixture.alice {
            Principal::User { id } => id,
            _ => unreachable!(),
        }
    }

    async fn seed_account(fixture: &Fixture, created_by: Option<UserId>) -> ServiceAccountId {
        let id = ServiceAccountId::parse("bot-argocd").unwrap();
        let origin = if created_by.is_some() {
            notify_domain::Origin::Dynamic
        } else {
            notify_domain::Origin::Managed
        };
        let account = ServiceAccount::new(
            id.clone(),
            None,
            None,
            origin,
            created_by,
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        ServiceAccountRepository::upsert(&fixture.store, &account)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_personal_key_cannot_be_minted_for_somebody_else() {
        // Manage everywhere is not a way to become Bob.
        let fixture = fixture(vec![Scope::Subscribe, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());

        let failure = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::User {
                        id: UserId::generate(),
                    },
                    vec![Scope::Subscribe],
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code(), Some("scope.missing"));
    }

    #[tokio::test]
    async fn a_service_accounts_key_needs_whoever_administers_it() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let bob = UserId::generate();
        let account = seed_account(&fixture, Some(bob)).await;

        assert!(
            fixture
                .create
                .execute(
                    &caller,
                    request(
                        ApiKeyOwner::ServiceAccount {
                            id: account.clone()
                        },
                        vec![Scope::Publish]
                    )
                )
                .await
                .is_err(),
            "managing the channel does not make Bob's bot alice's to speak as"
        );
        assert!(
            fixture
                .create
                .execute(
                    &Caller::member(Principal::User { id: bob }),
                    request(
                        ApiKeyOwner::ServiceAccount { id: account },
                        vec![Scope::Publish]
                    )
                )
                .await
                .is_err(),
            "and its creator still needs manage on the channels it names"
        );
    }

    #[tokio::test]
    async fn a_managed_accounts_key_is_minted_by_the_channels_manager() {
        // The configuration owns it; `task dev:argocd-key` is this path.
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let account = seed_account(&fixture, None).await;

        assert!(
            fixture
                .create
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    request(
                        ApiKeyOwner::ServiceAccount { id: account },
                        vec![Scope::Publish]
                    )
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_key_cannot_mint_another_key() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let as_key = Caller {
            principal: fixture.alice.clone(),
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Publish, Scope::Manage],
                channels: vec![channel_id()],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };

        assert!(
            fixture
                .create
                .execute(
                    &as_key,
                    request(
                        ApiKeyOwner::User {
                            id: alice_id(&fixture)
                        },
                        vec![Scope::Publish]
                    )
                )
                .await
                .is_err(),
            "a leaked one-day key must not mint itself a permanent copy"
        );
    }

    #[tokio::test]
    async fn a_channel_key_is_revocable_by_its_manager() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();

        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });
        assert!(
            !fixture
                .revoke
                .execute(&stranger, minted.key.id)
                .await
                .unwrap()
        );
        assert!(
            fixture
                .revoke
                .execute(&caller, minted.key.id)
                .await
                .unwrap(),
            "otherwise the admin who minted a leaked channel key could never kill it"
        );
    }

    #[tokio::test]
    async fn a_channel_key_is_listed_to_its_managers_only() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();

        let listed = fixture.list.execute(&caller).await.unwrap();
        assert!(listed.iter().any(|key| key.id == minted.key.id));

        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });
        assert!(fixture.list.execute(&stranger).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_service_accounts_keys_are_listed_and_revoked_by_its_creator() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let account = seed_account(&fixture, Some(alice_id(&fixture))).await;
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::ServiceAccount { id: account },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();

        let listed = fixture.list.execute(&caller).await.unwrap();
        assert!(listed.iter().any(|key| key.id == minted.key.id));
        assert!(
            fixture
                .revoke
                .execute(&caller, minted.key.id)
                .await
                .unwrap()
        );
    }

    // --- D24 · a platform-admin's ceiling on one key -----------------------

    #[tokio::test]
    async fn a_platform_admin_sets_a_keys_own_ceiling() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();
        assert_eq!(minted.key.publish_per_minute, None);

        let admin = platform_admin(Principal::User {
            id: UserId::generate(),
        });
        let updated = fixture
            .set_limit
            .execute(&admin, minted.key.id, Some(5))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(updated.publish_per_minute, Some(5));
        assert!(
            fixture
                .store
                .audit_entries()
                .iter()
                .any(|entry| entry.action.as_str() == "apikey.limit_set")
        );
    }

    #[tokio::test]
    async fn the_keys_own_owner_cannot_set_its_ceiling() {
        // The whole point of D24: a self-service ceiling is not a ceiling. Same
        // caller that minted the key, no platform role.
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();

        assert_eq!(
            fixture
                .set_limit
                .execute(&caller, minted.key.id, Some(5))
                .await
                .unwrap_err()
                .code(),
            Some("platform.role_required")
        );
    }

    #[tokio::test]
    async fn setting_a_nonexistent_keys_limit_answers_none() {
        let fixture = fixture(vec![Scope::Publish]);
        let admin = platform_admin(Principal::User {
            id: UserId::generate(),
        });

        assert_eq!(
            fixture
                .set_limit
                .execute(&admin, ApiKeyId::generate(), Some(5))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn setting_the_same_limit_twice_writes_no_second_audit_entry() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();
        let admin = platform_admin(Principal::User {
            id: UserId::generate(),
        });

        fixture
            .set_limit
            .execute(&admin, minted.key.id, Some(5))
            .await
            .unwrap();
        fixture
            .set_limit
            .execute(&admin, minted.key.id, Some(5))
            .await
            .unwrap();

        assert_eq!(
            fixture
                .store
                .audit_entries()
                .iter()
                .filter(|entry| entry.action.as_str() == "apikey.limit_set")
                .count(),
            1,
            "a PATCH that changes nothing is idempotent, not a second event"
        );
    }

    #[tokio::test]
    async fn clearing_a_limit_is_the_same_operation_as_setting_one() {
        let fixture = fixture(vec![Scope::Publish, Scope::Manage]);
        let caller = Caller::member(fixture.alice.clone());
        let minted = fixture
            .create
            .execute(
                &caller,
                request(
                    ApiKeyOwner::Channel { id: channel_id() },
                    vec![Scope::Publish],
                ),
            )
            .await
            .unwrap();
        let admin = platform_admin(Principal::User {
            id: UserId::generate(),
        });
        fixture
            .set_limit
            .execute(&admin, minted.key.id, Some(5))
            .await
            .unwrap();

        let cleared = fixture
            .set_limit
            .execute(&admin, minted.key.id, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(cleared.publish_per_minute, None);
    }

    #[test]
    fn the_production_generator_makes_a_valid_prefix_and_two_distinct_keys() {
        let generator = OsRngKeyGenerator;
        let (first_prefix, first_secret) = generator.generate();
        let (second_prefix, second_secret) = generator.generate();

        assert_eq!(first_prefix.as_str().len(), ApiKeyPrefix::LEN);
        assert_ne!(first_secret, second_secret);
        // Prefixes come from the random tail of a ULID, so two minted in the same
        // millisecond do not collide.
        assert_ne!(first_prefix, second_prefix);
    }
}

#[cfg(test)]
mod generator_tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use std::collections::HashSet;

    use super::*;

    #[test]
    fn both_halves_are_drawn_from_the_accepted_alphabet() {
        let (prefix, secret) = OsRngKeyGenerator.generate();

        assert_eq!(prefix.as_str().len(), ApiKeyPrefix::LEN);
        assert_eq!(secret.len(), SECRET_LEN);
        for value in [prefix.as_str(), secret.as_str()] {
            assert!(
                value.bytes().all(|b| ALPHABET.contains(&b)),
                "`{value}` left the alphabet, which `ApiKeyPrefix::parse` would refuse"
            );
        }
    }

    #[test]
    fn the_secret_never_contains_the_separator() {
        // `PresentedApiKey::parse` splits `wnk_<prefix>_<secret>` on `_`.
        for _ in 0..64 {
            let (_, secret) = OsRngKeyGenerator.generate();
            assert!(!secret.contains('_'));
        }
    }

    #[test]
    fn a_minted_key_round_trips_through_the_presented_form() {
        let (prefix, secret) = OsRngKeyGenerator.generate();
        let presented = format!("wnk_{prefix}_{secret}");

        let parsed = notify_domain::PresentedApiKey::parse(&presented).unwrap();
        assert_eq!(parsed.prefix, prefix);
        assert_eq!(parsed.secret, secret);
    }

    #[test]
    fn nothing_repeats_across_a_batch() {
        // The old implementation drew from the id generator, where a monotonic mode
        // would have made successive values related. Distinctness is the cheap
        // observable half of "unpredictable".
        let mut prefixes = HashSet::new();
        let mut secrets = HashSet::new();
        for _ in 0..512 {
            let (prefix, secret) = OsRngKeyGenerator.generate();
            assert!(prefixes.insert(prefix.as_str().to_owned()));
            assert!(secrets.insert(secret));
        }
    }

    #[test]
    fn every_character_of_the_alphabet_is_reachable() {
        // Guards the rejection sampling: an off-by-one in the ceiling would silently
        // make part of the alphabet unreachable and cost entropy without failing.
        let mut seen = HashSet::new();
        for _ in 0..500 {
            let (_, secret) = OsRngKeyGenerator.generate();
            seen.extend(secret.bytes());
        }
        assert_eq!(
            seen.len(),
            ALPHABET.len(),
            "some characters never appeared: {:?}",
            ALPHABET
                .iter()
                .filter(|b| !seen.contains(b))
                .map(|&b| char::from(b))
                .collect::<Vec<_>>()
        );
    }
}
