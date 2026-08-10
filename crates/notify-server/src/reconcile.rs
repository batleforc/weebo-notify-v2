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
use notify_ports::{
    ApiKeyRepository, AuditAction, AuditActor, AuditEntry, AuditLog, ChannelRepository, Clock,
    Declaration, DeclarationKind, DeclarationStore, ServiceAccountRepository, UserRepository,
    actions,
};
use time::OffsetDateTime;

use crate::config::{Access, Config, ManagedChannel, OnRemoved};

/// What one pass did, for the log and for the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconcileReport {
    /// Managed channels written.
    pub channels: usize,
    /// Managed service accounts written.
    pub accounts: usize,
    /// Channels that left the config and were archived or deleted, plus
    /// service accounts that left it and were disabled.
    pub removed: usize,
    /// Bindings by `oidc_sub` or `email` naming somebody who has never logged in.
    pub pending: Vec<String>,
    /// Audit entries this pass wrote — zero unless the file moved (§13, D28).
    pub journalled: usize,
}

impl ReconcileReport {
    /// The same numbers, in the shape the console reads (design §10.4bis).
    ///
    /// The timestamp is passed in rather than taken here: this type is what a pass
    /// *did*, and when it happened is what the caller observed. It also keeps the
    /// conversion testable without a clock.
    pub(crate) fn as_pass(
        &self,
        at: OffsetDateTime,
        trigger: notify_app::Trigger,
    ) -> notify_app::Reconciliation {
        notify_app::Reconciliation {
            at,
            trigger,
            channels: self.channels,
            accounts: self.accounts,
            removed: self.removed,
            pending: self.pending.clone(),
        }
    }
}

/// The three audit actions one kind of declaration can produce.
struct Vocabulary {
    added: AuditAction,
    changed: AuditAction,
    /// `None` where nothing withdraws this kind — see the service account arm.
    removed: Option<AuditAction>,
}

/// `[[trust_policy]]` blocks, as the log identifies them.
///
/// Keyed by issuer **and** subject, because §5.3 lets one forge carry several
/// subject patterns with different rights: keying on the issuer alone would make
/// "a second branch pattern was granted" indistinguishable from "the existing one
/// was edited", and both would be reported against the same target.
fn trust_policies(config: &Config) -> anyhow::Result<Vec<Declaration>> {
    config
        .trust_policy
        .iter()
        .map(|policy| {
            Ok(Declaration {
                key: format!("{} {}", policy.issuer, policy.subject),
                snapshot: serde_json::to_value(policy)?,
            })
        })
        .collect()
}

/// `[[channels.managed]]` blocks, keyed by channel id.
fn managed_channels(config: &Config) -> anyhow::Result<Vec<Declaration>> {
    config
        .channels
        .managed
        .iter()
        .map(|channel| {
            Ok(Declaration {
                key: channel.id.clone(),
                snapshot: serde_json::to_value(channel)?,
            })
        })
        .collect()
}

/// `[[service_accounts]]` blocks, keyed by account id.
fn managed_accounts(config: &Config) -> anyhow::Result<Vec<Declaration>> {
    config
        .service_accounts
        .iter()
        .map(|account| {
            Ok(Declaration {
                key: account.id.clone(),
                snapshot: serde_json::to_value(account)?,
            })
        })
        .collect()
}

/// Puts the database back the way the configuration says.
pub(crate) struct Reconciler {
    channels: Arc<dyn ChannelRepository>,
    accounts: Arc<dyn ServiceAccountRepository>,
    users: Arc<dyn UserRepository>,
    clock: Arc<dyn Clock>,
    /// What the last pass applied, so this one can say what moved (§13, D28).
    declarations: Arc<dyn DeclarationStore>,
    /// Where that goes.
    audit: Arc<dyn AuditLog>,
    /// So a service account retired by [`Self::retire_departed_accounts`] loses
    /// its keys in the same pass, not on whatever schedule they would otherwise
    /// expire on.
    keys: Arc<dyn ApiKeyRepository>,
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
        declarations: Arc<dyn DeclarationStore>,
        audit: Arc<dyn AuditLog>,
        keys: Arc<dyn ApiKeyRepository>,
    ) -> Self {
        Self {
            channels,
            accounts,
            users,
            clock,
            declarations,
            audit,
            keys,
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
                    // Not carried forward from `existing`: declaring the account is
                    // what config uses to mean "this exists and is active" — the same
                    // rule `build()` applies to a managed channel reappearing after
                    // being archived. An account `retire_departed_accounts` disabled
                    // because it left the file must become usable again if the file
                    // names it once more, or removing and re-adding an id would be a
                    // one-way trip with no API able to undo it (`ensure_may_disable`
                    // refuses `Origin::Managed` outright).
                    disabled_at: None,
                })
                .await?;
            report.accounts += 1;
        }

        let declared_accounts: Vec<ServiceAccountId> = config
            .service_accounts
            .iter()
            .map(|account| ServiceAccountId::parse(&account.id))
            .collect::<Result<_, _>>()?;

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

        report.removed = self.drop_departed(config, &declared).await?
            + self.retire_departed_accounts(&declared_accounts).await?;

        // Last, and after the writes: a pass that fails half way must not have
        // announced changes it did not finish applying. The cost is the opposite
        // failure — applied but unannounced, if the process dies here — and that is
        // the right way round, because the next pass re-diffs against the same
        // stored snapshot and announces them then.
        report.journalled = self.journal(config).await?;

        Ok(report)
    }

    /// Write what the file changed since the last pass into the audit log (§13).
    ///
    /// Returns how many entries were written, which is zero on a steady instance —
    /// the property that keeps this log readable, given the pass runs every minute.
    async fn journal(&self, config: &Config) -> anyhow::Result<usize> {
        let at = self.clock.now();
        let mut written = 0;

        for (kind, declarations, vocabulary) in [
            (
                DeclarationKind::TrustPolicy,
                trust_policies(config),
                Vocabulary {
                    added: actions::TRUST_POLICY_ADDED,
                    changed: actions::TRUST_POLICY_CHANGED,
                    removed: Some(actions::TRUST_POLICY_REMOVED),
                },
            ),
            (
                DeclarationKind::Channel,
                managed_channels(config),
                Vocabulary {
                    added: actions::MANAGED_CHANNEL_ADDED,
                    changed: actions::MANAGED_CHANNEL_CHANGED,
                    removed: Some(actions::MANAGED_CHANNEL_REMOVED),
                },
            ),
            (
                DeclarationKind::ServiceAccount,
                managed_accounts(config),
                // Still `None`, but not for the old reason: a departed managed
                // account *is* retired now (disabled, its keys revoked) by
                // `retire_departed_accounts`, which writes its own audit entry —
                // richer than this generic declaration-diff could produce, since
                // it carries how many keys that took. Wiring this vocabulary's
                // `removed` too would double-journal the same event.
                Vocabulary {
                    added: actions::MANAGED_ACCOUNT_ADDED,
                    changed: actions::MANAGED_ACCOUNT_CHANGED,
                    removed: None,
                },
            ),
        ] {
            let declarations = declarations?;
            let before = self.declarations.snapshot(kind).await?;
            let changes = notify_ports::declarations::diff(&before, &declarations);

            for added in &changes.added {
                self.record(
                    &vocabulary.added,
                    kind,
                    &added.key,
                    &added.snapshot,
                    None,
                    at,
                )
                .await?;
                written += 1;
            }
            for change in &changes.changed {
                self.record(
                    &vocabulary.changed,
                    kind,
                    &change.current.key,
                    &change.current.snapshot,
                    Some(&change.previous.snapshot),
                    at,
                )
                .await?;
                written += 1;
            }
            for removed in &changes.removed {
                if let Some(action) = vocabulary.removed {
                    self.record(&action, kind, &removed.key, &removed.snapshot, None, at)
                        .await?;
                    written += 1;
                }
            }

            if !changes.is_empty() {
                self.declarations.apply(kind, &declarations, at).await?;
            }
        }

        Ok(written)
    }

    /// One audit entry, from the configuration rather than from anybody (§11, D28).
    async fn record(
        &self,
        action: &AuditAction,
        kind: DeclarationKind,
        key: &str,
        snapshot: &serde_json::Value,
        previous: Option<&serde_json::Value>,
        at: OffsetDateTime,
    ) -> anyhow::Result<()> {
        let mut detail = serde_json::json!({ "declaration": snapshot });
        if let Some(previous) = previous
            && let Some(object) = detail.as_object_mut()
        {
            object.insert("previous".to_owned(), previous.clone());
        }

        self.audit
            .record(&AuditEntry {
                actor: AuditActor::configuration(),
                action: *action,
                target: Some(key.to_owned()),
                // A managed channel's entry names its channel, so the console's
                // per-channel filter finds it; the other two kinds are not about
                // one channel, and a trust policy naming the first of several it
                // grants would be a half-truth.
                channel: (kind == DeclarationKind::Channel)
                    .then(|| ChannelId::parse(key))
                    .transpose()?,
                detail: Some(detail),
                at,
            })
            .await?;
        Ok(())
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
            // `limits.publish_per_min` (§8.2, §11). Absent means the published
            // default, which is why this is threaded through as `Option` the whole
            // way rather than defaulted to 600 here: the number lives in the
            // adapter that enforces it, and a second copy of it in the reconciler
            // is a second copy to drift.
            publish_per_minute: managed.limits.and_then(|limits| limits.publish_per_min),
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

    /// Disable managed service accounts that left the configuration, revoking
    /// their keys with them — the service-account analogue of
    /// [`Self::drop_departed`] above.
    ///
    /// Disable, never archive-to-dynamic: a stray managed account is usually
    /// the id changing between passes rather than the account really being
    /// withdrawn (see the module doc's note on `service_account` bindings),
    /// and a dynamic account is still fully usable — the opposite of what a
    /// stray one needs. Idempotent: an already-disabled account is skipped,
    /// so a steady instance reconciling every SIGHUP does not re-revoke keys
    /// or re-journal the same retirement forever.
    async fn retire_departed_accounts(
        &self,
        declared: &[ServiceAccountId],
    ) -> anyhow::Result<usize> {
        let stored = self.accounts.list_managed().await?;
        let mut retired = 0;

        for mut account in stored {
            if declared.contains(&account.id) || account.disabled_at.is_some() {
                continue;
            }

            let now = self.clock.now();
            // Keys revoked *before* the account is marked disabled, on the same
            // terms `DeactivateServiceAccount` already sets for the API path: the
            // account row alone would stop only *new* authentications, and a key
            // left usable is a credential still in somebody's CI configuration.
            // The order also makes a failure between the two calls self-healing —
            // `disabled_at` is the flag this loop treats as "already retired", so
            // it must not be persisted until the keys are actually gone. If the
            // upsert below fails, the next pass finds the account still enabled
            // and revokes (a no-op the second time) and disables it again, rather
            // than skipping it forever with its keys still live.
            let revoked = self.keys.revoke_all_for(&account.principal(), now).await?;
            account.disable(now);
            self.accounts.upsert(&account).await?;

            self.audit
                .record(&AuditEntry {
                    actor: AuditActor::configuration(),
                    action: actions::MANAGED_ACCOUNT_REMOVED,
                    target: Some(account.id.as_str().to_owned()),
                    channel: None,
                    detail: Some(serde_json::json!({ "keys_revoked": revoked })),
                    at: now,
                })
                .await?;

            tracing::warn!(
                account = %account.id,
                keys_revoked = revoked,
                "disabled: it left the configuration, so it is no longer managed"
            );
            retired += 1;
        }

        Ok(retired)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, Scope, User, UserId};
    use notify_ports::StoredApiKey;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;
    use crate::config::{Access, Channels, ManagedChannel, ManagedServiceAccount};

    fn reconciler(store: &InMemory, clock: &TestClock) -> Reconciler {
        Reconciler::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
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

    // --- What GitOps changed, in the log (design §13, D28) -------------------

    /// The audit rows one pass wrote, oldest first.
    async fn journal(store: &InMemory) -> Vec<notify_ports::AuditEntry> {
        let mut entries =
            notify_ports::AuditLog::query(store, &notify_ports::AuditQuery::default())
                .await
                .unwrap()
                .items;
        entries.reverse();
        entries
    }

    fn trust_policy(
        issuer: &str,
        subject: &str,
        channels: Vec<String>,
    ) -> crate::config::TrustPolicy {
        crate::config::TrustPolicy {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
            channels,
            ..crate::config::TrustPolicy::default()
        }
    }

    #[tokio::test]
    async fn a_trust_policy_appearing_is_journalled_by_the_configuration() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let config = Config {
            trust_policy: vec![trust_policy(
                "https://forge.example",
                "repo:weebo/infra:ref:refs/heads/main",
                vec!["ci-infra".to_owned()],
            )],
            ..Config::default()
        };

        reconciler(&store, &clock).execute(&config).await.unwrap();

        let entries = journal(&store).await;
        assert_eq!(
            entries.len(),
            1,
            "the policy that arrived, and nothing else"
        );
        assert_eq!(entries[0].action.as_str(), "trust_policy.added");
        // The point of D28: `[[trust_policy]]` is what lets a forge publish here,
        // and editing it left no trace at all in the log §13 says to replay.
        assert_eq!(
            entries[0].actor,
            notify_ports::AuditActor::configuration(),
            "nobody did this — a file did, and the log has to be able to say so"
        );
    }

    #[tokio::test]
    async fn a_steady_instance_writes_nothing() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let config = Config {
            trust_policy: vec![trust_policy("https://forge.example", "repo:a", vec![])],
            ..Config::default()
        };
        let reconciler = reconciler(&store, &clock);

        reconciler.execute(&config).await.unwrap();
        let after_first = journal(&store).await.len();
        reconciler.execute(&config).await.unwrap();
        reconciler.execute(&config).await.unwrap();

        // The pass runs every sixty seconds. A row per pass would bury the rows
        // that matter under the ones that do not, which is the failure §13's own
        // "what does not go in" paragraph is about.
        assert_eq!(journal(&store).await.len(), after_first);
    }

    #[tokio::test]
    async fn an_edited_policy_carries_both_sides() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let reconciler = reconciler(&store, &clock);

        let before = Config {
            trust_policy: vec![trust_policy("https://forge.example", "repo:a", vec![])],
            ..Config::default()
        };
        reconciler.execute(&before).await.unwrap();

        let after = Config {
            trust_policy: vec![trust_policy(
                "https://forge.example",
                "repo:a",
                vec!["ci-infra".to_owned()],
            )],
            ..Config::default()
        };
        reconciler.execute(&after).await.unwrap();

        let entries = journal(&store).await;
        let changed = entries
            .iter()
            .find(|entry| entry.action.as_str() == "trust_policy.changed")
            .unwrap();
        let detail = changed.detail.as_ref().unwrap();

        // "A trust policy changed" is not the sentence §13 promises. What it
        // changed *to*, and *from*, is the whole reason to read the log.
        assert!(detail.get("declaration").is_some());
        assert!(
            detail.get("previous").is_some(),
            "an operator cannot judge a change from one side of it"
        );
    }

    #[tokio::test]
    async fn widening_what_a_forge_may_do_is_journalled_as_a_change() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let reconciler = reconciler(&store, &clock);

        let policy = |scopes: Vec<Scope>| crate::config::TrustPolicy {
            issuer: "https://forge.example".to_owned(),
            subject: "repo:a".to_owned(),
            scopes,
            ..crate::config::TrustPolicy::default()
        };

        reconciler
            .execute(&Config {
                trust_policy: vec![policy(vec![Scope::Publish])],
                ..Config::default()
            })
            .await
            .unwrap();
        reconciler
            .execute(&Config {
                trust_policy: vec![policy(vec![Scope::Publish, Scope::Manage])],
                ..Config::default()
            })
            .await
            .unwrap();

        // The snapshot is the whole block and not a chosen few fields: `scopes`
        // decides what a forge may *do*, and a comparison that skipped it would call
        // a widened policy unchanged — the quietest way to lose an audit trail.
        assert!(
            journal(&store)
                .await
                .iter()
                .any(|entry| entry.action.as_str() == "trust_policy.changed")
        );
    }

    #[tokio::test]
    async fn a_withdrawn_policy_is_journalled_as_removed() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let reconciler = reconciler(&store, &clock);

        let before = Config {
            trust_policy: vec![trust_policy("https://forge.example", "repo:a", vec![])],
            ..Config::default()
        };
        reconciler.execute(&before).await.unwrap();
        reconciler.execute(&Config::default()).await.unwrap();

        // Withdrawing is the half an upsert-only store would lose, and it is the
        // half that matters most: a forge that may no longer publish.
        assert!(
            journal(&store)
                .await
                .iter()
                .any(|entry| entry.action.as_str() == "trust_policy.removed")
        );
    }

    #[tokio::test]
    async fn managed_channels_and_accounts_are_journalled_too() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let config = Config {
            service_accounts: vec![ManagedServiceAccount {
                id: "svc_ci".to_owned(),
                ..ManagedServiceAccount::default()
            }],
            channels: Channels {
                managed: vec![ManagedChannel {
                    id: "ci-infra".to_owned(),
                    ..ManagedChannel::default()
                }],
                ..Channels::default()
            },
            ..Config::default()
        };

        reconciler(&store, &clock).execute(&config).await.unwrap();

        // Doing trust policies alone would have fixed a third of the problem while
        // looking like the whole of it: the same reconciler writes all three, and
        // none of them were logged.
        let actions: Vec<&str> = journal(&store)
            .await
            .iter()
            .map(|entry| entry.action.as_str())
            .collect();
        assert!(actions.contains(&"managed.channel.added"), "{actions:?}");
        assert!(
            actions.contains(&"managed.service_account.added"),
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn a_managed_channel_entry_names_its_channel() {
        let store = InMemory::new();
        let clock = TestClock::new();

        reconciler(&store, &clock)
            .execute(&config(vec![ManagedChannel {
                id: "ci-infra".to_owned(),
                ..ManagedChannel::default()
            }]))
            .await
            .unwrap();

        // So the console's per-channel filter finds it. A trust policy has no one
        // channel — it can grant several — so only this kind carries the field.
        let entries = journal(&store).await;
        let entry = entries
            .iter()
            .find(|entry| entry.action.as_str() == "managed.channel.added")
            .unwrap();
        assert_eq!(
            entry.channel.as_ref().map(ChannelId::as_str),
            Some("ci-infra")
        );
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
    async fn a_service_account_that_leaves_the_config_is_disabled_and_its_keys_revoked() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let id = ServiceAccountId::parse("bot-argocd").unwrap();

        let mut declared = config(Vec::new());
        declared.service_accounts = vec![ManagedServiceAccount {
            id: "bot-argocd".to_owned(),
            display_name: Some("ArgoCD".to_owned()),
            description: None,
        }];
        reconciler(&store, &clock).execute(&declared).await.unwrap();

        // A key minted for it while it was still declared.
        store.seed_api_key(StoredApiKey {
            key: ApiKey {
                id: ApiKeyId::generate(),
                label: Some("ci".to_owned()),
                prefix: ApiKeyPrefix::parse("wnk1abcd").unwrap(),
                owner: ApiKeyOwner::ServiceAccount { id: id.clone() },
                scopes: vec![Scope::Publish],
                channels: Vec::new(),
                publish_per_minute: None,
                created_at: clock.now(),
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            secret_hash: "stub$secret".to_owned(),
        });

        // The next sync's config no longer names it — the id churned, or it was
        // genuinely withdrawn; either way this instance must stop trusting it.
        let report = reconciler(&store, &clock)
            .execute(&config(Vec::new()))
            .await
            .unwrap();

        assert_eq!(report.removed, 1);

        let stored = ServiceAccountRepository::get(&store, &id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            stored.disabled_at.is_some(),
            "left the config, so it must stop being usable"
        );

        let keys = ApiKeyRepository::list_for(&store, &Principal::ServiceAccount { id })
            .await
            .unwrap();
        assert!(
            keys[0].revoked_at.is_some(),
            "a stray managed account's keys must not keep authenticating"
        );
    }

    #[tokio::test]
    async fn retiring_an_account_twice_does_not_re_revoke_or_re_journal() {
        let store = InMemory::new();
        let clock = TestClock::new();

        let mut declared = config(Vec::new());
        declared.service_accounts = vec![ManagedServiceAccount {
            id: "bot-argocd".to_owned(),
            display_name: Some("ArgoCD".to_owned()),
            description: None,
        }];
        reconciler(&store, &clock).execute(&declared).await.unwrap();

        let departed = config(Vec::new());
        let first = reconciler(&store, &clock).execute(&departed).await.unwrap();
        let second = reconciler(&store, &clock).execute(&departed).await.unwrap();

        assert_eq!(first.removed, 1);
        assert_eq!(
            second.removed, 0,
            "already disabled — nothing left to retire"
        );

        let entries = journal(&store).await;
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.action.as_str() == "managed.service_account.removed")
                .count(),
            1,
            "one retirement, one audit entry — not one per pass"
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

    #[tokio::test]
    async fn a_configured_publish_limit_reaches_the_channel_policy() {
        // The step that was missing: `limits.publish_per_min` parsed cleanly and
        // stopped there. It is on the policy now, which is what the publish path
        // reads (§8.2, §11).
        let store = InMemory::new();
        let clock = TestClock::new();
        let declared = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            limits: Some(crate::config::ChannelLimits {
                publish_per_min: Some(30),
            }),
            ..ManagedChannel::default()
        }]);

        reconciler(&store, &clock).execute(&declared).await.unwrap();

        let channel = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(channel.policy.publish_per_minute, Some(30));
    }

    #[tokio::test]
    async fn a_channel_that_declares_no_limits_keeps_the_published_default() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let declared = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            ..ManagedChannel::default()
        }]);

        reconciler(&store, &clock).execute(&declared).await.unwrap();

        let channel = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            channel.policy.publish_per_minute, None,
            "`None` means §8.2's default, not `0`"
        );
    }
}
