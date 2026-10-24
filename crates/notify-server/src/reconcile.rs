//! Declarative channels and service accounts (design §11): each pass puts the database back as the file says.
//! Bindings: `oidc_group` → [`GroupBinding`]; `service_account` → [`Membership`]; `oidc_sub`/`email` →
//! [`Membership`] only once that user has logged in, hence the timer.

use std::sync::Arc;

use notify_domain::{
    Channel, ChannelId, ChannelPolicy, GroupBinding, Membership, Origin, Principal, ServiceAccount,
    ServiceAccountId, TtlPolicy,
};
use notify_ports::{
    ApiKeyRepository, AuditAction, AuditActor, AuditEntry, AuditLog, ChannelRepository, Clock,
    Declaration, DeclarationKind, DeclarationStore, EmailMatch, ServiceAccountRepository,
    UserRepository, actions,
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
    /// Removals this pass would have made and refused, because there were more of
    /// them than `[reconcile] max_removals` allows (see [`Reconciler::execute`]).
    pub refused: usize,
}

impl ReconcileReport {
    /// The same numbers in the console's shape (design §10.4bis); the caller supplies the timestamp.
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

/// `[[trust_policy]]` blocks, keyed by issuer and subject (one forge may carry several patterns).
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
                snapshot: channel_snapshot(channel, &config.sinks)?,
            })
        })
        .collect()
}

/// A channel's declaration as it is stored and journalled, webhook urls redacted.
///
/// The snapshot lands in `managed_declarations` and, on every change, in an audit
/// entry's `detail` — and a webhook url often carries its consumer's credential in
/// the path. So it is stored as `notify_ports::redact_url` renders it: the origin
/// and the same keyed fingerprint the sink id carries, which still changes when the
/// url does, so rotating one is journalled as the change it is — and which, unlike
/// an unkeyed digest, a reader of the log or a backup cannot guess the url back from.
fn channel_snapshot(
    channel: &ManagedChannel,
    settings: &crate::config::Sinks,
) -> anyhow::Result<serde_json::Value> {
    let mut snapshot = serde_json::to_value(channel)?;
    // Only read when there is a url to redact: `Config::validate` requires the key
    // exactly when a webhook is configured.
    let mut key = None;
    if let Some(sinks) = snapshot
        .get_mut("sinks")
        .and_then(serde_json::Value::as_array_mut)
    {
        for sink in sinks {
            let url = sink
                .get("url")
                .and_then(serde_json::Value::as_str)
                .and_then(|raw| url::Url::parse(raw).ok());
            if let (Some(url), Some(object)) = (url, sink.as_object_mut()) {
                let key = match &mut key {
                    Some(key) => key,
                    None => key.insert(settings.fingerprint_key()?),
                };
                object.insert(
                    "url".to_owned(),
                    serde_json::Value::String(notify_ports::redact_url(&url, key)),
                );
            }
        }
    }
    Ok(snapshot)
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
    /// Revokes the keys of accounts [`Self::retire_departed_accounts`] retires, in the same pass.
    keys: Arc<dyn ApiKeyRepository>,
    /// Where a refused mass removal is counted.
    metrics: Arc<dyn notify_ports::Metrics>,
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
            metrics: Arc::new(notify_ports::NoMetrics),
        }
    }

    /// Count refused mass removals into `metrics`.
    pub(crate) fn with_metrics(mut self, metrics: Arc<dyn notify_ports::Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Apply the configuration.
    ///
    /// # The blast-radius guard
    ///
    /// A truncated file, a bad merge or an empty `WNK_CONFIG` mount reads exactly
    /// like an operator removing most of what they manage, and on its own this pass
    /// would act on it: every key of every managed service account revoked, and
    /// with `on_removed = "delete"` every channel deleted with its history. So when
    /// a pass would remove (archive, delete or disable) more than
    /// `[reconcile] max_removals` entities, it applies everything else in the file,
    /// refuses the removals, logs an error and counts
    /// `reconcile_removals_refused_total`. `allow_mass_removal = true` is how an
    /// operator says the big removal is meant.
    ///
    /// Re-declaring an account that a pass *did* disable re-enables it, but its
    /// keys stay revoked: a revocation is not undone by a file edit, and whoever
    /// needs the account back issues new keys. That is deliberate — the keys may
    /// have leaked while nobody was watching them.
    pub(crate) async fn execute(&self, config: &Config) -> anyhow::Result<ReconcileReport> {
        let mut report = ReconcileReport::default();

        // Service accounts first: channel bindings can name them.
        for declared in &config.service_accounts {
            let id = ServiceAccountId::parse(&declared.id)?;
            let existing = self.accounts.get(&id).await?;

            self.accounts
                .upsert(&ServiceAccount {
                    id,
                    display_name: declared.display_name.clone(),
                    description: declared.description.clone(),
                    origin: Origin::Managed,
                    // Managed accounts have no owner; config administers them (§11).
                    created_by: None,
                    // Preserved: config declares existence, not creation time.
                    created_at: existing
                        .as_ref()
                        .map_or_else(|| self.clock.now(), |account| account.created_at),
                    // Declared means active: re-adding a retired id must re-enable it (no API can, for managed).
                    disabled_at: None,
                })
                .await?;
            report.accounts = report.accounts.saturating_add(1);
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
            report.channels = report.channels.saturating_add(1);
        }

        let departed_channels: Vec<Channel> = self
            .channels
            .list_managed()
            .await?
            .into_iter()
            .filter(|channel| !declared.contains(&channel.id))
            .collect();
        let departed_accounts: Vec<ServiceAccount> = self
            .accounts
            .list_managed()
            .await?
            .into_iter()
            // Already disabled is already retired: it is not a removal this pass
            // would make, so it does not count towards the guard either.
            .filter(|account| {
                !declared_accounts.contains(&account.id) && account.disabled_at.is_none()
            })
            .collect();

        let removals = departed_channels
            .len()
            .saturating_add(departed_accounts.len());
        let guard = config.reconcile;
        let refused = removals > usize::try_from(guard.max_removals).unwrap_or(usize::MAX)
            && !guard.allow_mass_removal;

        if refused {
            tracing::error!(
                channels = departed_channels.len(),
                accounts = departed_accounts.len(),
                max_removals = guard.max_removals,
                "refusing to remove this many managed entities in one pass; the rest of \
                 the file was applied. Check the configuration is complete, or set \
                 [reconcile] allow_mass_removal = true if this is intended"
            );
            self.metrics.reconcile_removals_refused();
            report.refused = removals;
        } else {
            let dropped = self.drop_departed(config, departed_channels).await?;
            let retired = self.retire_departed_accounts(departed_accounts).await?;
            report.removed = dropped.saturating_add(retired);
        }

        // Journal after the writes: better applied-but-unannounced (re-diffed next pass) than the reverse.
        report.journalled = self.journal(config, refused).await?;

        Ok(report)
    }

    /// Whether this configuration is the one the last pass recorded; `false` means stand aside on the timer.
    pub(crate) async fn is_current(&self, config: &Config) -> anyhow::Result<bool> {
        for (kind, declared) in [
            (DeclarationKind::TrustPolicy, trust_policies(config)?),
            (DeclarationKind::Channel, managed_channels(config)?),
            (DeclarationKind::ServiceAccount, managed_accounts(config)?),
        ] {
            let before = self.declarations.snapshot(kind).await?;
            if !notify_ports::declarations::diff(&before, &declared).is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Write what the file changed since the last pass into the audit log (§13); zero when steady.
    /// With `removals_refused`, removals are neither journalled nor dropped from the snapshot,
    /// so the next pass still sees them pending.
    async fn journal(&self, config: &Config, removals_refused: bool) -> anyhow::Result<usize> {
        let at = self.clock.now();
        let mut written = 0_usize;

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
                // `None`: `retire_departed_accounts` journals departed accounts itself, with key counts.
                Vocabulary {
                    added: actions::MANAGED_ACCOUNT_ADDED,
                    changed: actions::MANAGED_ACCOUNT_CHANGED,
                    removed: None,
                },
            ),
        ] {
            let mut declarations = declarations?;
            let before = self.declarations.snapshot(kind).await?;
            let changes = notify_ports::declarations::diff(&before, &declarations);
            let held = removals_refused && kind != DeclarationKind::TrustPolicy;

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
                written = written.saturating_add(1);
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
                written = written.saturating_add(1);
            }
            for removed in &changes.removed {
                if held {
                    declarations.push(removed.clone());
                    continue;
                }
                if let Some(action) = vocabulary.removed {
                    self.record(&action, kind, &removed.key, &removed.snapshot, None, at)
                        .await?;
                    written = written.saturating_add(1);
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
                // Only channel entries name a channel, for the console's per-channel filter.
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
        // Config owns its shape; the API must refuse to reshape it (§11).
        channel.origin = Origin::Managed;
        channel.policy = ChannelPolicy {
            ttl: TtlPolicy {
                default: managed.default_ttl.unwrap_or(TtlPolicy::default().default),
                max: managed.max_ttl,
                allow_forever: TtlPolicy::default().allow_forever,
            },
            // Absent means the published default (one week, §7.2bis), not "never".
            close_after: managed
                .close_after
                .unwrap_or(ChannelPolicy::default().close_after),
            default_priority: managed
                .default_priority
                .unwrap_or(ChannelPolicy::default().default_priority),
            receipts_enabled: managed.receipts,
            // `limits.publish_per_min` (§8.2, §11); `None` keeps the adapter's default.
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

        // These need a user row; `None` means not logged in yet.
        let found = if let Some(subject) = &access.oidc_sub {
            self.users.find_by_subject(subject).await?
        } else if let Some(email) = &access.email {
            match self.users.find_by_email(email).await? {
                EmailMatch::One(user) => Some(*user),
                EmailMatch::Nobody => None,
                // Granting either would hand a channel to whichever row came back
                // first. Nobody is bound, and nobody is reported as pending either:
                // logging in will not fix this, an operator has to.
                EmailMatch::Ambiguous => {
                    tracing::error!(
                        channel = %channel.id,
                        email = %email,
                        "email binding skipped: several users have this address; \
                         bind by `oidc_sub` instead"
                    );
                    return Ok(());
                }
            }
        } else {
            // The config validator refuses a binding naming nothing.
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
        departed: Vec<Channel>,
    ) -> anyhow::Result<usize> {
        let mut removed = 0_usize;
        for channel in &departed {
            match config.channels.on_removed {
                OnRemoved::Delete => {
                    self.channels.delete(&channel.id).await?;
                    tracing::warn!(channel = %channel.id, "deleted: it left the configuration");
                }
                OnRemoved::Archive => {
                    // Handed back to the API rather than deleted, keeping history (the default, §11).
                    let mut archived = channel.clone();
                    archived.origin = Origin::Dynamic;
                    self.channels.upsert(&archived).await?;
                    tracing::warn!(
                        channel = %channel.id,
                        "archived: it left the configuration, so it is no longer managed"
                    );
                }
            }
            removed = removed.saturating_add(1);
        }

        Ok(removed)
    }

    /// Disable managed service accounts that left the configuration and revoke their keys.
    /// Disabled rather than made dynamic (a stray is usually an id change); already-disabled ones are skipped.
    async fn retire_departed_accounts(
        &self,
        departed: Vec<ServiceAccount>,
    ) -> anyhow::Result<usize> {
        let mut retired = 0_usize;

        for mut account in departed {
            let now = self.clock.now();
            // Keys first: `disabled_at` marks "retired", so a failure between calls is retried next pass.
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
            retired = retired.saturating_add(1);
        }

        Ok(retired)
    }
}

#[cfg(test)]
mod tests {
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
        // `[[trust_policy]]` edits must leave a trace in the log.
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

        // Steady passes must write nothing, or the log is buried.
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

        // The entry must carry both before and after.
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

        // Whole-block snapshot: a widened `scopes` must not compare equal.
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

        // Withdrawal: the half that matters most.
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

        // All three kinds are journalled.
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

        // Only channel entries carry the channel field.
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
        assert_eq!(report.pending, [] as [std::string::String; 0]);

        let stored = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(stored.grants(&carol.principal(), Scope::Subscribe));
    }

    #[tokio::test]
    async fn an_email_two_users_share_binds_neither() {
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

        // Same address, different case: the comparison is case-insensitive, so
        // this is one address held by two people.
        let mut both = Vec::new();
        for (subject, email) in [
            ("carol", "carol@weebo.local"),
            ("mallory", "Carol@weebo.local"),
        ] {
            let user =
                User::from_claims(UserId::generate(), subject, Some(email), None, clock.now())
                    .unwrap();
            UserRepository::upsert(&store, &user).await.unwrap();
            both.push(user);
        }

        reconciler(&store, &clock).execute(&declared).await.unwrap();

        let stored = ChannelRepository::get(&store, &ChannelId::parse("ci-infra").unwrap())
            .await
            .unwrap()
            .unwrap();
        for user in &both {
            assert!(!stored.grants(&user.principal(), Scope::Subscribe));
        }
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

        // No longer named: this instance must stop trusting it.
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
    async fn a_replica_holding_an_older_file_learns_it_is_not_current() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let old = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            ..ManagedChannel::default()
        }]);
        let new = config(vec![ManagedChannel {
            id: "ci-deploy".to_owned(),
            ..ManagedChannel::default()
        }]);

        reconciler(&store, &clock).execute(&old).await.unwrap();
        assert!(reconciler(&store, &clock).is_current(&old).await.unwrap());

        // A newer replica booted and applied its file.
        reconciler(&store, &clock).execute(&new).await.unwrap();
        assert!(
            !reconciler(&store, &clock).is_current(&old).await.unwrap(),
            "the old replica's timer must stand aside rather than archive `ci-deploy`"
        );
        assert!(reconciler(&store, &clock).is_current(&new).await.unwrap());
    }

    #[tokio::test]
    async fn a_configured_publish_limit_reaches_the_channel_policy() {
        // The parsed limit must reach the policy the publish path reads (§8.2, §11).
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

    // --- The blast-radius guard ------------------------------------------------

    fn channels(count: usize) -> Vec<ManagedChannel> {
        (0..count)
            .map(|n| ManagedChannel {
                id: format!("ch-{n}"),
                ..ManagedChannel::default()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_truncated_file_does_not_take_every_channel_with_it() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let metrics = Arc::new(notify_metrics_prometheus::PrometheusMetrics::new());
        let reconciler = reconciler(&store, &clock).with_metrics(metrics.clone());

        let mut full = config(channels(8));
        full.channels.on_removed = OnRemoved::Delete;
        reconciler.execute(&full).await.unwrap();

        // The file lost seven of its eight channels — and gained one.
        let mut truncated = config(vec![
            ManagedChannel {
                id: "ch-0".to_owned(),
                ..ManagedChannel::default()
            },
            ManagedChannel {
                id: "brand-new".to_owned(),
                ..ManagedChannel::default()
            },
        ]);
        truncated.channels.on_removed = OnRemoved::Delete;
        let report = reconciler.execute(&truncated).await.unwrap();

        assert_eq!(report.removed, 0, "the destructive half is refused");
        assert_eq!(report.refused, 7);
        assert_eq!(
            ChannelRepository::list_managed(&store).await.unwrap().len(),
            9,
            "nothing deleted, and the rest of the file was still applied"
        );
        assert!(
            notify_ports::Metrics::render(metrics.as_ref())
                .contains("reconcile_removals_refused_total 1")
        );
        assert!(
            !journal(&store)
                .await
                .iter()
                .any(|entry| entry.action.as_str() == "managed.channel.removed"),
            "a removal that did not happen is not journalled"
        );

        // The operator means it.
        truncated.reconcile.allow_mass_removal = true;
        let report = reconciler.execute(&truncated).await.unwrap();
        assert_eq!(report.removed, 7);
        assert_eq!(report.refused, 0);
    }

    #[tokio::test]
    async fn a_few_removals_stay_under_the_guard() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let reconciler = reconciler(&store, &clock);

        reconciler.execute(&config(channels(4))).await.unwrap();
        let report = reconciler.execute(&config(channels(1))).await.unwrap();

        assert_eq!(report.removed, 3, "renaming a few channels is ordinary");
        assert_eq!(report.refused, 0);
    }

    #[tokio::test]
    async fn a_webhook_url_is_never_journalled_in_full() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let mut declared = config(vec![ManagedChannel {
            id: "ci-infra".to_owned(),
            sinks: vec![crate::config::Sink {
                kind: "webhook".to_owned(),
                url: Some("https://hooks.slack.com/services/T0/B0/SECRETPATH".to_owned()),
                secret_ref: Some("env:WNK_SINK_HMAC".to_owned()),
                ..crate::config::Sink::default()
            }],
            ..ManagedChannel::default()
        }]);

        declared.sinks.fingerprint_key = "reconcile-test-fingerprint-key-0123456789".into();

        reconciler(&store, &clock).execute(&declared).await.unwrap();

        let entries = journal(&store).await;
        let detail = serde_json::to_string(&entries[0].detail).unwrap();
        assert!(!detail.contains("SECRETPATH"), "{detail}");
        // The keyed fingerprint, the same one the sink id carries.
        let keyed = notify_ports::redact_url(
            &url::Url::parse("https://hooks.slack.com/services/T0/B0/SECRETPATH").unwrap(),
            &declared.sinks.fingerprint_key().unwrap(),
        );
        assert!(detail.contains(&keyed), "{detail}");
    }
}
