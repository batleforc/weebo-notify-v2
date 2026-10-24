//! Managing channels and their members. A GitOps-managed channel is read-only here
//! (§11): refuse rather than let the reconciler undo the write.

use std::sync::Arc;

use notify_domain::{Channel, ChannelId, DomainError, Membership, PolicyEdit, Principal, Scope};
use notify_ports::{
    AuditEntry, AuditLog, ChannelRepository, Clock, Failure, Page, PageRequest, actions,
};

use crate::authz::{Authorizer, Caller, Grant};

/// `GET /api/v1/channels` — the channels a caller can see (design §8).
#[derive(Clone)]
pub struct ListChannels {
    channels: Arc<dyn ChannelRepository>,
    authorizer: Authorizer,
}

impl core::fmt::Debug for ListChannels {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListChannels").finish_non_exhaustive()
    }
}

impl ListChannels {
    #[must_use]
    pub const fn new(channels: Arc<dyn ChannelRepository>, authorizer: Authorizer) -> Self {
        Self {
            channels,
            authorizer,
        }
    }

    /// List the channels the caller may read.
    pub async fn execute(
        &self,
        caller: &Caller,
        page: PageRequest,
    ) -> Result<Page<Channel>, Failure> {
        let mut listed = self
            .channels
            .list_for(&caller.principal, &caller.groups, Scope::Subscribe, page)
            .await?;
        // A key or CI token acts as its owner here, but reads only what its own grant
        // covers: otherwise a publish-only key lists every member of every owner channel.
        if caller.grant != Grant::Membership {
            let readable = self.authorizer.readable_channels(caller).await?;
            listed
                .items
                .retain(|channel| readable.contains(&channel.id));
        }
        Ok(listed)
    }
}

/// Wake `member`'s live streams so they re-read what they may read: a membership writes
/// no event, so no channel poke would reach a stream that does not know the channel yet.
/// No event behind it either, hence the start cursor; streams wake on it regardless.
pub(crate) async fn announce_membership(
    bus: Option<&dyn notify_ports::EventBus>,
    member: &Principal,
) {
    let (Some(bus), Principal::User { id }) = (bus, member) else {
        return;
    };
    let poke = notify_ports::Poke::User {
        id: *id,
        cursor: notify_domain::Cursor::START,
    };
    if let Err(error) = bus.publish(&poke).await {
        tracing::warn!(
            ?error,
            "could not tell a new member's streams; they catch up within a minute"
        );
    }
}

/// `GET /api/v1/channels/{channel}` — one channel, by id (design §8).
#[derive(Clone)]
pub struct ReadChannel {
    authorizer: Authorizer,
}

impl core::fmt::Debug for ReadChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadChannel").finish_non_exhaustive()
    }
}

impl ReadChannel {
    #[must_use]
    pub const fn new(authorizer: Authorizer) -> Self {
        Self { authorizer }
    }

    /// One channel, if the caller may see it: `Subscribe`, the read [`ListChannels`]
    /// already grants. Missing and forbidden answer alike.
    pub async fn execute(
        &self,
        caller: &Caller,
        channel_id: &ChannelId,
    ) -> Result<Channel, Failure> {
        self.authorizer
            .authorize(caller, channel_id, Scope::Subscribe)
            .await
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

    /// Create it, with the caller as its first manager (otherwise nobody could administer it).
    pub async fn execute(
        &self,
        caller: &Caller,
        id: ChannelId,
        display_name: Option<&str>,
    ) -> Result<Channel, Failure> {
        // A person's session only: a key creating channels would grant itself footholds.
        caller.ensure_person()?;
        let mut channel = Channel::dynamic(id, display_name)?;
        channel.upsert_member(Membership::new(
            caller.principal.clone(),
            vec![Scope::Publish, Scope::Subscribe, Scope::Manage],
        )?);

        // Insert-if-absent in one write, so two creators of one name cannot both win.
        match self.channels.insert(&channel).await {
            Ok(()) => {}
            Err(notify_ports::PortError::Conflict { .. }) => {
                return Err(DomainError::Malformed {
                    field: "channel.id",
                    reason: "is already taken",
                }
                .into());
            }
            Err(other) => return Err(other.into()),
        }
        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::CHANNEL_CREATED,
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
    /// Tells a new member's open streams to re-read what they may read.
    bus: Option<Arc<dyn notify_ports::EventBus>>,
}

impl core::fmt::Debug for ManageMembers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ManageMembers").finish_non_exhaustive()
    }
}

impl ManageMembers {
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
            bus: None,
        }
    }

    /// Announce memberships to the member's live streams, which otherwise learn of a
    /// joined channel only when they next re-authenticate.
    #[must_use]
    pub fn with_bus(mut self, bus: Arc<dyn notify_ports::EventBus>) -> Self {
        self.bus = Some(bus);
        self
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
        // Config owns a managed channel's access list (§11).
        channel
            .origin
            .ensure_mutable_through_api("channel.members")?;

        let membership = Membership::new(principal.clone(), scopes.clone())?;
        // One row, so a concurrent manager's addition is not dropped.
        self.channels.put_member(channel_id, &membership).await?;
        channel.upsert_member(membership);
        announce_membership(self.bus.as_deref(), &principal).await;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::CHANNEL_MEMBER_ADDED,
                target: Some(principal.reference()),
                channel: Some(channel_id.clone()),
                detail: serde_json::to_value(&scopes).ok(),
                at: self.clock.now(),
            })
            .await?;
        Ok(channel)
    }

    /// Remove a member, or leave; only removing somebody else needs `manage`.
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

        let channel = self.authorizer.authorize(caller, channel_id, scope).await?;
        channel
            .origin
            .ensure_mutable_through_api("channel.members")?;

        // Asked of the store, so only one of two concurrent removals is journalled.
        let removed = self.channels.remove_member(channel_id, principal).await?;
        if removed {
            self.audit
                .record(&AuditEntry {
                    actor: caller.principal.clone().into(),
                    action: if leaving {
                        actions::CHANNEL_MEMBER_LEFT
                    } else {
                        actions::CHANNEL_MEMBER_REMOVED
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

/// `PATCH /api/v1/channels/{id}` — the channel's policy (§8).
#[derive(Clone)]
pub struct UpdateChannel {
    channels: Arc<dyn ChannelRepository>,
    authorizer: Authorizer,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for UpdateChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UpdateChannel").finish_non_exhaustive()
    }
}

impl UpdateChannel {
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

    /// Change part of a channel's policy. Managed channels are refused (§8, §11).
    pub async fn execute(
        &self,
        caller: &Caller,
        channel_id: &ChannelId,
        edit: PolicyEdit,
    ) -> Result<Channel, Failure> {
        let mut channel = self
            .authorizer
            .authorize(caller, channel_id, Scope::Manage)
            .await?;
        // Config owns a managed channel's policy (§11).
        channel
            .origin
            .ensure_mutable_through_api("channel.policy")?;

        // The domain refuses an incoherent edit before storage.
        if !channel.edit_policy(edit)? {
            // Nothing moved: idempotent answer, no write, no audit entry.
            return Ok(channel);
        }

        // The policy alone, so a stale member list is not written back.
        self.channels
            .set_policy(channel_id, &channel.policy)
            .await?;
        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::CHANNEL_POLICY_UPDATED,
                target: Some(channel_id.to_string()),
                channel: Some(channel_id.clone()),
                // The resulting policy, not the partial request body.
                detail: serde_json::to_value(channel.policy).ok(),
                at: self.clock.now(),
            })
            .await?;

        Ok(channel)
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
                    actor: caller.principal.clone().into(),
                    action: actions::CHANNEL_DELETED,
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
    use notify_domain::{Origin, UserId};
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    struct Fixture {
        store: InMemory,
        list: ListChannels,
        create: CreateChannel,
        members: ManageMembers,
        update: UpdateChannel,
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
            list: ListChannels::new(
                Arc::new(store.clone()),
                Authorizer::new(Arc::new(store.clone())),
            ),
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
            )
            .with_bus(Arc::new(store.clone())),
            update: UpdateChannel::new(
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
    async fn only_a_persons_session_creates_a_channel() {
        let fixture = fixture();
        let as_key = Caller {
            grant: crate::authz::Grant::ApiKey {
                id: notify_domain::ApiKeyId::generate(),
                scopes: vec![Scope::Publish, Scope::Manage],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            ..Caller::member(fixture.alice.clone())
        };
        let as_bot = Caller::member(Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        });

        for caller in [as_key, as_bot] {
            assert!(
                fixture
                    .create
                    .execute(&caller, channel_id("foothold"), None)
                    .await
                    .is_err()
            );
        }
        assert!(
            ChannelRepository::get(&fixture.store, &channel_id("foothold"))
                .await
                .unwrap()
                .is_none()
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
                .any(|entry| entry.action.as_str() == "channel.member_added")
        ); // Bob's open streams are told, so the channel reaches them without waiting.
        let bob_id = bob.as_user().expect("built as a user above");
        assert!(
            fixture
                .store
                .pokes()
                .iter()
                .any(|poke| matches!(poke, notify_ports::Poke::User { id, .. } if *id == bob_id))
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
        assert!(
            fixture
                .update
                .execute(
                    &caller,
                    &channel_id("cluster-ovh-1"),
                    PolicyEdit {
                        receipts_enabled: Some(true),
                        ..PolicyEdit::default()
                    },
                )
                .await
                .is_err(),
            "a policy is config's to set on a managed channel too"
        );
    }

    #[tokio::test]
    async fn updating_a_policy_needs_manage_and_is_audited() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .create
            .execute(&caller, channel_id("ci-infra"), None)
            .await
            .unwrap();

        let updated = fixture
            .update
            .execute(
                &caller,
                &channel_id("ci-infra"),
                PolicyEdit {
                    receipts_enabled: Some(true),
                    ..PolicyEdit::default()
                },
            )
            .await
            .unwrap();

        assert!(updated.policy.receipts_enabled);
        assert!(
            fixture
                .store
                .audit_entries()
                .iter()
                .any(|entry| entry.action.as_str() == "channel.policy_updated")
        );

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
                .update
                .execute(
                    &Caller::member(bob),
                    &channel_id("ci-infra"),
                    PolicyEdit {
                        receipts_enabled: Some(false),
                        ..PolicyEdit::default()
                    },
                )
                .await
                .is_err(),
            "a subscriber must not be able to turn receipts off for everybody"
        );
    }

    #[tokio::test]
    async fn a_patch_that_changes_nothing_writes_nothing() {
        let fixture = fixture();
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .create
            .execute(&caller, channel_id("ci-infra"), None)
            .await
            .unwrap();

        fixture
            .update
            .execute(&caller, &channel_id("ci-infra"), PolicyEdit::default())
            .await
            .unwrap();

        assert!(
            !fixture
                .store
                .audit_entries()
                .iter()
                .any(|entry| entry.action.as_str() == "channel.policy_updated"),
            "an audit entry for a policy that did not move is a false record"
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
                .any(|entry| entry.action.as_str() == "channel.deleted")
        );
    }
}
