//! Managing channels: creating them, and who is in them.
//!
//! A GitOps-managed channel is read-only through the API (design §11). Refusing
//! here rather than letting the write through and having the reconciler undo it is
//! the difference between an error message and an afternoon spent watching a
//! channel resurrect.

use std::sync::Arc;

use notify_domain::{Channel, ChannelId, DomainError, Membership, Principal, Scope};
use notify_ports::{AuditEntry, AuditLog, ChannelRepository, Clock, Failure, Page, PageRequest};

use crate::authz::{Authorizer, Caller};

/// `GET /api/v1/channels` — the channels a caller can see (design §8).
#[derive(Clone)]
pub struct ListChannels {
    channels: Arc<dyn ChannelRepository>,
}

impl core::fmt::Debug for ListChannels {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListChannels").finish_non_exhaustive()
    }
}

impl ListChannels {
    /// Wire it.
    #[must_use]
    pub fn new(channels: Arc<dyn ChannelRepository>) -> Self {
        Self { channels }
    }

    /// List the channels the caller may read.
    pub async fn execute(
        &self,
        caller: &Caller,
        page: PageRequest,
    ) -> Result<Page<Channel>, Failure> {
        Ok(self
            .channels
            .list_for(&caller.principal, &caller.groups, Scope::Subscribe, page)
            .await?)
    }
}

/// `POST /api/v1/channels` — create a dynamic channel (design §8).
#[derive(Clone)]
pub struct CreateChannel {
    channels: Arc<dyn ChannelRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for CreateChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CreateChannel").finish_non_exhaustive()
    }
}

impl CreateChannel {
    /// Wire it.
    #[must_use]
    pub fn new(
        channels: Arc<dyn ChannelRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            channels,
            audit,
            clock,
        }
    }

    /// Create it, with the caller as its first manager.
    ///
    /// Making the creator a manager is not a courtesy: a channel nobody can
    /// administer needs an operator with database access to fix.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: ChannelId,
        display_name: Option<&str>,
    ) -> Result<Channel, Failure> {
        if self.channels.get(&id).await?.is_some() {
            return Err(DomainError::Malformed {
                field: "channel.id",
                reason: "is already taken",
            }
            .into());
        }

        let mut channel = Channel::dynamic(id, display_name)?;
        channel.upsert_member(Membership::new(
            caller.principal.clone(),
            vec![Scope::Publish, Scope::Subscribe, Scope::Manage],
        )?);

        self.channels.upsert(&channel).await?;
        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone(),
                action: "channel.created",
                target: Some(channel.id.to_string()),
                channel: Some(channel.id.clone()),
                detail: None,
                at: self.clock.now(),
            })
            .await?;

        Ok(channel)
    }
}

/// `POST`/`DELETE /api/v1/channels/{id}/members` — membership (design §8).
#[derive(Clone)]
pub struct ManageMembers {
    channels: Arc<dyn ChannelRepository>,
    authorizer: Authorizer,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for ManageMembers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ManageMembers").finish_non_exhaustive()
    }
}

impl ManageMembers {
    /// Wire it.
    #[must_use]
    pub fn new(
        channels: Arc<dyn ChannelRepository>,
        authorizer: Authorizer,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            channels,
            authorizer,
            audit,
            clock,
        }
    }

    /// Add a member or change their scopes.
    pub async fn add(
        &self,
        caller: &Caller,
        channel_id: &ChannelId,
        principal: Principal,
        scopes: Vec<Scope>,
    ) -> Result<Channel, Failure> {
        let mut channel = self
            .authorizer
            .authorize(caller, channel_id, Scope::Manage)
            .await?;
        // Config owns a managed channel's access list; writing here would be undone
        // at the next reconciliation (design §11).
        channel
            .origin
            .ensure_mutable_through_api("channel.members")?;

        channel.upsert_member(Membership::new(principal.clone(), scopes.clone())?);
        self.channels.upsert(&channel).await?;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone(),
                action: "channel.member_added",
                target: Some(principal.reference()),
                channel: Some(channel_id.clone()),
                detail: serde_json::to_value(&scopes).ok(),
                at: self.clock.now(),
            })
            .await?;
        Ok(channel)
    }

    /// Remove a member, or leave.
    ///
    /// Leaving needs no `manage`: you may always take yourself out of a channel.
    /// Removing somebody else does.
    pub async fn remove(
        &self,
        caller: &Caller,
        channel_id: &ChannelId,
        principal: &Principal,
    ) -> Result<bool, Failure> {
        let leaving = *principal == caller.principal;
        let scope = if leaving {
            Scope::Subscribe
        } else {
            Scope::Manage
        };

        let mut channel = self.authorizer.authorize(caller, channel_id, scope).await?;
        channel
            .origin
            .ensure_mutable_through_api("channel.members")?;

        let removed = channel.remove_member(principal);
        if removed {
            self.channels.upsert(&channel).await?;
            self.audit
                .record(&AuditEntry {
                    actor: caller.principal.clone(),
                    action: if leaving {
                        "channel.member_left"
                    } else {
                        "channel.member_removed"
                    },
                    target: Some(principal.reference()),
                    channel: Some(channel_id.clone()),
                    detail: None,
                    at: self.clock.now(),
                })
                .await?;
        }
        Ok(removed)
    }
}

/// `DELETE /api/v1/channels/{id}` (design §8).
#[derive(Clone)]
pub struct DeleteChannel {
    channels: Arc<dyn ChannelRepository>,
    authorizer: Authorizer,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for DeleteChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeleteChannel").finish_non_exhaustive()
    }
}

impl DeleteChannel {
    /// Wire it.
    #[must_use]
    pub fn new(
        channels: Arc<dyn ChannelRepository>,
        authorizer: Authorizer,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            channels,
            authorizer,
            audit,
            clock,
        }
    }

    /// Delete a dynamic channel. Managed ones are refused (design §8, §11).
    pub async fn execute(&self, caller: &Caller, channel_id: &ChannelId) -> Result<bool, Failure> {
        let channel = self
            .authorizer
            .authorize(caller, channel_id, Scope::Manage)
            .await?;
        channel.ensure_mutable_through_api()?;

        let deleted = self.channels.delete(channel_id).await?;
        if deleted {
            self.audit
                .record(&AuditEntry {
                    actor: caller.principal.clone(),
                    action: "channel.deleted",
                    target: Some(channel_id.to_string()),
                    channel: Some(channel_id.clone()),
                    detail: None,
                    at: self.clock.now(),
                })
                .await?;
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Origin, UserId};
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    struct Fixture {
        store: InMemory,
        list: ListChannels,
        create: CreateChannel,
        members: ManageMembers,
        delete: DeleteChannel,
        alice: Principal,
    }

    fn channel_id(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let authorizer = Authorizer::new(Arc::new(store.clone()));

        Fixture {
            list: ListChannels::new(Arc::new(store.clone())),
            create: CreateChannel::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            members: ManageMembers::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(store.clone()),
                Arc::new(clock.clone()),
            ),
            delete: DeleteChannel::new(
                Arc::new(store.clone()),
                authorizer,
                Arc::new(store.clone()),
                Arc::new(clock),
            ),
            store,
            alice: Principal::User {
                id: UserId::generate(),
            },
        }
    }

    #[tokio::test]
    async fn creating_a_channel_makes_the_creator_its_manager() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        let channel = fixture
            .create
            .execute(&caller, channel_id("ci-infra"), Some("CI infra"))
            .await
            .unwrap();

        assert!(
            channel.grants(&fixture.alice, Scope::Manage),
            "a channel nobody can administer needs database access to fix"
        );
        assert_eq!(
            fixture
                .list
                .execute(&caller, PageRequest::first())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_taken_channel_id_is_refused() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .create
            .execute(&caller, channel_id("ci-infra"), None)
            .await
            .unwrap();

        assert!(
            fixture
                .create
                .execute(&caller, channel_id("ci-infra"), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn adding_a_member_needs_manage_and_is_audited() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .create
            .execute(&caller, channel_id("ci-infra"), None)
            .await
            .unwrap();

        let bob = Principal::User {
            id: UserId::generate(),
        };
        fixture
            .members
            .add(
                &caller,
                &channel_id("ci-infra"),
                bob.clone(),
                vec![Scope::Subscribe],
            )
            .await
            .unwrap();

        assert!(
            fixture
                .members
                .add(
                    &Caller::member(bob.clone()),
                    &channel_id("ci-infra"),
                    Principal::User {
                        id: UserId::generate()
                    },
                    vec![Scope::Manage],
                )
                .await
                .is_err(),
            "a subscriber must not be able to grant themselves company"
        );

        let audit = fixture.store.audit_entries();
        assert!(
            audit
                .iter()
                .any(|entry| entry.action == "channel.member_added")
        );
    }

    #[tokio::test]
    async fn leaving_needs_no_manage_but_removing_somebody_else_does() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .create
            .execute(&caller, channel_id("ci-infra"), None)
            .await
            .unwrap();

        let bob = Principal::User {
            id: UserId::generate(),
        };
        fixture
            .members
            .add(
                &caller,
                &channel_id("ci-infra"),
                bob.clone(),
                vec![Scope::Subscribe],
            )
            .await
            .unwrap();

        // Bob cannot remove Alice…
        assert!(
            fixture
                .members
                .remove(
                    &Caller::member(bob.clone()),
                    &channel_id("ci-infra"),
                    &fixture.alice
                )
                .await
                .is_err()
        );
        // …but may always leave.
        assert!(
            fixture
                .members
                .remove(&Caller::member(bob.clone()), &channel_id("ci-infra"), &bob)
                .await
                .unwrap()
        );
    }

    // --- T21 · GitOps owns managed channels (design §3.4) -----------------

    #[tokio::test]
    async fn critical_t21_a_managed_channel_refuses_api_mutation() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());

        let mut managed = Channel::dynamic(channel_id("cluster-ovh-1"), None).unwrap();
        managed.origin = Origin::Managed;
        managed.upsert_member(
            Membership::new(fixture.alice.clone(), vec![Scope::Manage, Scope::Subscribe]).unwrap(),
        );
        fixture.store.seed_channel(managed);

        assert!(
            fixture
                .delete
                .execute(&caller, &channel_id("cluster-ovh-1"))
                .await
                .is_err(),
            "config would put it straight back"
        );
        assert!(
            fixture
                .members
                .add(
                    &caller,
                    &channel_id("cluster-ovh-1"),
                    Principal::User {
                        id: UserId::generate()
                    },
                    vec![Scope::Subscribe],
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deleting_a_dynamic_channel_works_and_is_audited() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .create
            .execute(&caller, channel_id("ci-infra"), None)
            .await
            .unwrap();

        assert!(
            fixture
                .delete
                .execute(&caller, &channel_id("ci-infra"))
                .await
                .unwrap()
        );
        assert!(
            fixture
                .store
                .audit_entries()
                .iter()
                .any(|entry| entry.action == "channel.deleted")
        );
    }
}
