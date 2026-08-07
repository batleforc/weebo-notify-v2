//! The platform console's use cases (design §5.4, §10.4bis).
//!
//! # The boundary is the whole design, and it is not "admins see everything"
//!
//! §5.4 draws it in one sentence: **metadata and operations, never content**. The
//! role hands over the inventory — which channels exist, who is in them, which keys
//! are dormant — the operations, and the audit log. It does **not** hand over a
//! single notification of a channel the admin is not a member of.
//!
//! That is not caution, it is the only line that keeps §2.4's rule true. Profiles
//! are visible "only through a shared channel", and a role that could read any
//! channel's content would make that sentence false for everybody at once, silently,
//! for as long as somebody held the role. Membership stays the one door to content
//! (scenario T30); an admin may *join* a channel, and that join is audited, which
//! turns "I looked" into something a later reader can see.
//!
//! # Why every use case here re-checks the role
//!
//! Not because the HTTP layer forgets — it does not — but because a check that only
//! exists in a route table is a check that a second route table can be written
//! without. The refusal lives with the operation.

use std::sync::Arc;

use notify_domain::{ChannelId, Scope};
use notify_ports::{AuditEntry, AuditLog, AuditQuery, ChannelRepository, Failure, Page, actions};
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
    /// Wire it.
    #[must_use]
    pub fn new(audit: Arc<dyn AuditLog>) -> Self {
        Self { audit }
    }

    /// Browse, filtered.
    ///
    /// The log records *acts*, not payloads, so this is inside the boundary: "alice
    /// joined `cluster-ovh-1` at 14:02" is metadata about the instance, and it is
    /// precisely what the role exists to make readable.
    pub async fn execute(
        &self,
        caller: &Caller,
        query: AuditQuery,
    ) -> Result<Page<AuditEntry>, Failure> {
        caller.ensure_platform_admin()?;
        Ok(self.audit.query(&query).await?)
    }
}

/// `GET /api/v1/admin/channels` — the inventory (design §10.4bis).
///
/// A channel is a **record card** here and not an inbox: id, origin, policy,
/// members. The notifications are not reachable through this use case and must not
/// become reachable through it later — that is what `GET /channels/{id}/notifications`
/// is for, and it answers to membership.
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
    /// Wire it.
    #[must_use]
    pub fn new(channels: Arc<dyn ChannelRepository>) -> Self {
        Self { channels }
    }

    /// Every channel on the instance, as record cards.
    pub async fn execute(&self, caller: &Caller) -> Result<Vec<ChannelCard>, Failure> {
        caller.ensure_platform_admin()?;

        let mut cards = Vec::new();
        for id in self.channels.all_ids().await? {
            let Some(channel) = self.channels.get(&id).await? else {
                // Deleted between the two reads. Not an error: an inventory is a
                // snapshot, and refusing the whole page over one vanished row would
                // make the console fail exactly when somebody is tidying up.
                continue;
            };
            cards.push(ChannelCard {
                id: channel.id,
                origin_managed: channel.origin.is_managed(),
                members: channel.members.len(),
                group_bindings: channel.group_access.len(),
                receipts_enabled: channel.policy.receipts_enabled,
            });
        }
        Ok(cards)
    }
}

/// What the console shows for one channel.
///
/// Counts rather than lists for members: the console's question is "is this channel
/// configured the way we think", and a name here would put somebody's identity on a
/// screen belonging to a channel they may not share with the reader (§2.4).
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
    /// The interval elapsed. The common case, and the reason this enum exists: a
    /// timer pass looks exactly like a signalled one in a timestamp.
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
    ///
    /// §10.4bis asks this screen for "le dernier SIGHUP", which a timestamp alone
    /// cannot answer: the reconciler also runs every minute, so `at` is almost
    /// always the timer and an operator who has just sent a signal cannot tell
    /// whether it landed.
    pub trigger: Trigger,
    /// Managed channels written.
    pub channels: usize,
    /// Managed service accounts written.
    pub accounts: usize,
    /// Channels that left the configuration and were archived or deleted.
    pub removed: usize,
    /// Bindings by `oidc_sub` or `email` naming somebody who has never logged in.
    ///
    /// The one thing on this screen that is *actionable and quiet otherwise*: such a
    /// binding grants nothing until that person's first login (§14.1, M1.8), and
    /// without a place to see it the symptom is "alice says she cannot see the
    /// channel" with nothing to connect it to.
    pub pending: Vec<String>,
}

/// Where the composition root leaves its last pass for the console to read.
///
/// # Why a slot and not a port
///
/// Reconciliation is not storage and not an external system: it is something this
/// process did, a minute ago, in its own memory. A port would mean an adapter, a
/// double and a trait for a value that never leaves the replica.
///
/// # And that is exactly what makes it per-replica
///
/// Unlike the purge (§11, leader-only), **every** replica reconciles — so this is
/// the last pass of *whichever replica served the request*, not of the instance.
/// Behind a load balancer two reads can legitimately differ by a tick. Reporting it
/// as instance-wide would be the more comfortable lie; the field is documented as a
/// replica's answer instead.
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
        // A poisoned lock here means a previous writer panicked mid-write. Reading
        // through it costs nothing: the worst case is a stale report on a console,
        // and refusing to record would make that permanent.
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
    /// Wire it.
    #[must_use]
    pub fn new(log: ReconciliationLog) -> Self {
        Self { log }
    }

    /// What the last pass did on this replica.
    ///
    /// `None` means no pass has finished here. In practice the boot pass runs before
    /// the socket is bound, so a caller should never see it — which is precisely why
    /// it is modelled rather than papered over with zeroes: a console showing
    /// "0 channels, 0 accounts" for "we have not looked" would be reporting a
    /// healthy-looking instance that has not applied its configuration.
    pub fn execute(&self, caller: &Caller) -> Result<Option<Reconciliation>, Failure> {
        caller.ensure_platform_admin()?;
        Ok(self.log.last())
    }
}

/// `POST /api/v1/admin/channels/{id}/join` — the audited way in (design §5.4).
///
/// The escape hatch the boundary needs to be usable: an admin investigating an
/// incident does sometimes have to read the channel. Making that a **deliberate,
/// recorded act** is the whole difference between a role that reads everything
/// quietly and one that reads what it asked for, in writing.
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
    /// Wire it.
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

    /// Join a channel as the calling admin, and record it.
    ///
    /// Subscribe only. An admin needs to *read* an incident, and the role is not a
    /// licence to publish into a channel as though one belonged to it — a message
    /// nobody in the team sent would be worse than one nobody could read.
    pub async fn execute(&self, caller: &Caller, id: &ChannelId) -> Result<(), Failure> {
        caller.ensure_platform_admin()?;

        let mut channel = self
            .channels
            .get(id)
            .await?
            .ok_or(notify_domain::DomainError::NotificationGone)?;

        // A managed channel is joined all the same, and the reconciler will take the
        // membership away again (§11). That is the honest outcome: the config owns
        // the member list, so an admin who needs lasting access edits the config —
        // and the audit entry below still records that they looked.
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
///
/// Read-only, and not because the endpoint is lazy: trust policies come from GitOps
/// (§11) and no API path edits them, so a console that offered an edit button would
/// be offering something the next reconciliation undoes. What it answers is the
/// question somebody actually has after a pipeline was refused — *which subjects do
/// we trust, from which forge, on which channels* — and that is metadata, squarely
/// inside the boundary.
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

        // The set in force, not the one parsed at boot: a console that showed the
        // file as it was read would answer "why was that pipeline refused" with
        // yesterday.s configuration after a SIGHUP (D31).
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
    /// Whether it came from a channel's own `publishers` block, which §11 makes win
    /// over a global one — the thing to look at first when a policy "should" match
    /// and does not.
    pub channel_scoped: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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
            inventory.execute(&ordinary()).await.unwrap_err().code(),
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

        let cards = inventory.execute(&admin()).await.unwrap();

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, ChannelId::parse("cluster-ovh-1").unwrap());
        assert_eq!(cards[0].members, 1);
        // The type is the guarantee: there is no field here that could hold a
        // notification, so no future edit can widen this into an inbox by accident.
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
