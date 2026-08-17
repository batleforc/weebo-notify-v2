//! Which sinks this instance routes to, and what talks to them (design §7.1bis,
//! §11).
//!
//! Two things live here and nowhere else, for the same reason the push registry
//! does: they are properties of *what was configured*, and nothing above the
//! composition root is allowed to know that.
//!
//! * [`SinkRoutes`] — the channel-to-sink table, built from
//!   `[[channels.managed.sinks]]` and holding the resolved secrets in memory. It is
//!   the reason a queue row can name a sink without carrying its credential.
//! * [`SinkFleet`] — a [`SinkDispatcher`] that picks the adapter by kind. Same shape
//!   as `TransportRegistry`, same answer when a kind is not configured: name the
//!   capability rather than fail vaguely.
//!
//! # A secret that cannot be read is a startup failure
//!
//! `secret_ref = "env:WNK_SINK_INCIDENTS_HMAC"` with nothing in the environment
//! stops the process, exactly as an unreadable VAPID key does (M3.5). The
//! alternative — start, and sign every delivery with an empty secret — produces a
//! consumer rejecting every request for a reason neither side can see.

use std::collections::BTreeMap;
use std::sync::Arc;

use notify_domain::{ChannelId, SinkId};
use notify_ports::{
    PortError, PortResult, SinkDelivery, SinkDispatcher, SinkOutcome, SinkRegistry, SinkTarget,
};

use crate::config::{Config, resolve_secret};

/// The configured routing table (design §11).
#[derive(Clone, Default)]
pub(crate) struct SinkRoutes {
    /// Channel to its sinks, in configuration order — which is the order a
    /// publish queues them in, so an operator reading the file reads the fan-out.
    routes: BTreeMap<ChannelId, Vec<(SinkId, SinkTarget)>>,
}

impl core::fmt::Debug for SinkRoutes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never derived: a target holds an HMAC secret, and a `{:?}` of the wiring
        // is exactly the kind of line that ends up in a startup log.
        f.debug_struct("SinkRoutes")
            .field("channels", &self.routes.len())
            .field("sinks", &self.len())
            .finish()
    }
}

impl SinkRoutes {
    /// Build it from the configuration, resolving every secret.
    ///
    /// # Errors
    ///
    /// A sink whose kind, url or address the schema refused (that check lives in
    /// [`crate::config`], next to the raw strings), or a `secret_ref` that cannot be
    /// resolved.
    pub(crate) fn build(config: &Config) -> anyhow::Result<Self> {
        let mut routes: BTreeMap<ChannelId, Vec<(SinkId, SinkTarget)>> = BTreeMap::new();

        for channel in &config.channels.managed {
            let id = ChannelId::parse(&channel.id)?;

            for sink in &channel.sinks {
                let target = match sink.kind.as_str() {
                    "webhook" => SinkTarget::Webhook {
                        url: url::Url::parse(sink.url.as_deref().unwrap_or_default())?,
                        secret: resolve_secret(
                            &format!("`{}`'s webhook secret_ref", channel.id),
                            sink.secret_ref.as_deref().unwrap_or_default(),
                        )?,
                    },
                    "email" => SinkTarget::Email {
                        address: sink.address.clone().unwrap_or_default(),
                    },
                    other => {
                        anyhow::bail!("channel `{}` declares a sink of type `{other}`", channel.id)
                    }
                };

                let sink_id = target.id()?;
                let entry = routes.entry(id.clone()).or_default();

                // Two identical sinks on one channel would share an identity and
                // therefore a queue row, so the second one would silently replace
                // the first's delivery. Refused rather than deduplicated: it is a
                // copy-paste in the configuration, and saying so is the fix.
                anyhow::ensure!(
                    !entry.iter().any(|(existing, _)| *existing == sink_id),
                    "channel `{}` declares `{sink_id}` twice",
                    channel.id
                );
                entry.push((sink_id, target));
            }
        }

        Ok(Self { routes })
    }

    /// How many sinks are configured, across every channel.
    pub(crate) fn len(&self) -> usize {
        self.routes.values().map(Vec::len).sum()
    }

    /// The kinds in use, for the startup log — never the destinations, which is
    /// where the secrets are keyed.
    pub(crate) fn kinds(&self) -> Vec<&str> {
        let mut kinds: Vec<&str> = self
            .routes
            .values()
            .flatten()
            .map(|(id, _)| id.kind())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }
}

impl SinkRegistry for SinkRoutes {
    fn sinks_for(&self, channel: &ChannelId) -> Vec<SinkId> {
        self.routes
            .get(channel)
            .map(|sinks| sinks.iter().map(|(id, _)| id.clone()).collect())
            .unwrap_or_default()
    }

    fn target(&self, channel: &ChannelId, sink: &SinkId) -> Option<SinkTarget> {
        self.routes
            .get(channel)?
            .iter()
            .find(|(id, _)| id == sink)
            .map(|(_, target)| target.clone())
    }

    fn is_empty(&self) -> bool {
        self.routes.values().all(Vec::is_empty)
    }
}

/// The sink adapters this deployment has, keyed by the kind they handle.
#[derive(Clone, Default)]
pub(crate) struct SinkFleet {
    dispatchers: BTreeMap<&'static str, Arc<dyn SinkDispatcher>>,
}

impl core::fmt::Debug for SinkFleet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SinkFleet")
            .field("kinds", &self.dispatchers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SinkFleet {
    /// One with nothing wired.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Register an adapter for every kind it declares it handles.
    pub(crate) fn with(mut self, dispatcher: &Arc<dyn SinkDispatcher>) -> Self {
        for kind in dispatcher.handles() {
            self.dispatchers.insert(kind, Arc::clone(dispatcher));
        }
        self
    }
}

#[async_trait::async_trait]
impl SinkDispatcher for SinkFleet {
    fn handles(&self) -> &'static [&'static str] {
        // What the fleet as a whole handles is decided at runtime, so the static
        // answer is the closed set of kinds the domain knows; the map decides which
        // of them actually resolve.
        notify_domain::SINK_KINDS
    }

    async fn deliver(
        &self,
        delivery: &SinkDelivery,
        target: &SinkTarget,
    ) -> PortResult<SinkOutcome> {
        let Some(dispatcher) = self.dispatchers.get(target.kind()) else {
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
        Config::load(&path).unwrap()
    }

    /// A secret file, and the `file:` reference that names it.
    ///
    /// `file:` rather than `env:` throughout these tests, and not by preference:
    /// setting an environment variable is `unsafe` since the 2024 edition and this
    /// workspace forbids `unsafe` outright. The two branches of the resolver are the
    /// same three lines; the one that can be tested here is tested here.
    fn secret_file(name: &str, contents: &str) -> String {
        let dir = std::env::temp_dir().join(format!("wnk-sinks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        format!("file:{}", path.display())
    }

    fn webhook_config(secret_ref: &str) -> Config {
        config(&format!(
            r#"
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
        // Written with a trailing newline on purpose: that is what `echo > secret`
        // produces, and an HMAC over the newline is a signature nobody can reproduce.
        let reference = secret_file("hmac", "shh\n");
        let routes = SinkRoutes::build(&webhook_config(&reference)).unwrap();
        let channel = ChannelId::parse("ci-infra").unwrap();
        let sink = routes.sinks_for(&channel).into_iter().next().unwrap();

        assert_eq!(sink.kind(), "webhook");
        assert_eq!(sink.destination(), "https://hooks.weebo.poc/incidents");
        assert_eq!(
            routes.target(&channel, &sink),
            Some(SinkTarget::Webhook {
                url: url::Url::parse("https://hooks.weebo.poc/incidents").unwrap(),
                secret: "shh".to_owned(),
            })
        );
        assert!(
            routes
                .sinks_for(&ChannelId::parse("elsewhere").unwrap())
                .is_empty()
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
        // Not "treated as the secret itself": accepting an inline value would make
        // §11's whole reference scheme optional, and the first thing an operator
        // would do with it is commit a secret.
        let error = SinkRoutes::build(&webhook_config("shh"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("env:NAME"), "{error}");
    }

    #[tokio::test]
    async fn a_kind_with_no_adapter_names_the_capability() {
        let fleet = SinkFleet::empty();
        let delivery = SinkDelivery {
            delivery_id: notify_domain::DeliveryId::generate(),
            channel: ChannelId::parse("ci-infra").unwrap(),
            sink: SinkId::new("webhook", "https://hooks.weebo.poc/x").unwrap(),
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
        };

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
