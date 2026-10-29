//! Putting a name on a principal (§2.4). Named at write time, so the live SSE event
//! and the row read back later are the same value; a rename does not travel backwards
//! (the receipts roll names at read time instead). Channel keys and CI jobs stay anonymous.

use std::sync::Arc;

use notify_domain::{Author, Principal};
use notify_ports::{ServiceAccountRepository, UserRepository};

/// The two repositories that hold a profile, for the use cases that attribute a write.
#[derive(Clone)]
pub struct Attribution {
    users: Arc<dyn UserRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
}

impl core::fmt::Debug for Attribution {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attribution").finish_non_exhaustive()
    }
}

impl Attribution {
    #[must_use]
    pub const fn new(
        users: Arc<dyn UserRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
    ) -> Self {
        Self { users, accounts }
    }

    /// The author to record for a principal that is about to write. Never fails the
    /// write: any lookup problem yields the anonymous author.
    pub async fn author(&self, principal: &Principal) -> Author {
        let name = match principal {
            Principal::User { id } => match self.users.get(*id).await {
                Ok(Some(user)) => user.display_name,
                _ => None,
            },
            Principal::ServiceAccount { id } => match self.accounts.get(id).await {
                Ok(Some(account)) => account.display_name,
                _ => None,
            },
            // No profile exists for these, by design.
            Principal::ChannelKey { .. }
            | Principal::CiJob { .. }
            | Principal::KeyIssuer { .. } => None,
        };

        name.and_then(|name| Author::named(principal.clone(), &name).ok())
            .unwrap_or_else(|| Author::anonymous(principal.clone()))
    }
}

#[cfg(test)]
mod tests {
    use notify_domain::{Origin, ServiceAccount, ServiceAccountId, User, UserId};
    use notify_ports::test_utils::InMemory;
    use time::OffsetDateTime;

    use super::*;

    fn fixture() -> (InMemory, Attribution) {
        let store = InMemory::new();
        let under_test = Attribution::new(Arc::new(store.clone()), Arc::new(store.clone()));
        (store, under_test)
    }

    async fn user_named(store: &InMemory, name: Option<&str>) -> UserId {
        let id = UserId::generate();
        let user = User::from_claims(
            id,
            &format!("subject-{id}"),
            None,
            name,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        UserRepository::upsert(store, &user).await.unwrap();
        id
    }

    #[tokio::test]
    async fn a_user_with_a_profile_publishes_under_their_name() {
        let (store, under_test) = fixture();
        let id = user_named(&store, Some("Alice Martin")).await;

        let author = under_test.author(&Principal::User { id }).await;

        assert_eq!(author.display_name.as_deref(), Some("Alice Martin"));
        assert_eq!(author.label(), "Alice Martin");
    }

    #[tokio::test]
    async fn a_user_without_one_falls_back_to_the_reference() {
        // A user whose provider released no `name` claim is shown by reference.
        let (store, under_test) = fixture();
        let id = user_named(&store, None).await;

        let author = under_test.author(&Principal::User { id }).await;

        assert_eq!(author.display_name, None);
        assert_eq!(author.label(), id.to_string());
    }

    #[tokio::test]
    async fn a_service_account_publishes_under_its_name() {
        // §2.4: a bot with a recognisable name beats a raw id.
        let (store, under_test) = fixture();
        let id = ServiceAccountId::parse("bot-argocd").unwrap();
        let account = ServiceAccount::new(
            id.clone(),
            Some("ArgoCD"),
            None,
            Origin::Managed,
            None,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        ServiceAccountRepository::upsert(&store, &account)
            .await
            .unwrap();

        let author = under_test.author(&Principal::ServiceAccount { id }).await;

        assert_eq!(author.label(), "ArgoCD");
    }

    #[tokio::test]
    async fn a_principal_that_has_no_profile_is_left_alone() {
        let (_, under_test) = fixture();
        let subject = "repo:weebo/infra:ref:refs/heads/main";
        let principal =
            Principal::ci_job("https://forge.weebo.poc".parse().unwrap(), subject).unwrap();

        let author = under_test.author(&principal).await;

        assert_eq!(author.display_name, None);
        assert_eq!(author.label(), subject);
    }

    #[tokio::test]
    async fn a_user_who_no_longer_exists_does_not_fail_the_write() {
        // Deleted between authentication and publish: the notification matters more.
        let (_, under_test) = fixture();

        let id = UserId::generate();
        let author = under_test.author(&Principal::User { id }).await;

        assert_eq!(author.label(), id.to_string());
    }
}
