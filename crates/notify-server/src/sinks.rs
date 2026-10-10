//! Which sinks this instance routes to (design §7.1bis, §11): [`SinkRoutes`] (channel → sinks, secrets
//! in memory) and [`SinkFleet`] (adapter by kind). Both are shared by clone and swapped in place on
//! `SIGHUP` (`adopt`), like `notify_app::LivePolicies`. An unresolvable `secret_ref` fails startup.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use notify_domain::{ChannelId, SinkId};
use notify_ports::{
    PortError, PortResult, SinkDelivery, SinkDispatcher, SinkOutcome, SinkRegistry, SinkTarget,
};

use crate::config::{Config, resolve_secret};

/// Channel to its sinks, in configuration order — which is the order a publish
/// queues them in, so an operator reading the file reads the fan-out.
type Table = BTreeMap<ChannelId, Vec<(SinkId, SinkTarget)>>;

/// The configured routing table (design §11), shared and swappable.
#[derive(Clone, Default)]
pub(crate) struct SinkRoutes {
    routes: Arc<RwLock<Arc<Table>>>,
}

impl core::fmt::Debug for SinkRoutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Not derived: targets hold HMAC secrets.
        f.debug_struct("SinkRoutes")
            .field("channels", &self.table().len())
            .field("sinks", &self.len())
            .finish()
    }
}

impl SinkRoutes {
    /// A sink the schema refused (see [`crate::config`]), or an unresolvable `secret_ref`.
    pub(crate) fn build(config: &Config) -> anyhow::Result<Self> {
        let mut routes: BTreeMap<ChannelId, Vec<(SinkId, SinkTarget)>> = BTreeMap::new();
        // Only a webhook is fingerprinted, so an instance with none needs no key —
        // which is also all `Config::validate` demands.
        let mut key = None;

        for channel in &config.channels.managed {
            let id = ChannelId::parse(&channel.id)?;

            for sink in &channel.sinks {
                let (sink_id, target) = match sink.kind.as_str() {
                    "webhook" => {
                        let target = SinkTarget::Webhook {
                            url: url::Url::parse(sink.url.as_deref().unwrap_or_default())?,
                            secret: resolve_secret(
                                &format!("`{}`'s webhook secret_ref", channel.id),
                                sink.secret_ref.as_deref().unwrap_or_default(),
                            )?,
                        };
                        let key = match &mut key {
                            Some(key) => key,
                            None => key.insert(config.sinks.fingerprint_key()?),
                        };
                        (target.id(key)?, target)
                    }
                    "email" => {
                        let address = sink.address.clone().unwrap_or_default();
                        (
                            SinkId::new("email", &address)?,
                            SinkTarget::Email { address },
                        )
                    }
                    other => {
                        anyhow::bail!("channel `{}` declares a sink of type `{other}`", channel.id)
                    }
                };

                let entry = routes.entry(id.clone()).or_default();

                // Identical sinks would share a queue row; refuse the copy-paste.
                anyhow::ensure!(
                    !entry.iter().any(|(existing, _)| *existing == sink_id),
                    "channel `{}` declares `{sink_id}` twice",
                    channel.id
                );
                entry.push((sink_id, target));
            }
        }

        Ok(Self {
            routes: Arc::new(RwLock::new(Arc::new(routes))),
        })
    }

    /// Replace what every holder of this table routes by with `other`'s routes —
    /// what a `SIGHUP` does.
    ///
    /// Whole-table replacement, never a merge: a sink withdrawn from the file has to
    /// stop receiving new deliveries, which is the half a merge would lose.
    pub(crate) fn adopt(&self, other: &Self) {
        let table = other.table();
        // Written through a poisoned lock for the reason `LivePolicies` gives: the
        // alternative keeps a withdrawn sink receiving for the life of the process.
        match self.routes.write() {
            Ok(mut slot) => *slot = table,
            Err(poisoned) => *poisoned.into_inner() = table,
        }
    }

    /// The table in force right now. Cloned out (an `Arc`), so no caller holds the
    /// lock across anything slower than a lookup.
    fn table(&self) -> Arc<Table> {
        match self.routes.read() {
            Ok(slot) => Arc::clone(&slot),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// How many sinks are configured, across every channel.
    pub(crate) fn len(&self) -> usize {
        self.table().values().map(Vec::len).sum()
    }

    /// The kinds in use, for the startup log (never destinations).
    pub(crate) fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = self
            .table()
            .values()
            .flatten()
            .map(|(_, target)| target.kind())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }
}

impl SinkRegistry for SinkRoutes {
    fn sinks_for(&self, channel: &ChannelId) -> Vec<SinkId> {
        self.table()
            .get(channel)
            .map(|sinks| sinks.iter().map(|(id, _)| id.clone()).collect())
            .unwrap_or_default()
    }

    fn target(&self, channel: &ChannelId, sink: &SinkId) -> Option<SinkTarget> {
        self.table()
            .get(channel)?
            .iter()
            .find(|(id, _)| id == sink)
            .map(|(_, target)| target.clone())
    }

    fn is_empty(&self) -> bool {
        self.table().values().all(Vec::is_empty)
    }
}

/// Move every stored sink id written under an older identity to the one this
/// configuration gives it, and redact the ones nothing configures any more.
///
/// Run once at boot, before the drain. SQL cannot do this — a webhook id is an HMAC
/// under `[sinks] fingerprint_key`, which the database never sees — so the
/// migration leaves queue rows and dead letters as they were and this, which knows
/// both the key and the configured urls, renames them:
///
/// * `webhook:<full url>` — what every build before the redaction stored — and
///   `webhook:<origin>#<hmac under a previous key>` become the current id of the
///   configured webhook they name;
/// * a `webhook:<full url>` no configured webhook matches becomes
///   `webhook:<origin>#unrouted`. It was unroutable already — the drain keeps an
///   unknown sink's deliveries until the retry window closes, then dead-letters
///   them — so nothing is lost but the url, which is the point.
///
/// An id under a previous key whose webhook is no longer configured is left alone:
/// it is keyed, so it says nothing, and it dead-letters like any withdrawn sink.
///
/// # Errors
///
/// A previous key that is not one, or the store failing.
pub(crate) async fn rekey_stored(
    config: &Config,
    routes: &SinkRoutes,
    queue: &dyn notify_ports::SinkQueue,
) -> anyhow::Result<u64> {
    let previous = config.sinks.previous_fingerprint_keys()?;
    let mut renames = std::collections::HashMap::new();
    for (id, target) in routes.table().values().flatten() {
        let SinkTarget::Webhook { url, .. } = target else {
            continue;
        };
        renames.insert(format!("webhook:{url}"), id.clone());
        for key in &previous {
            renames.insert(
                format!("webhook:{}", notify_ports::redact_url(url, key)),
                id.clone(),
            );
        }
    }

    let rekey = |stored: &str| renames.get(stored).cloned().or_else(|| unrouted(stored));
    Ok(queue.rekey_sinks(&rekey).await?)
}

/// `webhook:<origin>#unrouted` for a stored id that still carries a url's path or
/// query; `None` for anything already redacted, or not a webhook.
fn unrouted(stored: &str) -> Option<SinkId> {
    let url = url::Url::parse(stored.strip_prefix("webhook:")?).ok()?;
    // A redacted id parses too — `https://host#<hex>` is a bare origin with a
    // fragment — and has nothing left to hide.
    if url.path() == "/" && url.query().is_none() {
        return None;
    }
    SinkId::new(
        "webhook",
        &format!("{}#unrouted", &url[..url::Position::BeforePath]),
    )
    .ok()
}

/// The adapters, by the kind they handle.
type Dispatchers = BTreeMap<&'static str, Arc<dyn SinkDispatcher>>;

/// The sink adapters this deployment has, keyed by the kind they handle — shared
/// and swappable like [`SinkRoutes`], because a reload that adds the first email
/// sink also needs the SMTP adapter behind it.
#[derive(Clone, Default)]
pub(crate) struct SinkFleet {
    dispatchers: Arc<RwLock<Dispatchers>>,
}

impl core::fmt::Debug for SinkFleet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SinkFleet")
            .field("kinds", &self.current().keys().copied().collect::<Vec<_>>())
            .finish()
    }
}

impl SinkFleet {
    /// One with nothing wired.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Register an adapter for every kind it declares it handles.
    pub(crate) fn with(self, dispatcher: &Arc<dyn SinkDispatcher>) -> Self {
        {
            let mut map = match self.dispatchers.write() {
                Ok(map) => map,
                Err(poisoned) => poisoned.into_inner(),
            };
            for kind in dispatcher.handles() {
                map.insert(kind, Arc::clone(dispatcher));
            }
        }
        self
    }

    /// Replace the adapters every holder of this fleet uses with `other`'s.
    pub(crate) fn adopt(&self, other: &Self) {
        let replacement = other.current();
        match self.dispatchers.write() {
            Ok(mut map) => *map = replacement,
            Err(poisoned) => *poisoned.into_inner() = replacement,
        }
    }

    fn current(&self) -> Dispatchers {
        match self.dispatchers.read() {
            Ok(map) => map.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[async_trait::async_trait]
impl SinkDispatcher for SinkFleet {
    fn handles(&self) -> &'static [&'static str] {
        // Statically every domain kind; the map decides which resolve.
        notify_domain::SINK_KINDS
    }

    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &SinkTarget,
    ) -> PortResult<SinkOutcome> {
        // Cloned out before the await: a send can take seconds, and a lock held
        // across it would make a reload wait on somebody else's webhook.
        let Some(dispatcher) = self.current().get(target.kind()).cloned() else {
            return Err(PortError::NotConfigured {
                what: capability(target.kind()),
            });
        };

        dispatcher.deliver(delivery, target).await
    }
}

/// The `sinks.<kind>` capability name, as an operator reads it in the log.
const fn capability(kind: &str) -> &'static str {
    match kind.as_bytes() {
        b"webhook" => "sinks.webhook",
        b"email" => "sinks.email",
        _ => "sinks",
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn config(toml: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("wnk-sinks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{:x}.toml", toml.len()));
        std::fs::write(&path, toml).unwrap();
        crate::config::load_isolated(&path).unwrap()
    }

    /// A secret file and its `file:` reference (setting env vars needs `unsafe`, which is forbidden).
    fn secret_file(name: &str, contents: &str) -> String {
        let dir = std::env::temp_dir().join(format!("wnk-sinks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        format!("file:{}", path.display())
    }

    /// The key every config here fingerprints under.
    const KEY: &str = "sinks-test-fingerprint-key-0123456789";

    fn webhook_config(secret_ref: &str) -> Config {
        webhook_config_with(secret_ref, &format!("fingerprint_key = \"{KEY}\""))
    }

    fn webhook_config_with(secret_ref: &str, sinks: &str) -> Config {
        config(&format!(
            r#"
[sinks]
{sinks}

[[channels.managed]]
id = "ci-infra"

  [[channels.managed.sinks]]
  type = "webhook"
  url = "https://hooks.weebo.poc/incidents"
  secret_ref = "{secret_ref}"
"#
        ))
    }

    #[test]
    fn a_configured_webhook_becomes_a_route_carrying_its_resolved_secret() {
        // Trailing newline, as `echo > secret` writes; it must be trimmed.
        let reference = secret_file("hmac", "shh\n");
        let routes = SinkRoutes::build(&webhook_config(&reference)).unwrap();
        let channel = ChannelId::parse("ci-infra").unwrap();
        let sink = routes.sinks_for(&channel).into_iter().next().unwrap();

        assert_eq!(sink.kind(), "webhook");
        assert_eq!(
            sink.destination(),
            notify_ports::redact_url(
                &url::Url::parse("https://hooks.weebo.poc/incidents").unwrap(),
                &notify_ports::FingerprintKey::new(KEY.as_bytes()).unwrap(),
            ),
            "the origin and a fingerprint under the configured key, never the path: {sink}"
        );
        assert_eq!(
            routes.target(&channel, &sink),
            Some(SinkTarget::Webhook {
                url: url::Url::parse("https://hooks.weebo.poc/incidents").unwrap(),
                secret: "shh".to_owned(),
            })
        );
        assert_eq!(
            routes.sinks_for(&ChannelId::parse("elsewhere").unwrap()),
            [] as [notify_domain::SinkId; 0]
        );
    }

    #[test]
    fn a_secret_that_is_not_there_stops_the_process_instead_of_signing_with_nothing() {
        let error = SinkRoutes::build(&webhook_config("file:/nowhere/at/all"))
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("/nowhere/at/all"),
            "an operator is told which file to mount, not that `sinks failed`: {error}"
        );
    }

    #[test]
    fn a_secret_reference_of_a_form_nobody_serves_is_refused() {
        // Inline values are refused, or operators would commit secrets (§11).
        let error = SinkRoutes::build(&webhook_config("shh"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("env:NAME"), "{error}");
    }

    #[test]
    fn a_reload_swaps_the_routes_every_holder_sees() {
        let reference = secret_file("hmac-reload", "shh");
        let live = SinkRoutes::build(&Config::default()).unwrap();
        // What the fan-out and the drain hold: a clone taken at boot.
        let held_by_the_drain = live.clone();
        let channel = ChannelId::parse("ci-infra").unwrap();
        assert_eq!(
            held_by_the_drain.sinks_for(&channel),
            [] as [notify_domain::SinkId; 0]
        );

        live.adopt(&SinkRoutes::build(&webhook_config(&reference)).unwrap());

        assert_eq!(
            held_by_the_drain.sinks_for(&channel).len(),
            1,
            "a webhook added by SIGHUP is routed without a restart"
        );

        live.adopt(&SinkRoutes::build(&Config::default()).unwrap());
        assert!(
            held_by_the_drain.is_empty(),
            "and one removed by SIGHUP stops receiving"
        );
    }

    /// A delivery to `sink` on `ci-infra`, of an event nobody reads.
    fn delivery_to(sink: SinkId) -> SinkDelivery {
        SinkDelivery {
            delivery_id: notify_domain::DeliveryId::generate(),
            channel: ChannelId::parse("ci-infra").unwrap(),
            sink,
            event: notify_ports::Event {
                cursor: notify_domain::Cursor::from_sequence(1),
                audience: notify_ports::Audience::Channel {
                    id: ChannelId::parse("ci-infra").unwrap(),
                },
                recorded_at: time::OffsetDateTime::UNIX_EPOCH,
                muted_by_dnd: false,
                payload: notify_ports::EventPayload::Tombstone {
                    notification_id: notify_domain::NotificationId::generate(),
                    reason: notify_domain::TombstoneReason::Expired,
                },
            },
            attempts: 0,
            first_failed_at: None,
        }
    }

    #[tokio::test]
    async fn stored_ids_are_rekeyed_at_boot_and_unconfigured_urls_redacted() {
        use notify_ports::SinkQueue as _;

        let reference = secret_file("hmac-rekey", "shh");
        let old_key = "the-previous-fingerprint-key-0123456789";
        let config = webhook_config_with(
            &reference,
            &format!("fingerprint_key = \"{KEY}\"\nprevious_fingerprint_keys = [\"{old_key}\"]"),
        );
        let routes = SinkRoutes::build(&config).unwrap();
        let channel = ChannelId::parse("ci-infra").unwrap();
        let current = routes.sinks_for(&channel).remove(0);
        let url = url::Url::parse("https://hooks.weebo.poc/incidents").unwrap();
        let under_old_key = notify_ports::redact_url(
            &url,
            &notify_ports::FingerprintKey::new(old_key.as_bytes()).unwrap(),
        );

        let store = notify_ports::test_utils::InMemory::new();
        let at = time::OffsetDateTime::UNIX_EPOCH;
        // What an older build queued: the url itself.
        let legacy = delivery_to(SinkId::new("webhook", url.as_str()).unwrap());
        // What this build queued before the key was rotated — dead by now.
        let rotated = delivery_to(SinkId::new("webhook", &under_old_key).unwrap());
        // A webhook since removed from the file, credential in its path.
        let gone = delivery_to(
            SinkId::new(
                "webhook",
                "https://hooks.slack.com/services/T0/B0/SECRETPATH",
            )
            .unwrap(),
        );
        for delivery in [&legacy, &rotated, &gone] {
            store.enqueue(delivery, at).await.unwrap();
        }
        store.bury(&rotated, "410", at).await.unwrap();

        let moved = rekey_stored(&config, &routes, &store).await.unwrap();
        assert_eq!(moved, 3);
        assert_eq!(
            rekey_stored(&config, &routes, &store).await.unwrap(),
            0,
            "a second boot finds nothing to move"
        );

        let queued = store.claim_due(at, 10, at).await.unwrap();
        let sink_of = |id| {
            queued
                .iter()
                .find(|held| held.delivery_id == id)
                .map(|held| held.sink.to_string())
        };
        assert_eq!(sink_of(legacy.delivery_id), Some(current.to_string()));
        assert_eq!(
            sink_of(gone.delivery_id).as_deref(),
            Some("webhook:https://hooks.slack.com#unrouted")
        );
        let letters = store.dead_letters(None).await.unwrap();
        assert_eq!(letters[0].sink, current, "dead letters are re-keyed too");
    }

    #[tokio::test]
    async fn a_kind_with_no_adapter_names_the_capability() {
        let fleet = SinkFleet::empty();
        let delivery = delivery_to(
            SinkId::new("webhook", "https://hooks.weebo.poc#0000000000000000").unwrap(),
        );

        let error = fleet
            .deliver(
                &delivery,
                &SinkTarget::Webhook {
                    url: url::Url::parse("https://hooks.weebo.poc/x").unwrap(),
                    secret: "shh".to_owned(),
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            PortError::NotConfigured {
                what: "sinks.webhook"
            }
        ));
    }
}
