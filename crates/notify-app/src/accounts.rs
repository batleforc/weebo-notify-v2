//! Service accounts (design §5.2bis, §8).
//!
//! # Why a bot gets an account of its own
//!
//! The rule §5.2 states and this module exists to make possible: *a team bot never
//! runs on a person's key*. Offboarding somebody must not take the deploy
//! notifications with them, and an audit log that says "published by a key of
//! alice" when ArgoCD published is a log that misleads whoever reads it in six
//! months.
//!
//! # Who administers one
//!
//! Two answers, and which one applies is decided by [`Origin`]:
//!
//! * **managed** — the configuration is the source of truth, so the API refuses to
//!   mutate it at all (§11). The same refusal `DELETE /channels/{id}` already gives:
//!   letting an API call win would only mean the next reconciliation undoing it.
//! * **dynamic** — created through the API, and the creator may deactivate it. That
//!   is the rule `CreateChannel` already follows for the same reason: an object
//!   nobody can administer needs somebody with database access to fix.
//!
//! The gap that leaves is the one §5.2bis names out loud: a bot whose creator has
//! left keeps *running* — its keys are its own — but has nobody left to turn it
//! off. GitOps is the answer for anything durable, and the platform role (§5.4)
//! closes it for the ones already created. Neither is M2.3, and pretending
//! otherwise by handing deactivation to everybody would be worse than the gap.

use std::sync::Arc;

use notify_domain::{DomainError, Origin, Principal, ServiceAccount, ServiceAccountId};
use notify_ports::{
    ApiKeyRepository, AuditEntry, AuditLog, Clock, Failure, ServiceAccountRepository, actions,
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
    /// Wire it.
    #[must_use]
    pub fn new(accounts: Arc<dyn ServiceAccountRepository>) -> Self {
        Self { accounts }
    }

    /// Every service account, deactivated ones included.
    ///
    /// Not filtered by who is asking, and that is not an oversight. A service
    /// account is a *public identity* on this instance: its name appears as the
    /// `author` of revisions and in audit entries (§2.4, §5.2bis), so the list adds
    /// nothing somebody could not already read from a timeline. What it carries is
    /// a profile and dates — never a key, never a secret.
    pub async fn execute(&self, _caller: &Caller) -> Result<Vec<ServiceAccount>, Failure> {
        Ok(self.accounts.list().await?)
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
    /// Wire it.
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
        // A person, not a bot. A service account minting service accounts would be
        // a privilege-escalation path with no audit trail worth the name: the chain
        // of "who asked for this" ends at somebody who can be asked.
        let Principal::User { id: owner } = caller.principal else {
            return Err(Failure::Domain(DomainError::NotAPerson));
        };

        let id = ServiceAccountId::parse(id)?;
        if self.accounts.get(&id).await?.is_some() {
            // Names are the identity here (§5.2bis) and they appear in the audit
            // log, so silently returning the existing one would let somebody adopt
            // a bot they did not create.
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

/// `DELETE /api/v1/serviceaccounts/{id}` (design §8, §5.2bis, scenario T25).
///
/// Deactivation, not deletion: the account keeps existing so the revisions and
/// audit entries naming it stay readable. What it loses is the ability to act, and
/// it loses it *immediately* — its keys are revoked in the same call rather than
/// left to expire.
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
    /// Wire it.
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
    pub async fn execute(&self, caller: &Caller, id: &ServiceAccountId) -> Result<(), Failure> {
        let Principal::User { id: user } = caller.principal else {
            return Err(Failure::Domain(DomainError::NotAPerson));
        };

        let Some(mut account) = self.accounts.get(id).await? else {
            // Deactivating something that is not there is the desired state, and
            // saying "no such account" would let anybody enumerate which names
            // exist on this instance.
            return Ok(());
        };

        account.ensure_may_disable(user)?;

        let now = self.clock.now();
        account.disable(now);
        self.accounts.upsert(&account).await?;

        // Keys first-class: the account row alone would stop *new* authentications
        // only because `Authenticate` checks it (T25), and a key left usable is a
        // credential still in somebody's CI configuration.
        let revoked = self.keys.revoke_all_for(&account.principal(), now).await?;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::SERVICE_ACCOUNT_DISABLED,
                target: Some(account.id.as_str().to_owned()),
                channel: None,
                // How many credentials stopped working, which is the number an
                // incident review actually wants from this line.
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

    #[tokio::test]
    async fn creating_one_makes_the_creator_its_owner() {
        let fixture = fixture();

        let account = fixture
            .create
            .execute(&caller(fixture.alice), "bot-argocd", Some("ArgoCD"), None)
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
            .execute(&caller(fixture.alice), "bot-argocd", None, None)
            .await
            .unwrap();

        let refused = fixture
            .create
            .execute(&caller(fixture.bob), "bot-argocd", None, None)
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
            .execute(&caller(fixture.alice), "bot-argocd", None, None)
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
            .execute(&caller(fixture.alice), "bot-argocd", None, None)
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
        // §11: the configuration is its source of truth, so an API deactivation
        // would only survive until the next reconciliation.
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
            .execute(&caller(fixture.alice), "bot-argocd", Some("ArgoCD"), None)
            .await
            .unwrap();

        // Bob sees it: a service account is a public identity on this instance, its
        // name already appears as the `author` of everything it published.
        let listed = fixture.list.execute(&caller(fixture.bob)).await.unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].display_name.as_deref(), Some("ArgoCD"));
    }
}
