//! API keys: minting, listing and revoking (design §5.2).
//!
//! The secret is returned **once**, at creation, and there is no endpoint that can
//! show it again — only its Argon2 hash is stored. That is enforced by the shape of
//! the types rather than by discipline: [`notify_domain::ApiKey`] has no field that
//! could carry it (scenario T12).

use std::sync::Arc;

use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, ChannelId, DomainError, PresentedApiKey,
    Principal, Scope,
};
use notify_ports::{
    ApiKeyRepository, AuditEntry, AuditLog, Clock, Failure, KeyGenerator, SecretHasher,
    StoredApiKey, actions,
};
use time::OffsetDateTime;

use crate::authz::{Authorizer, Caller};

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
        hasher: Arc<dyn SecretHasher>,
        generator: Arc<dyn KeyGenerator>,
        authorizer: Authorizer,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            keys,
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

        // A key for a channel or a service account is administrative; a personal key
        // only needs the caller to already hold what it grants.
        let needs_manage = !matches!(&request.owner, ApiKeyOwner::User { id }
            if Principal::User { id: *id } == caller.principal);

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
}

impl core::fmt::Debug for ListApiKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListApiKeys").finish_non_exhaustive()
    }
}

impl ListApiKeys {
    /// Wire it.
    #[must_use]
    pub fn new(keys: Arc<dyn ApiKeyRepository>) -> Self {
        Self { keys }
    }

    /// The caller's own keys.
    pub async fn execute(&self, caller: &Caller) -> Result<Vec<ApiKey>, Failure> {
        Ok(self.keys.list_for(&caller.principal).await?)
    }
}

/// `DELETE /api/v1/apikeys/{id}` — revoke, effective immediately (design §8).
#[derive(Clone)]
pub struct RevokeApiKey {
    keys: Arc<dyn ApiKeyRepository>,
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
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { keys, audit, clock }
    }

    /// Revoke a key the caller owns.
    ///
    /// Ownership rather than `manage`: a key is revoked by whoever it acts as. That
    /// keeps the emergency path — "this leaked, kill it" — available to the person
    /// most likely to notice first.
    pub async fn execute(&self, caller: &Caller, id: ApiKeyId) -> Result<bool, Failure> {
        let Some(key) = self.keys.get(id).await? else {
            return Ok(false);
        };
        if key.principal() != caller.principal {
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

/// Where a key's prefix comes from in production: 8 characters of the key id.
///
/// A separate generator port exists so tests can be deterministic; this is the real
/// one, and it needs no randomness of its own — a ULID already carries 80 bits of
/// it.
#[derive(Debug, Clone, Copy, Default)]
pub struct UlidKeyGenerator;

impl KeyGenerator for UlidKeyGenerator {
    fn generate(&self) -> (ApiKeyPrefix, String) {
        let prefix_source = ApiKeyId::generate().as_ulid().to_string().to_lowercase();
        let secret_source = format!(
            "{}{}",
            ApiKeyId::generate().as_ulid(),
            ApiKeyId::generate().as_ulid()
        );

        // Take the *last* characters of the ULID: the leading ones are the
        // timestamp, so two keys minted in the same millisecond would share them.
        let tail: String = prefix_source
            .chars()
            .rev()
            .take(ApiKeyPrefix::LEN)
            .collect();
        let prefix = ApiKeyPrefix::parse(&tail).unwrap_or_else(|_| {
            // Unreachable: a Crockford base32 tail is always alphanumeric. Falling
            // back to a fixed prefix would be worse than a slightly odd one, so
            // build a valid one from scratch instead.
            ApiKeyPrefix::parse("00000000").unwrap_or_else(|_| unreachable!())
        });

        (prefix, secret_source.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Channel, Membership, UserId};
    use notify_ports::test_utils::{CountingKeyGenerator, InMemory, StubSecretHasher, TestClock};

    use super::*;

    struct Fixture {
        store: InMemory,
        create: CreateApiKey,
        list: ListApiKeys,
        revoke: RevokeApiKey,
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
                Arc::new(StubSecretHasher),
                Arc::new(CountingKeyGenerator::new()),
                Authorizer::new(Arc::new(store.clone())),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            list: ListApiKeys::new(Arc::new(store.clone())),
            revoke: RevokeApiKey::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock),
            ),
            store,
            alice,
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

    #[test]
    fn the_production_generator_makes_a_valid_prefix_and_two_distinct_keys() {
        let generator = UlidKeyGenerator;
        let (first_prefix, first_secret) = generator.generate();
        let (second_prefix, second_secret) = generator.generate();

        assert_eq!(first_prefix.as_str().len(), ApiKeyPrefix::LEN);
        assert_ne!(first_secret, second_secret);
        // Prefixes come from the random tail of a ULID, so two minted in the same
        // millisecond do not collide.
        assert_ne!(first_prefix, second_prefix);
    }
}
