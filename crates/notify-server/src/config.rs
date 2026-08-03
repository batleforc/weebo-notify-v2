//! Configuration: defaults < TOML file < `WNK_*` environment (design §11).
//!
//! **Every key is overridable from the environment, without exception.** The
//! convention is `WNK_` plus the path in `__`, so `[oidc] issuer` becomes
//! `WNK_OIDC__ISSUER`. That is not a nicety: it is what lets one `config/dev.toml`
//! serve Compose *and* an Eclipse Che workspace with no second file, because the
//! only thing that differs between them — a browser-facing URL — is an override
//! (§12.1).
//!
//! # What this build does not act on yet
//!
//! The schema below covers the whole of §11, including sections whose behaviour
//! lands later: trust policies (M4), channel sinks (§7.1bis) and push transport
//! credentials (M3 onwards). They are parsed rather than ignored, and
//! [`Config::unhonoured`] reports them at startup — a configuration key that is
//! silently dropped is how an operator loses an afternoon.

// The whole §11 schema is modelled here, ahead of the wiring that reads it: the
// composition root's `run` is still blocked on group-based channel access (§11
// `oidc_group`), which nothing resolves yet. Parsing and validating the complete
// file now is what makes that blocker the only one left.
#![allow(
    dead_code,
    reason = "the reader of these fields is the composition root, which is blocked"
)]

use std::net::SocketAddr;
use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format as _, Serialized, Toml};
use notify_domain::{HumanDuration, Priority, Scope, Ttl};
use serde::{Deserialize, Serialize};

/// The environment prefix, shared with the clients (design §9.1) so one habit
/// covers both sides.
const ENV_PREFIX: &str = "WNK_";

/// Everything the server is configured with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub server: Server,
    pub database: Database,
    pub oidc: Oidc,
    pub admin: Admin,
    pub retention: Retention,
    pub ha: Ha,
    pub eventbus: EventBus,
    pub channels: Channels,
    pub attachments: Option<Attachments>,

    /// Managed service accounts, reconciled like channels (§5.2bis).
    pub service_accounts: Vec<ManagedServiceAccount>,

    /// Federation trust policies (§5.3). Parsed, not yet honoured — M4.
    pub trust_policy: Vec<TrustPolicy>,

    /// Push transport credentials (§11). Parsed, not yet honoured — M3 onwards.
    pub push: Push,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Server {
    pub bind: SocketAddr,
    /// Closed by default (§5.1): an origin that is not listed cannot call the API
    /// from a browser.
    pub cors_origins: Vec<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 8080)),
            cors_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Database {
    pub url: String,
    /// Pool size. Leaves headroom on purpose: every background job that leads holds
    /// one connection open for as long as it runs (the advisory lock *is* that
    /// connection), so a pool sized exactly to the request concurrency would
    /// deadlock the first time the purge took the lock.
    pub max_connections: u32,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            url: "postgres://weebo:weebo@localhost:5432/weebo_notify".to_owned(),
            max_connections: 16,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Oidc {
    pub issuer: String,
    pub client_id: String,
    /// Extra audiences a token may carry. The client id is always accepted; this is
    /// for issuers whose access token names the *API* rather than the client, and
    /// for an instance migrating between the two.
    pub audiences: Vec<String>,
    /// Which claim carries group membership. There is no standard one: Authentik
    /// says `groups`, Keycloak says whatever the mapper was called, and Dex says
    /// nothing at all (T29).
    pub groups_claim: String,
}

impl Default for Oidc {
    fn default() -> Self {
        Self {
            issuer: "http://localhost:9110".to_owned(),
            client_id: "weebo-notify".to_owned(),
            audiences: Vec::new(),
            groups_claim: "groups".to_owned(),
        }
    }
}

/// The platform role (§5.4). GitOps only — no API path can grant it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Admin {
    pub oidc_groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Retention {
    pub default_ttl: Ttl,
    pub max_ttl: Option<HumanDuration>,
    /// Whether a channel may keep notifications forever. Off by default: unbounded
    /// retention is a deliberate operator decision (§7.2bis).
    pub allow_forever: bool,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            #[allow(
                clippy::expect_used,
                reason = "a literal that fails to parse is a compile-time mistake, \
                          caught by this module's own tests"
            )]
            default_ttl: Ttl::parse("7d", "retention.default_ttl").expect("a valid literal"),
            max_ttl: None,
            allow_forever: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaderBackend {
    /// A Postgres advisory lock. Zero extra infrastructure.
    #[default]
    Postgres,
    /// The Kubernetes Lease API. Not implemented yet — refused at startup rather
    /// than silently downgraded, because an operator who asked for it would
    /// otherwise get two leaders across a cluster and never know.
    Kubernetes,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Ha {
    pub leader_election: LeaderBackend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BusBackend {
    /// `LISTEN/NOTIFY` (ADR-0008).
    #[default]
    Postgres,
    /// Redis pub/sub, for when the NOTIFY per transaction starts to weigh. Not
    /// implemented yet; refused at startup for the same reason as above.
    Redis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct EventBus {
    pub backend: BusBackend,
    pub redis_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Attachments {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

/// What happens to a managed channel that disappears from the config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnRemoved {
    /// Leave the rows in place and stop reconciling it. The safe default: deleting
    /// a channel takes its history with it, and a config typo should not.
    #[default]
    Archive,
    /// Delete it, history included.
    Delete,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Channels {
    pub on_removed: OnRemoved,
    /// `sighup` — reload without a restart. Recorded for §11; the handler lands
    /// with the reconciler's hot path.
    pub reload: Option<String>,
    pub managed: Vec<ManagedChannel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ManagedChannel {
    pub id: String,
    pub name: Option<String>,
    pub default_ttl: Option<Ttl>,
    pub max_ttl: Option<HumanDuration>,
    pub default_priority: Option<Priority>,
    pub receipts: bool,
    pub limits: Option<ChannelLimits>,
    pub access: Vec<Access>,
    /// Channel-scoped federation publishers (§5.3). Parsed, not yet honoured — M4.
    pub publishers: Vec<Publisher>,
    /// Outbound sinks (§7.1bis). Parsed, not yet honoured.
    pub sinks: Vec<Sink>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ChannelLimits {
    pub publish_per_min: Option<u32>,
}

/// One access binding: exactly one subject, plus the scopes it grants.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Access {
    pub oidc_group: Option<String>,
    pub oidc_sub: Option<String>,
    pub email: Option<String>,
    pub service_account: Option<String>,
    pub scopes: Vec<Scope>,
}

impl Access {
    /// How many subjects this binding names. Anything but one is a config mistake.
    pub(crate) fn subjects(&self) -> usize {
        usize::from(self.oidc_group.is_some())
            + usize::from(self.oidc_sub.is_some())
            + usize::from(self.email.is_some())
            + usize::from(self.service_account.is_some())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Publisher {
    pub issuer: String,
    pub subject: String,
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Sink {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: Option<String>,
    pub secret_ref: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ManagedServiceAccount {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TrustPolicy {
    pub issuer: String,
    pub subject: String,
    pub channels: Vec<String>,
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Push {
    pub webpush: Option<serde_json::Value>,
    pub apns: Option<serde_json::Value>,
    pub unifiedpush: Option<serde_json::Value>,
}

impl Config {
    /// Read the layered configuration.
    ///
    /// A missing file is not an error: every key has a default and every key is
    /// overridable from the environment, so a container with nothing but `WNK_*`
    /// variables is a supported deployment.
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        if path.exists() {
            figment = figment.merge(Toml::file(path));
        } else {
            tracing::warn!(path = %path.display(), "no configuration file; using defaults and WNK_*");
        }

        // `split("__")` is what makes `WNK_OIDC__ISSUER` reach `[oidc] issuer`.
        // Every key, without exception — the rule §11 states.
        let config: Self = figment
            .merge(Env::prefixed(ENV_PREFIX).split("__"))
            .extract()?;

        config.validate()?;
        Ok(config)
    }

    /// Refuse a configuration this build cannot honour.
    ///
    /// Refused at startup rather than downgraded: an operator who asked for the
    /// Kubernetes lease and silently got an advisory lock would run two leaders
    /// across a cluster and never be told.
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.ha.leader_election == LeaderBackend::Postgres,
            "[ha] leader_election = \"kubernetes\" is not implemented yet; \
             use \"postgres\" (advisory lock) or pin an older release"
        );
        anyhow::ensure!(
            self.eventbus.backend == BusBackend::Postgres,
            "[eventbus] backend = \"redis\" is not implemented yet; \
             use \"postgres\" (LISTEN/NOTIFY)"
        );

        for channel in &self.channels.managed {
            anyhow::ensure!(
                !channel.id.is_empty(),
                "a [[channels.managed]] entry has no id"
            );
            for access in &channel.access {
                anyhow::ensure!(
                    access.subjects() == 1,
                    "[[channels.managed.access]] in `{}` must name exactly one of \
                     oidc_group, oidc_sub, email or service_account",
                    channel.id
                );
                anyhow::ensure!(
                    !access.scopes.is_empty(),
                    "an access binding in `{}` grants no scopes, which grants nothing",
                    channel.id
                );
            }
        }

        Ok(())
    }

    /// Sections this build parses but does not act on, for the startup log.
    pub(crate) fn unhonoured(&self) -> Vec<String> {
        let mut pending = Vec::new();

        if !self.trust_policy.is_empty() {
            pending.push(format!(
                "{} [[trust_policy]] entries — JWT federation lands in M4 (§5.3)",
                self.trust_policy.len()
            ));
        }

        let publishers: usize = self
            .channels
            .managed
            .iter()
            .map(|channel| channel.publishers.len())
            .sum();
        if publishers > 0 {
            pending.push(format!(
                "{publishers} [[channels.managed.publishers]] entries — M4 (§5.3)"
            ));
        }

        let sinks: usize = self
            .channels
            .managed
            .iter()
            .map(|channel| channel.sinks.len())
            .sum();
        if sinks > 0 {
            pending.push(format!(
                "{sinks} [[channels.managed.sinks]] entries — outbound sinks (§7.1bis)"
            ));
        }

        if self.push.webpush.is_some()
            || self.push.apns.is_some()
            || self.push.unifiedpush.is_some()
        {
            pending
                .push("[push.*] credentials — no push transport is wired in M1 (§6.2)".to_owned());
        }

        if self.attachments.is_some() {
            pending.push("[attachments] — the object store adapter lands with M2".to_owned());
        }

        pending
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    /// Write a config file somewhere only this test will look.
    fn with_config(test: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wnk-server-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn the_defaults_are_a_working_configuration() {
        let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();

        assert_eq!(config.server.bind.port(), 8080);
        assert!(
            config.server.cors_origins.is_empty(),
            "closed by default (§5.1): an unlisted origin cannot call the API"
        );
        assert_eq!(config.retention.default_ttl, Ttl::parse("7d", "t").unwrap());
        assert!(!config.retention.allow_forever);
    }

    #[test]
    fn the_committed_dev_configuration_parses() {
        // The one config file this repository ships. If it stops parsing, `task
        // dev:run` stops working, and finding that out at startup is worse than
        // finding it out here.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml");
        let config = Config::load(&path).expect("config/dev.toml parses");

        assert_eq!(config.oidc.client_id, "weebo-notify");
        assert_eq!(config.admin.oidc_groups, ["platform-admins"]);
        assert_eq!(config.channels.managed.len(), 2);
        assert_eq!(config.service_accounts.len(), 1);

        let incidents = &config.channels.managed[0];
        assert_eq!(incidents.id, "cluster-ovh-1");
        assert!(incidents.receipts);
        assert_eq!(incidents.access.len(), 3);
        assert_eq!(incidents.default_priority, Some(Priority::High));
    }

    #[test]
    fn a_kubernetes_lease_is_refused_rather_than_silently_downgraded() {
        let path = with_config("k8s", "[ha]\nleader_election = \"kubernetes\"\n");

        let error = Config::load(&path).unwrap_err().to_string();

        assert!(
            error.contains("not implemented"),
            "two leaders across a cluster is the failure this prevents: {error}"
        );
    }

    #[test]
    fn an_access_binding_must_name_exactly_one_subject() {
        let both = with_config(
            "both",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             oidc_group = \"dev-team\"\n\
             email = \"alice@weebo.local\"\n\
             scopes = [\"subscribe\"]\n",
        );
        assert!(Config::load(&both).is_err());

        let neither = with_config(
            "neither",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             scopes = [\"subscribe\"]\n",
        );
        assert!(Config::load(&neither).is_err());
    }

    #[test]
    fn a_binding_that_grants_nothing_is_a_mistake_worth_reporting() {
        let path = with_config(
            "scopeless",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             oidc_group = \"dev-team\"\n\
             scopes = []\n",
        );

        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn what_this_build_parses_but_ignores_is_reported_rather_than_dropped() {
        let path = with_config(
            "pending",
            "[[trust_policy]]\n\
             issuer = \"https://forgejo.weebo.poc\"\n\
             subject = \"repo:weebo/*\"\n\
             scopes = [\"publish\"]\n\
             \n\
             [[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n",
        );

        let pending = Config::load(&path).unwrap().unhonoured();

        assert_eq!(
            pending.len(),
            2,
            "a silently ignored key costs an afternoon"
        );
        assert!(pending.iter().any(|line| line.contains("trust_policy")));
        assert!(pending.iter().any(|line| line.contains("sinks")));
    }
}
