//! Declarative channels and service accounts (design §11).
//!
//! Config owns the shape of a managed object; the API may not reshape it. So this
//! runs at boot and puts the database back the way the file says — which is what
//! makes "delete a managed channel through the API, watch it resurrect" impossible
//! rather than merely discouraged.
//!
//! # The three kinds of access binding, and why they are not alike
//!
//! * `oidc_group` becomes a [`GroupBinding`] on the channel. Nothing is stored per
//!   user, and a token that stops carrying the group stops granting access — see
//!   the type's own documentation.
//! * `service_account` becomes a [`Membership`] straight away: the account is
//!   declared in the same file, so it always exists by the time we get here.
//! * `oidc_sub` and `email` become a [`Membership`] **only once that person has
//!   logged in at least once**, because a membership needs a `UserId` and a user
//!   row is created on first login (design §5.1).
//!
//! That last one is a real limitation and it is why this runs on a timer as well as
//! at boot: somebody bound by email who has never logged in gains their access
//! within one interval of their first login, not at the next restart. It is
//! reported at reconcile time rather than passed over in silence.

use std::sync::Arc;

use notify_domain::{
    Channel, ChannelId, ChannelPolicy, GroupBinding, Membership, Origin, Principal, ServiceAccount,
    ServiceAccountId, TtlPolicy,
};
use notify_ports::{ChannelRepository, Clock, ServiceAccountRepository, UserRepository};

use crate::config::{Access, Config, ManagedChannel, OnRemoved};

/// What one pass did, for the log and for the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconcileReport {
    /// Managed channels written.
    pub channels: usize,
    /// Managed service accounts written.
    pub accounts: usize,
    /// Channels that left the config and were archived or deleted.
    pub removed: usize,
    /// Bindings by `oidc_sub` or `email` naming somebody who has never logged in.
    pub pending: Vec<String>,
}

/// Puts the database back the way the configuration says.
pub(crate) struct Reconciler {
    channels: Arc<dyn ChannelRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    users: Arc<dyn UserRepository>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for Reconciler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reconciler").finish_non_exhaustive()
    }
}

impl Reconciler {
    pub(crate) fn new(
        channels: Arc<dyn ChannelRepository>,
        accounts: Arc<dyn ServiceAccountRepository>,
        users: Arc<dyn UserRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            channels,
            accounts,
            users,
            clock,
        }
    }

    /// Apply the configuration.
    pub(crate) async fn execute(&self, config: &Config) -> anyhow::Result<ReconcileReport> {
        let mut report = ReconcileReport::default();

        // Service accounts first: a channel binding can name one, and resolving a
        // name that has not been written yet would drop the membership.
        for declared in &config.service_accounts {
            let id = ServiceAccountId::parse(&declared.id)?;
            let existing = self.accounts.get(&id).await?;

            self.accounts
                .upsert(&ServiceAccount {
                    id,
                    display_name: declared.display_name.clone(),
                    description: declared.description.clone(),
                    origin: Origin::Managed,
                    // A managed account has no owner: the configuration administers
                    // it, and whoever happened to apply that config is not its
                    // owner (§11).
                    created_by: None,
                    // Preserved rather than reset: config declares that the account
                    // exists, not when it was created.
                    created_at: existing
                        .as_ref()
                        .map_or_else(|| self.clock.now(), |account| account.created_at),
                    disabled_at: existing.and_then(|account| account.disabled_at),
                })
                .await?;
            report.accounts += 1;
        }

        let declared: Vec<ChannelId> = config
            .channels
            .managed
            .iter()
            .map(|channel| ChannelId::parse(&channel.id))
            .collect::<Result<_, _>>()?;

        for managed in &config.channels.managed {
            let channel = self.build(managed, &mut report).await?;
            self.channels.upsert(&channel).await?;
            report.channels += 1;
        }

        report.removed = self.drop_departed(config, &declared).await?;

        Ok(report)
    }

    /// Turn one `[[channels.managed]]` block into a channel.
    async fn build(
        &self,
        managed: &ManagedChannel,
        report: &mut ReconcileReport,
    ) -> anyhow::Result<Channel> {
        let id = ChannelId::parse(&managed.id)?;
        let mut channel = Channel::dynamic(id.clone(), managed.name.as_deref())?;
        // Declared in config, so config owns its shape and the API must refuse to
        // reshape it (§11).
        channel.origin = Origin::Managed;
        channel.policy = ChannelPolicy {
            ttl: TtlPolicy {
                default: managed.default_ttl.unwrap_or(TtlPolicy::default().default),
                max: managed.max_ttl,
                allow_forever: TtlPolicy::default().allow_forever,
            },
            default_priority: managed
                .default_priority
                .unwrap_or(ChannelPolicy::default().default_priority),
            receipts_enabled: managed.receipts,
        };

        for access in &managed.access {
            self.apply(&mut channel, access, report).await?;
        }

        Ok(channel)
    }

    /// Turn one access binding into whichever door it names.
    async fn apply(
        &self,
        channel: &mut Channel,
        access: &Access,
        report: &mut ReconcileReport,
    ) -> anyhow::Result<()> {
        let scopes = access.scopes.clone();

        if let Some(group) = &access.oidc_group {
            channel.upsert_group(GroupBinding::new(group.clone(), scopes)?);
            return Ok(());
        }

        if let Some(id) = &access.service_account {
            let principal = Principal::ServiceAccount {
                id: ServiceAccountId::parse(id)?,
            };
            channel.upsert_member(Membership::new(principal, scopes)?);
            return Ok(());
        }

        // The two that need a user row. `find_by_*` answering `None` is not an
        // error: it means that person has not logged in yet.
        let found = if let Some(subject) = &access.oidc_sub {
            self.users.find_by_subject(subject).await?
        } else if let Some(email) = &access.email {
            self.users.find_by_email(email).await?
        } else {
            // The config validator already refuses a binding that names nothing.
            return Ok(());
        };

        match found {
            Some(user) => channel.upsert_member(Membership::new(user.principal(), scopes)?),
            None => report.pending.push(format!(
                "{} in `{}` — that person has not logged in yet",
                access
                    .oidc_sub
                    .as_deref()
                    .or(access.email.as_deref())
                    .unwrap_or("?"),
                channel.id
            )),
        }

        Ok(())
    }

    /// Handle managed channels that have left the configuration (§11).
    async fn drop_departed(
        &self,
        config: &Config,
        declared: &[ChannelId],
    ) -> anyhow::Result<usize> {
        let stored = self.channels.list_managed().await?;
        let departed: Vec<&Channel> = stored
            .iter()
            .filter(|channel| !declared.contains(&channel.id))
            .collect();

        let mut removed = 0;
        for channel in departed {
            match config.channels.on_removed {
                OnRemoved::Delete => {
                    self.channels.delete(&channel.id).await?;
                    tracing::warn!(channel = %channel.id, "deleted: it left the configuration");
                }
                OnRemoved::Archive => {
                    // Handed back to the API rather than deleted. Deleting takes the
                    // history with it, and a config typo should not — which is why
                    // this is the default (§11).
                    let mut archived = channel.clone();
                    archived.origin = Origin::Dynamic;
                    self.channels.upsert(&archived).await?;
                    tracing::warn!(
                        channel = %channel.id,
                        "archived: it left the configuration, so it is no longer managed"
                    );
                }
            }
            removed += 1;
        }

        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{Scope, User, UserId};
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;
    use crate::config::{Access, Channels, ManagedChannel, ManagedServiceAccount};

    fn reconciler(store: &InMemory, clock: &TestClock) -> Reconciler {
        Reconciler::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
        )
    }

    fn access(scopes: Vec<Scope>) -> Access {
        Access {
            scopes,
            ..Access::default()
        }
    }

    fn config(managed: Vec<ManagedChannel>) -> Config {
        Config {
            channels: Channels {
                managed,
                ..Channels::default()
            },
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn a_group_binding_becomes_group_access_and_never_a_membership() {
        let store = InMemory::new();
        let clock = TestClock::new();

        let report = reconciler(&store, &clock)
            .execute(&config(vec![ManagedChannel {
                id: "ci-infra".to_owned(),
                access: vec![Access {
                    oidc_group: Some("dev-team".to_owned()),
                    ..access(vec![Scope::Subscribe])
                }],
                ..ManagedChannel::default()
            }]))
            .await
            .unwrap();

        assert_eq!(report.channels, 1);

        let stored = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(stored.origin, Origin::Managed);
        assert!(
            stored.members.is_empty(),
            "nothing is stored per user; the token carries the group"
        );
        assert_eq!(stored.group_access.len(), 1);
        assert!(stored.grants_group(&["dev-team".to_owned()], Scope::Subscribe));
    }

    #[tokio::test]
    async fn an_email_binding_waits_for_that_person_to_log_in_and_says_so() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let declared = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            access: vec![Access {
                email: Some("carol@weebo.local".to_owned()),
                ..access(vec![Scope::Subscribe])
            }],
            ..ManagedChannel::default()
        }]);

        let report = reconciler(&store, &clock).execute(&declared).await.unwrap();

        assert_eq!(
            report.pending.len(),
            1,
            "a binding that resolved to nothing is reported, not passed over"
        );

        // She logs in, a user row appears, and the next pass picks her up.
        let carol = User::from_claims(
            UserId::generate(),
            "carol",
            Some("carol@weebo.local"),
            None,
            clock.now(),
        )
        .unwrap();
        UserRepository::upsert(&store, &carol).await.unwrap();

        let report = reconciler(&store, &clock).execute(&declared).await.unwrap();
        assert!(report.pending.is_empty());

        let stored = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(stored.grants(&carol.principal(), Scope::Subscribe));
    }

    #[tokio::test]
    async fn a_service_account_binding_resolves_immediately() {
        let store = InMemory::new();
        let clock = TestClock::new();

        let mut declared = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            access: vec![Access {
                service_account: Some("bot-argocd".to_owned()),
                ..access(vec![Scope::Publish])
            }],
            ..ManagedChannel::default()
        }]);
        declared.service_accounts = vec![ManagedServiceAccount {
            id: "bot-argocd".to_owned(),
            display_name: Some("ArgoCD".to_owned()),
            description: None,
        }];

        let report = reconciler(&store, &clock).execute(&declared).await.unwrap();

        assert_eq!(report.accounts, 1);
        assert!(report.pending.is_empty(), "it is declared in the same file");

        let stored = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(stored.grants(
            &Principal::ServiceAccount {
                id: ServiceAccountId::parse("bot-argocd").unwrap()
            },
            Scope::Publish
        ));
    }

    #[tokio::test]
    async fn a_channel_that_leaves_the_config_is_archived_rather_than_deleted() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let id = ChannelId::parse("ci-infra").unwrap();

        reconciler(&store, &clock)
            .execute(&config(vec![ManagedChannel {
                id: "ci-infra".to_owned(),
                ..ManagedChannel::default()
            }]))
            .await
            .unwrap();

        let report = reconciler(&store, &clock)
            .execute(&config(Vec::new()))
            .await
            .unwrap();

        assert_eq!(report.removed, 1);
        let stored = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        assert_eq!(
            stored.origin,
            Origin::Dynamic,
            "deleting takes the history with it, and a config typo should not"
        );
    }

    #[tokio::test]
    async fn reconciling_twice_changes_nothing_the_second_time() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let declared = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            receipts: true,
            access: vec![Access {
                oidc_group: Some("dev-team".to_owned()),
                ..access(vec![Scope::Subscribe])
            }],
            ..ManagedChannel::default()
        }]);
        let id = ChannelId::parse("ci-infra").unwrap();

        reconciler(&store, &clock).execute(&declared).await.unwrap();
        let first = ChannelRepository::get(&store, &id).await.unwrap().unwrap();

        reconciler(&store, &clock).execute(&declared).await.unwrap();
        let second = ChannelRepository::get(&store, &id).await.unwrap().unwrap();

        assert_eq!(first, second, "boot, SIGHUP and the timer all run this");
        assert!(second.policy.receipts_enabled);
    }
}
