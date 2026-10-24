//! The platform console's use cases (§5.4, §10.4bis): metadata and operations, never
//! content. Membership stays the one door to content; an admin's join is audited.
//! Every use case re-checks the role, so a second route table cannot skip it.

use std::sync::Arc;

use notify_domain::{ChannelId, Scope};
use notify_ports::{
    AuditEntry, AuditLog, AuditQuery, ChannelRepository, Failure, Page, PageRequest, actions,
};
use time::OffsetDateTime;

use crate::authz::Caller;

/// `GET /api/v1/admin/audit` — the after-incident tool (design §13, §10.4bis).
#[derive(Clone)]
pub struct BrowseAudit {
    audit: Arc<dyn AuditLog>,
}

impl core::fmt::Debug for BrowseAudit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BrowseAudit").finish_non_exhaustive()
    }
}

impl BrowseAudit {
    #[must_use]
    pub fn new(audit: Arc<dyn AuditLog>) -> Self {
        Self { audit }
    }

    /// Browse, filtered. The log records acts, not payloads, so it is inside the boundary.
    pub async fn execute(
        &self,
        caller: &Caller,
        query: AuditQuery,
    ) -> Result<Page<AuditEntry>, Failure> {
        caller.ensure_platform_admin()?;
        Ok(self.audit.query(&query).await?)
    }
}

/// `GET /api/v1/admin/channels` — the inventory (§10.4bis): record cards, never
/// notifications, which answer to membership only.
#[derive(Clone)]
pub struct ChannelInventory {
    channels: Arc<dyn ChannelRepository>,
}

impl core::fmt::Debug for ChannelInventory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChannelInventory").finish_non_exhaustive()
    }
}

impl ChannelInventory {
    #[must_use]
    pub fn new(channels: Arc<dyn ChannelRepository>) -> Self {
        Self { channels }
    }

    /// One page of the channels on the instance, as record cards.
    pub async fn execute(
        &self,
        caller: &Caller,
        page: PageRequest,
    ) -> Result<Page<ChannelCard>, Failure> {
        caller.ensure_platform_admin()?;

        let channels = self.channels.list_all(page).await?;
        let cards = channels
            .items
            .into_iter()
            .map(|channel| ChannelCard {
                id: channel.id,
                origin_managed: channel.origin.is_managed(),
                members: channel.members.len(),
                group_bindings: channel.group_access.len(),
                receipts_enabled: channel.policy.receipts_enabled,
            })
            .collect();
        Ok(match channels.next {
            Some(next) => Page::more(cards, next),
            None => Page::last(cards),
        })
    }
}

/// What the console shows for one channel; member counts, not names (§2.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChannelCard {
    /// Which channel.
    pub id: ChannelId,
    /// Whether GitOps owns it (§11) — the badge that makes the UI read-only.
    pub origin_managed: bool,
    /// How many members it has.
    pub members: usize,
    /// How many identity-provider groups are bound to it.
    pub group_bindings: usize,
    /// Whether it collects receipts (§7.3).
    pub receipts_enabled: bool,
}

/// What made a reconciliation pass happen (design §11, §10.4bis).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The process started.
    Boot,
    /// The periodic timer fired.
    Timer,
    /// An operator sent `SIGHUP` and the file was re-read.
    Sighup,
}

/// What the last GitOps pass did (design §11, §10.4bis).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Reconciliation {
    /// When this replica last applied the configuration.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// What caused this pass — boot, the timer, or `SIGHUP` (§11).
    pub trigger: Trigger,
    /// Managed channels written.
    pub channels: usize,
    /// Managed service accounts written.
    pub accounts: usize,
    /// Channels that left the configuration and were archived or deleted.
    pub removed: usize,
    /// Bindings by `oidc_sub` or `email` naming somebody who has never logged in, which
    /// grant nothing until that person's first login (§14.1).
    pub pending: Vec<String>,
}

/// The last reconciliation pass, kept in memory for the console. Every replica
/// reconciles, so this is the serving replica's answer, not the instance's.
#[derive(Clone, Default)]
pub struct ReconciliationLog {
    last: Arc<std::sync::RwLock<Option<Reconciliation>>>,
}

impl core::fmt::Debug for ReconciliationLog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReconciliationLog").finish_non_exhaustive()
    }
}

impl ReconciliationLog {
    /// An empty one — nothing has been applied yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a pass.
    pub fn record(&self, pass: Reconciliation) {
        // A poisoned lock means a writer panicked; overwrite anyway rather than stay stale.
        match self.last.write() {
            Ok(mut slot) => *slot = Some(pass),
            Err(poisoned) => *poisoned.into_inner() = Some(pass),
        }
    }

    /// The last pass, if there has been one.
    #[must_use]
    pub fn last(&self) -> Option<Reconciliation> {
        match self.last.read() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// `GET /api/v1/admin/reconciliation` — the GitOps state (design §11, §10.4bis).
#[derive(Clone, Debug)]
pub struct ReadReconciliation {
    log: ReconciliationLog,
}

impl ReadReconciliation {
    #[must_use]
    pub fn new(log: ReconciliationLog) -> Self {
        Self { log }
    }

    /// What the last pass did on this replica; `None` means none has finished here,
    /// rather than zeroes that would look healthy.
    pub fn execute(&self, caller: &Caller) -> Result<Option<Reconciliation>, Failure> {
        caller.ensure_platform_admin()?;
        Ok(self.log.last())
    }
}

/// `POST /api/v1/admin/channels/{id}/join` — the audited way in (§5.4).
#[derive(Clone)]
pub struct SelfJoin {
    channels: Arc<dyn ChannelRepository>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn notify_ports::Clock>,
}

impl core::fmt::Debug for SelfJoin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SelfJoin").finish_non_exhaustive()
    }
}

impl SelfJoin {
    #[must_use]
    pub fn new(
        channels: Arc<dyn ChannelRepository>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn notify_ports::Clock>,
    ) -> Self {
        Self {
            channels,
            audit,
            clock,
        }
    }

    /// Join a channel as the calling admin, and record it. Subscribe only: the role is
    /// not a licence to publish.
    pub async fn execute(&self, caller: &Caller, id: &ChannelId) -> Result<(), Failure> {
        caller.ensure_platform_admin()?;

        let mut channel = self
            .channels
            .get(id)
            .await?
            .ok_or(notify_domain::DomainError::NotificationGone)?;

        // A managed channel is joined too; the reconciler removes it again (§11), and the
        // audit entry still records the look.
        channel.upsert_member(notify_domain::Membership::new(
            caller.principal.clone(),
            vec![notify_domain::Scope::Subscribe],
        )?);
        self.channels.upsert(&channel).await?;

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::PLATFORM_CHANNEL_SELF_JOIN,
                target: Some(id.to_string()),
                channel: Some(id.clone()),
                detail: None,
                at: self.clock.now(),
            })
            .await?;
        Ok(())
    }
}

/// `GET /api/v1/admin/trust-policies` — what this instance was deployed with.
/// Read-only: trust policies come from GitOps (§11).
#[derive(Clone, Debug)]
pub struct ListTrustPolicies {
    policies: crate::federate::LivePolicies,
}

impl ListTrustPolicies {
    /// Wire it with what the composition root parsed.
    #[must_use]
    pub fn new(policies: crate::federate::LivePolicies) -> Self {
        Self { policies }
    }

    /// Every configured policy.
    pub fn execute(&self, caller: &Caller) -> Result<Vec<TrustPolicyCard>, Failure> {
        caller.ensure_platform_admin()?;

        // The set in force, not the one parsed at boot (a SIGHUP may have replaced it).
        Ok(self
            .policies
            .current()
            .configured()
            .iter()
            .map(|policy| TrustPolicyCard {
                issuer: policy.issuer.clone(),
                subject: policy.subject.to_string(),
                channels: policy.channels.iter().map(ToString::to_string).collect(),
                scopes: policy.scopes.clone(),
                channel_scoped: policy.channel_scoped,
            })
            .collect())
    }
}

/// One trust policy as the console shows it.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TrustPolicyCard {
    /// The forge it trusts.
    pub issuer: String,
    /// The anchored subject pattern (§5.3).
    pub subject: String,
    /// The channel patterns it grants.
    pub channels: Vec<String>,
    /// What it grants there.
    pub scopes: Vec<Scope>,
    /// Whether it came from a channel's own `publishers` block, which wins over a global one (§11).
    pub channel_scoped: bool,
}

#[cfg(test)]
mod tests {
    use notify_domain::{Channel, Membership, Principal, Scope, UserId};
    use notify_ports::PageRequest;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;

    fn admin() -> Caller {
        Caller {
            platform_admin: true,
            ..Caller::member_of(
                Principal::User {
                    id: UserId::generate(),
                },
                vec!["platform-admins".to_owned()],
            )
        }
    }

    fn ordinary() -> Caller {
        Caller::member(Principal::User {
            id: UserId::generate(),
        })
    }

    fn seeded() -> InMemory {
        let store = InMemory::new();
        let mut channel =
            Channel::dynamic(ChannelId::parse("cluster-ovh-1").unwrap(), None).unwrap();
        channel.upsert_member(
            Membership::new(
                Principal::User {
                    id: UserId::generate(),
                },
                vec![Scope::Subscribe],
            )
            .unwrap(),
        );
        store.seed_channel(channel);
        store
    }

    #[tokio::test]
    async fn critical_t30_a_user_without_the_role_reaches_nothing() {
        let store = seeded();
        let inventory = ChannelInventory::new(Arc::new(store.clone()));
        let audit = BrowseAudit::new(Arc::new(store));

        assert_eq!(
            inventory
                .execute(&ordinary(), PageRequest::first())
                .await
                .unwrap_err()
                .code(),
            Some("platform.role_required")
        );
        assert_eq!(
            audit
                .execute(&ordinary(), AuditQuery::default())
                .await
                .unwrap_err()
                .code(),
            Some("platform.role_required"),
            "and the refusal is its own code: a missing channel scope sends a \
             client somewhere a channel manager can help, which nobody can here"
        );
    }

    #[tokio::test]
    async fn critical_t30_the_inventory_is_record_cards_and_never_content() {
        let store = seeded();
        let inventory = ChannelInventory::new(Arc::new(store));

        let cards = inventory
            .execute(&admin(), PageRequest::first())
            .await
            .unwrap()
            .items;

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, ChannelId::parse("cluster-ovh-1").unwrap());
        assert_eq!(cards[0].members, 1);
        // The type has no field that could hold a notification.
    }

    #[tokio::test]
    async fn critical_t30_a_self_join_is_recorded() {
        let store = seeded();
        let clock = TestClock::new();
        let join = SelfJoin::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock),
        );
        let admin = admin();
        let id = ChannelId::parse("cluster-ovh-1").unwrap();

        join.execute(&admin, &id).await.unwrap();

        let entries = AuditLog::query(
            &store,
            &AuditQuery {
                page: PageRequest::first(),
                ..AuditQuery::default()
            },
        )
        .await
        .unwrap();
        assert!(
            entries
                .items
                .iter()
                .any(|entry| entry.action.as_str() == "platform.channel.self_join"),
            "reading somebody's channel has to leave a trace a later reader can find"
        );
    }

    #[tokio::test]
    async fn a_self_join_grants_reading_and_not_publishing() {
        let store = seeded();
        let join = SelfJoin::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(TestClock::new()),
        );
        let admin = admin();
        let id = ChannelId::parse("cluster-ovh-1").unwrap();

        join.execute(&admin, &id).await.unwrap();

        let channel = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        let membership = channel
            .members
            .iter()
            .find(|member| member.principal == admin.principal)
            .unwrap();
        assert_eq!(
            membership.scopes,
            vec![Scope::Subscribe],
            "the role is a way to read an incident, not a licence to speak for a team"
        );
    }

    #[tokio::test]
    async fn a_self_join_needs_the_role_like_everything_else() {
        let store = seeded();
        let join = SelfJoin::new(
            Arc::new(store.clone()),
            Arc::new(store),
            Arc::new(TestClock::new()),
        );

        assert!(
            join.execute(&ordinary(), &ChannelId::parse("cluster-ovh-1").unwrap())
                .await
                .is_err()
        );
    }
}
