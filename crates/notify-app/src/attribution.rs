//! Putting a name on a principal (design §2.4).
//!
//! # What was wrong, and how long it took to see
//!
//! §2.4 ends on a sentence that reads like a summary and is in fact a
//! requirement: « le profil s'affiche **partout** où un principal apparaît —
//! `author` des révisions et interactions (timeline §10.3, SSE §8.1), "vu par
//! 3/5", liste des membres ». Of those three, exactly one was ever wired.
//! [`Author::named`] had a single production caller in this whole workspace —
//! the receipts roll in [`crate::interact`] — and every revision and every
//! interaction was built with [`Author::anonymous`].
//!
//! The consequence is not subtle once anybody looks: an inbox row, a timeline
//! entry and a live toast all rendered `usr_01KZ3RCPRRTH2J18W5WSN2AVGT` where
//! they were supposed to render a person. It survived because nothing about it
//! is *broken* — the clients have a documented fallback to the reference for
//! principals that have no profile (an anonymous channel key), and that fallback
//! is exactly what a named user got. A test asserting "the row shows the
//! author's label" passes on a ULID. It took rendering the page in a browser to
//! notice, which is the debt this increment exists to pay.
//!
//! # Naming at write time, not at read time
//!
//! Two places could do this, and only one of them can be right for both readers
//! of the same revision:
//!
//!   * **at read** — what the receipts roll does. It always shows the current
//!     name, at the cost of one lookup per row per page.
//!   * **at write** — one lookup per publish, and the name is frozen into the
//!     revision.
//!
//! It is written here, and the deciding argument is not the cost. A revision
//! reaches a client **twice**: once as an SSE event emitted by the publish that
//! created it (§8.1), and once as a row read back later. Both render the author.
//! Naming at read time names only the second, so a row that arrived live would
//! show `usr_01K…` and the very same row after a reload would show
//! `Alice Martin`. That inconsistency is the kind a user reports as a bug, and no
//! amount of hydration on the read path removes it — the event has already left.
//! Naming at write time makes the two identical by construction, because they
//! are the same value.
//!
//! What that costs, stated rather than discovered: **a rename does not travel
//! backwards**. A revision published by "Alice Martin" still says so after she
//! becomes "alice". For a timeline — a log of what happened, in order — that is
//! defensible and arguably right; for the receipts roll, which answers "who has
//! read this *now*", it is not, and that is why the roll keeps hydrating at read
//! and this module does not touch it. Two different questions, two answers.
//!
//! # Two of the four kinds have a profile
//!
//! Users and service accounts do (§2.4: « les service accounts portent le même
//! profil minimal »). A channel key and a CI job do not, and it is not an
//! oversight in either case: §5.2 says a key publish is attributed to the key
//! itself — « when attribution matters, reach for a service account » — and a CI
//! job *is* its subject claim (§5.3). Both stay anonymous here, and the clients
//! already word them (`clé key_… du canal ci-infra`, `repo:weebo/infra:ref:…`)
//! rather than falling through to a bare id.

use std::sync::Arc;

use notify_domain::{Author, Principal};
use notify_ports::{ServiceAccountRepository, UserRepository};

/// The two repositories that hold a profile, for the use cases that attribute a
/// write.
///
/// A struct rather than two `Arc` fields repeated in every writer: three use
/// cases need exactly this pair, and a fourth will need it the day the reconciler
/// starts auditing (D28).
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
    /// Wire it.
    #[must_use]
    pub const fn new(
        users: Arc<dyn UserRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
    ) -> Self {
        Self { users, accounts }
    }

    /// The author to record for a principal that is about to write.
    ///
    /// Never fails the write. A profile that cannot be read, a user deleted
    /// between authentication and publish, or a stored name this build's
    /// validation would refuse all land on the same answer — the anonymous
    /// author, which every client renders as the reference. Refusing a publish
    /// because a display name could not be fetched would trade a cosmetic
    /// degradation for a lost notification, and §7 spends its whole length
    /// arguing the other way.
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
            // No profile exists for these two, by design rather than by omission
            // — see the module header.
            Principal::ChannelKey { .. } | Principal::CiJob { .. } => None,
        };

        name.and_then(|name| Author::named(principal.clone(), &name).ok())
            .unwrap_or_else(|| Author::anonymous(principal.clone()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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
        // The state every row of this application was in until now: the label a
        // client shows is the ULID. Kept as a test because it is still the right
        // answer for a user whose provider released no `name` claim.
        let (store, under_test) = fixture();
        let id = user_named(&store, None).await;

        let author = under_test.author(&Principal::User { id }).await;

        assert_eq!(author.display_name, None);
        assert_eq!(author.label(), id.to_string());
    }

    #[tokio::test]
    async fn a_service_account_publishes_under_its_name() {
        // §2.4: « un bot avec une icône reconnaissable dans la timeline vaut mieux
        // qu'un id brut ».
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
        // Deleted between authentication and publish. The notification is worth
        // more than the name on it.
        let (_, under_test) = fixture();

        let id = UserId::generate();
        let author = under_test.author(&Principal::User { id }).await;

        assert_eq!(author.label(), id.to_string());
    }
}
