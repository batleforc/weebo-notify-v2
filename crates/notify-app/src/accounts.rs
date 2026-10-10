//! Service accounts (§5.2bis, §8): a team bot never runs on a person's key. Managed
//! accounts belong to the configuration (§11, read-only here); dynamic ones are
//! created by a platform admin (§5.4), who may deactivate them.

use std::sync::Arc;

use notify_domain::{DomainError, Origin, Principal, ServiceAccount, ServiceAccountId, User};
use notify_ports::{
    ApiKeyRepository, AuditEntry, AuditLog, Clock, Failure, ServiceAccountRepository,
    UserRepository, actions,
};

use crate::authz::Caller;

/// `GET /api/v1/serviceaccounts` (design §8).
#[derive(Clone)]
pub struct ListServiceAccounts {
    accounts: Arc<dyn ServiceAccountRepository>,
}

impl core::fmt::Debug for ListServiceAccounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListServiceAccounts")
            .finish_non_exhaustive()
    }
}

impl ListServiceAccounts {
    #[must_use]
    pub fn new(accounts: Arc<dyn ServiceAccountRepository>) -> Self {
        Self { accounts }
    }

    /// Every service account, deactivated ones included. Unfiltered: a service account
    /// is a public identity (its name authors revisions), and carries no secret.
    pub async fn execute(&self, _caller: &Caller) -> Result<Vec<ServiceAccount>, Failure> {
        Ok(self.accounts.list().await?)
    }
}

/// `GET /api/v1/users?q=` — the member picker. People only: a bot has no use for a directory.
#[derive(Clone)]
pub struct SearchUsers {
    users: Arc<dyn UserRepository>,
}

impl core::fmt::Debug for SearchUsers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SearchUsers").finish_non_exhaustive()
    }
}

impl SearchUsers {
    /// At most this many matches: a picker, not an export.
    pub const LIMIT: u32 = 10;

    #[must_use]
    pub fn new(users: Arc<dyn UserRepository>) -> Self {
        Self { users }
    }

    /// Users whose name or email contains `text`; fewer than two characters matches nobody.
    pub async fn execute(&self, caller: &Caller, text: &str) -> Result<Vec<User>, Failure> {
        if !matches!(caller.principal, Principal::User { .. }) {
            return Err(Failure::Domain(DomainError::NotAPerson));
        }
        let text = text.trim();
        if text.chars().count() < 2 {
            return Ok(Vec::new());
        }
        Ok(self.users.lookup(text, Self::LIMIT).await?)
    }
}

/// `POST /api/v1/serviceaccounts` (design §8, §5.2bis).
#[derive(Clone)]
pub struct CreateServiceAccount {
    accounts: Arc<dyn ServiceAccountRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for CreateServiceAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CreateServiceAccount")
            .finish_non_exhaustive()
    }
}

impl CreateServiceAccount {
    #[must_use]
    pub fn new(
        accounts: Arc<dyn ServiceAccountRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            accounts,
            audit,
            clock,
        }
    }

    /// Create a dynamic account owned by the caller.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: &str,
        display_name: Option<&str>,
        description: Option<&str>,
    ) -> Result<ServiceAccount, Failure> {
        // A person, not a bot: the "who asked for this" chain must end at somebody.
        let Principal::User { id: owner } = caller.principal else {
            return Err(Failure::Domain(DomainError::NotAPerson));
        };
        // Minting an instance-wide author is the platform role's call (§5.4).
        caller.ensure_platform_admin()?;

        let id = ServiceAccountId::parse(id)?;
        if self.accounts.get(&id).await?.is_some() {
            // Names are the identity (§5.2bis): returning the existing one would let somebody adopt it.
            return Err(Failure::Domain(DomainError::Malformed {
                field: "service_account.id",
                reason: "already taken",
            }));
        }

        let now = self.clock.now();
        let account = ServiceAccount::new(
            id,
            display_name,
            description,
            Origin::Dynamic,
            Some(owner),
            now,
        )?;
        self.accounts.upsert(&account).await?;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::SERVICE_ACCOUNT_CREATED,
                target: Some(account.id.as_str().to_owned()),
                channel: None,
                detail: None,
                at: now,
            })
            .await?;

        Ok(account)
    }
}

/// `DELETE /api/v1/serviceaccounts/{id}` (§8, §5.2bis, T25): deactivation, not
/// deletion, with its keys revoked in the same call.
#[derive(Clone)]
pub struct DeactivateServiceAccount {
    accounts: Arc<dyn ServiceAccountRepository>,
    keys: Arc<dyn ApiKeyRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for DeactivateServiceAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeactivateServiceAccount")
            .finish_non_exhaustive()
    }
}

impl DeactivateServiceAccount {
    #[must_use]
    pub fn new(
        accounts: Arc<dyn ServiceAccountRepository>,
        keys: Arc<dyn ApiKeyRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            accounts,
            keys,
            audit,
            clock,
        }
    }

    /// Deactivate it and revoke its keys.
    ///
    /// A person's session only: deactivation revokes every key the account holds,
    /// and a leaked key of its creator must not be a way to switch a pipeline off.
    pub async fn execute(&self, caller: &Caller, id: &ServiceAccountId) -> Result<(), Failure> {
        let user = caller.ensure_person()?;

        let Some(mut account) = self.accounts.get(id).await? else {
            // Already the desired state; "no such account" would enumerate names.
            return Ok(());
        };

        account.ensure_may_disable(user)?;

        let now = self.clock.now();
        account.disable(now);
        self.accounts.upsert(&account).await?;

        // Revoke the keys too, so no credential stays usable.
        let revoked = self.keys.revoke_all_for(&account.principal(), now).await?;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::SERVICE_ACCOUNT_DISABLED,
                target: Some(account.id.as_str().to_owned()),
                channel: None,
                // How many credentials stopped working.
                detail: serde_json::json!({ "keys_revoked": revoked }).into(),
                at: now,
            })
            .await?;

        tracing::info!(
            account = account.id.as_str(),
            revoked,
            "service account deactivated"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, Scope, UserId};
    use notify_ports::StoredApiKey;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        list: ListServiceAccounts,
        create: CreateServiceAccount,
        deactivate: DeactivateServiceAccount,
        alice: UserId,
        bob: UserId,
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();

        Fixture {
            list: ListServiceAccounts::new(Arc::new(store.clone())),
            create: CreateServiceAccount::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            deactivate: DeactivateServiceAccount::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            store,
            clock,
            alice: UserId::generate(),
            bob: UserId::generate(),
        }
    }

    fn caller(id: UserId) -> Caller {
        Caller::member(Principal::User { id })
    }

    fn admin(id: UserId) -> Caller {
        Caller {
            platform_admin: true,
            ..caller(id)
        }
    }

    #[tokio::test]
    async fn only_a_platform_admin_creates_one() {
        let fixture = fixture();

        let refused = fixture
            .create
            .execute(&caller(fixture.alice), "bot-argocd", None, None)
            .await;

        assert!(matches!(
            refused,
            Err(Failure::Domain(DomainError::NotPlatformAdmin))
        ));
        assert_eq!(
            fixture.store.list().await.unwrap(),
            [] as [notify_domain::ServiceAccount; 0]
        );
    }

    #[tokio::test]
    async fn creating_one_makes_the_creator_its_owner() {
        let fixture = fixture();

        let account = fixture
            .create
            .execute(&admin(fixture.alice), "bot-argocd", Some("ArgoCD"), None)
            .await
            .unwrap();

        assert_eq!(account.origin, Origin::Dynamic);
        assert_eq!(account.created_by, Some(fixture.alice));
        assert_eq!(account.display_name.as_deref(), Some("ArgoCD"));
    }

    #[tokio::test]
    async fn a_name_that_is_taken_is_refused_rather_than_adopted() {
        let fixture = fixture();
        fixture
            .create
            .execute(&admin(fixture.alice), "bot-argocd", None, None)
            .await
            .unwrap();

        let refused = fixture
            .create
            .execute(&admin(fixture.bob), "bot-argocd", None, None)
            .await;

        assert!(
            refused.is_err(),
            "returning the existing one would let bob adopt alice's bot"
        );
    }

    #[tokio::test]
    async fn a_service_account_cannot_create_service_accounts() {
        let fixture = fixture();
        let bot = Caller::member(Principal::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        });

        assert!(
            fixture
                .create
                .execute(&bot, "bot-sneaky", None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn critical_t25_deactivating_revokes_its_keys_in_the_same_call() {
        let fixture = fixture();
        let account = fixture
            .create
            .execute(&admin(fixture.alice), "bot-argocd", None, None)
            .await
            .unwrap();

        let owner = ApiKeyOwner::ServiceAccount {
            id: account.id.clone(),
        };
        fixture.store.seed_api_key(StoredApiKey {
            key: ApiKey {
                id: ApiKeyId::generate(),
                label: Some("ci".to_owned()),
                prefix: ApiKeyPrefix::parse("wnk1abcd").unwrap(),
                owner: owner.clone(),
                scopes: vec![Scope::Publish],
                channels: Vec::new(),
                publish_per_minute: None,
                created_at: fixture.clock.now(),
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            secret_hash: "stub$secret".to_owned(),
        });

        fixture
            .deactivate
            .execute(&caller(fixture.alice), &account.id)
            .await
            .unwrap();

        let stored = ServiceAccountRepository::get(&fixture.store, &account.id)
            .await
            .unwrap()
            .unwrap();
        assert!(stored.disabled_at.is_some());

        let keys = ApiKeyRepository::list_for(&fixture.store, &account.principal())
            .await
            .unwrap();
        assert!(
            keys.iter().all(|key| key.revoked_at.is_some()),
            "a key left usable is a credential still sitting in somebody's CI config"
        );
    }

    #[tokio::test]
    async fn somebody_elses_bot_is_not_yours_to_turn_off() {
        let fixture = fixture();
        let account = fixture
            .create
            .execute(&admin(fixture.alice), "bot-argocd", None, None)
            .await
            .unwrap();

        let refused = fixture
            .deactivate
            .execute(&caller(fixture.bob), &account.id)
            .await;

        assert!(refused.is_err());
    }

    #[tokio::test]
    async fn a_managed_account_refuses_the_api_whoever_asks() {
        // §11: the configuration is its source of truth.
        let fixture = fixture();
        let id = ServiceAccountId::parse("bot-managed").unwrap();
        let managed = ServiceAccount::new(
            id.clone(),
            None,
            None,
            Origin::Managed,
            None,
            fixture.clock.now(),
        )
        .unwrap();
        ServiceAccountRepository::upsert(&fixture.store, &managed)
            .await
            .unwrap();

        assert!(
            fixture
                .deactivate
                .execute(&caller(fixture.alice), &id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deactivating_one_that_is_not_there_says_nothing_about_which_names_exist() {
        let fixture = fixture();

        fixture
            .deactivate
            .execute(
                &caller(fixture.alice),
                &ServiceAccountId::parse("bot-nothing").unwrap(),
            )
            .await
            .expect("the desired state is already true");
    }

    #[tokio::test]
    async fn the_listing_shows_every_account_and_no_secret() {
        let fixture = fixture();
        fixture
            .create
            .execute(&admin(fixture.alice), "bot-argocd", Some("ArgoCD"), None)
            .await
            .unwrap();

        // A service account is a public identity on this instance.
        let listed = fixture.list.execute(&caller(fixture.bob)).await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].display_name.as_deref(), Some("ArgoCD"));
    }

    #[tokio::test]
    async fn people_find_people_and_bots_are_refused() {
        let f = fixture();
        let now = time::OffsetDateTime::now_utc();
        let alice = User::from_claims(
            f.alice,
            "alice",
            Some("alice@weebo.poc"),
            Some("Alice Martin"),
            now,
        )
        .unwrap();
        UserRepository::upsert(&f.store, &alice).await.unwrap();
        let search = SearchUsers::new(Arc::new(f.store.clone()));

        let found = search.execute(&caller(f.bob), "martin").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, f.alice);
        assert!(
            search
                .execute(&caller(f.bob), "m")
                .await
                .unwrap()
                .is_empty(),
            "two characters minimum"
        );

        let bot = Caller::member(Principal::ServiceAccount {
            id: ServiceAccountId::parse("bot-argocd").unwrap(),
        });
        assert!(search.execute(&bot, "martin").await.is_err());
    }
}
