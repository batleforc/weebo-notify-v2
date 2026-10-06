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
//! lands later: trust policies (M4), channel sinks (§7.1bis) and the relayed push
//! credentials (M6 and M7). They are parsed rather than ignored, and
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
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format as _, Serialized, Toml};
use notify_domain::{CloseAfter, HumanDuration, Priority, Scope, Ttl};
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

    /// Federation trust policies (§5.3).
    pub trust_policy: Vec<TrustPolicy>,

    /// `[federation]` — where the Ed25519 signing key lives (§5.3, §11).
    ///
    /// Absent means this deployment does not federate: `POST /auth/token/exchange`
    /// answers `501` and no CI token is accepted. Present with no readable key is a
    /// startup failure, not a silent downgrade — the same rule `[push.webpush]` set
    /// in M3.5.
    pub federation: Option<Federation>,

    /// Push transport credentials (§11). Parsed, not yet honoured — M3 onwards.
    pub push: Push,

    /// `[sinks]` — what the outbound sinks of §7.1bis need beyond their channel.
    pub sinks: Sinks,

    /// `[telemetry]` — where the traces of §13 go, if anywhere.
    ///
    /// Absent means this deployment emits no spans, which is the default and not a
    /// degradation: a collector endpoint is a fact about a deployment, and a default
    /// pointing at `localhost:4318` would make every instance without a collector
    /// retry an export it was never asked for.
    pub telemetry: Option<Telemetry>,

    /// `[cli]` — serving the `notify` CLI binary for download (design §9).
    ///
    /// Absent means this deployment does not serve it: `GET /api/v1/cli` answers
    /// `501` rather than guessing where a binary might live on this filesystem.
    /// Present with an unreadable path is a startup failure, the same rule
    /// `[federation]`/`[push.webpush]` already set — an operator who asked for
    /// this and got a silent 501 would lose an afternoon to it.
    pub cli: Option<Cli>,
}

/// `[cli]` — where the built `notify` binary lives (design §9).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Cli {
    /// A path on this filesystem, not the bytes themselves — the Containerfile
    /// copies the binary here, alongside `notify-server`, built from the same
    /// commit so the two can never disagree on version.
    pub binary_path: PathBuf,
}

/// Where the OTLP exporter sends, and what it calls this process (design §13).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Telemetry {
    /// The collector's OTLP **HTTP** endpoint, e.g. `http://collector:4318`.
    ///
    /// The `/v1/traces` path is appended by the exporter, so this is the base URL a
    /// collector documents rather than the one an operator has to assemble.
    pub otlp_endpoint: String,

    /// What the traces are attributed to. One name per deployment, not per replica:
    /// `service.instance.id` is what tells replicas apart, and putting the pod name
    /// here would make a trace search per-pod.
    pub service_name: String,

    /// Fraction of root traces recorded, 0.0 to 1.0.
    ///
    /// Sampling belongs here rather than at the collector for the reason every
    /// sampling decision is taken early: a span that was never recorded costs
    /// nothing to export, and one recorded to be dropped downstream has already
    /// been paid for. Children follow their root — a trace missing its middle is
    /// worse than no trace.
    pub sample_ratio: f64,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            // No default endpoint on purpose: the section only exists when somebody
            // wrote it, and an empty endpoint is refused at startup rather than
            // exporting into the void.
            otlp_endpoint: String::new(),
            service_name: "weebo-notify".to_owned(),
            sample_ratio: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Server {
    pub bind: SocketAddr,
    /// Closed by default (§5.1): an origin that is not listed cannot call the API
    /// from a browser.
    pub cors_origins: Vec<String>,
    /// Whether `/api/docs` serves the Scalar UI and the OpenAPI document.
    ///
    /// **On by default, and that is the right default**: the API surface of an
    /// open-source product is public information, the endpoint reads no data, and
    /// being able to send somebody a documentation link is most of its value.
    ///
    /// It is a setting rather than a constant because "the shape of our API is not
    /// something we publish" is a legitimate position for a private deployment to
    /// take, and there was previously no way to hold it. Turning it off answers
    /// `404`, not `401` — an endpoint that exists and refuses is still an
    /// admission that it exists.
    pub docs: bool,
    /// Declares this a development instance, which is what lets it boot on the
    /// development stack's defaults — a loopback OIDC issuer, the `weebo:weebo`
    /// database. Off by default: a production config that forgot `[oidc]` refuses
    /// to start instead of trusting whatever answers on localhost.
    pub dev: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 8080)),
            cors_origins: Vec::new(),
            docs: true,
            dev: false,
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
    /// Redis pub/sub, for when the NOTIFY per transaction starts to weigh. No
    /// adapter exists; kept as a name only so asking for it is refused at startup
    /// with a reason rather than as an unknown variant.
    Redis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct EventBus {
    pub backend: BusBackend,
}

/// Which object store `[attachments]` describes (ADR-0002, ADR-0022).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttachmentsBackend {
    /// Any S3-compatible bucket — RustFS by default (ADR-0002).
    #[default]
    S3,
    /// A local directory, served through this server's own routes rather than a
    /// bucket URL — for deployments with no S3-compatible service at all
    /// (ADR-0022).
    Filesystem,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Attachments {
    pub kind: AttachmentsBackend,

    // --- `kind = "s3"` ---
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Meaningless to most self-hosted stores and required by the signature, so it
    /// has a default rather than being asked for.
    #[serde(default = "default_region")]
    pub region: String,

    // --- `kind = "filesystem"` (ADR-0022) ---
    /// Where attachments live on disk. Created at startup if missing.
    pub path: String,
    /// This server's own externally-reachable base URL — the filesystem backend
    /// presigns against itself, so it has to know its own address.
    pub public_base_url: String,
    /// Signs the bearer URLs a filesystem-backed presign hands out.
    pub token_secret: String,
}

fn default_region() -> String {
    "us-east-1".to_owned()
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
    /// How long without activity before the server closes a lifecycle
    /// notification here (§7.2bis). Absent means the published default of one
    /// week; `"0"` means this channel's publishers close their own.
    pub close_after: Option<CloseAfter>,
    pub default_priority: Option<Priority>,
    pub receipts: bool,
    pub limits: Option<ChannelLimits>,
    pub access: Vec<Access>,
    /// Channel-scoped federation publishers (§5.3).
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

/// One `[[channels.managed.sinks]]` entry (design §7.1bis, §11).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Sink {
    #[serde(rename = "type")]
    pub kind: String,
    /// Where a `webhook` posts.
    pub url: Option<String>,
    /// Where an `email` goes.
    pub address: Option<String>,
    /// `env:NAME` or `file:/path` — resolved at startup, never held in this file.
    pub secret_ref: Option<String>,
}

impl Sink {
    /// Refuse a sink this build cannot serve, at startup rather than at the first
    /// incident.
    ///
    /// Three of these checks are security rather than tidiness, and they are here
    /// because this is the only place that sees the raw string:
    ///
    /// * **the scheme is `http` or `https`.** A `file:` webhook is not a delivery,
    ///   it is a write primitive pointed at our own filesystem;
    /// * **no credentials in the URL.** `https://user:pass@host` is legal and would
    ///   put a password into a queue row, a dead letter listing and a log line at
    ///   once — the sink id is derived from this URL (§7.1bis) precisely so that it
    ///   never carries one;
    /// * **a webhook has a secret.** An unsigned webhook is one a consumer cannot
    ///   tell from anyone else's POST, so the signature is not optional.
    pub(crate) fn validate(&self, channel: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            notify_domain::SINK_KINDS.contains(&self.kind.as_str()),
            "channel `{channel}` declares a sink of type `{}`; this build serves {:?}",
            self.kind,
            notify_domain::SINK_KINDS
        );

        match self.kind.as_str() {
            "webhook" => {
                let raw = self.url.as_deref().unwrap_or_default();
                let url = url::Url::parse(raw).map_err(|error| {
                    anyhow::anyhow!(
                        "channel `{channel}` has a webhook sink whose url is not one: {error}"
                    )
                })?;
                anyhow::ensure!(
                    matches!(url.scheme(), "http" | "https"),
                    "channel `{channel}` has a webhook sink with scheme `{}`; only http and https \
                     are delivered to",
                    url.scheme()
                );
                anyhow::ensure!(
                    url.username().is_empty() && url.password().is_none(),
                    "channel `{channel}` has a webhook url carrying credentials; put the secret in \
                     `secret_ref` — the url is stored and logged, and a password in it would be too"
                );
                anyhow::ensure!(
                    self.secret_ref.is_some(),
                    "channel `{channel}` has a webhook sink with no `secret_ref`; an unsigned \
                     webhook is one a consumer cannot tell from anybody else's POST (§7.1bis)"
                );
            }
            "email" => {
                let address = self.address.as_deref().unwrap_or_default();
                anyhow::ensure!(
                    address.contains('@') && !address.contains(char::is_whitespace),
                    "channel `{channel}` has an email sink whose `address` is not one"
                );
            }
            _ => {}
        }

        Ok(())
    }
}

/// Resolve a `env:NAME` / `file:/path` reference (design §11).
///
/// Two forms and no third: an inline secret would be a secret in a file somebody
/// commits, which is the whole reason §11 spells references rather than values.
pub(crate) fn resolve_secret(what: &str, reference: &str) -> anyhow::Result<String> {
    if let Some(name) = reference.strip_prefix("env:") {
        return std::env::var(name).map_err(|_| {
            anyhow::anyhow!("{what} names environment variable `{name}`, which is not set")
        });
    }

    if let Some(path) = reference.strip_prefix("file:") {
        let contents = std::fs::read_to_string(path).map_err(|error| {
            anyhow::anyhow!("{what} names `{path}`, which cannot be read: {error}")
        })?;
        // Trimmed: a secret written with `echo` into a mounted file ends in a
        // newline, and an HMAC over a trailing newline is a signature nobody can
        // reproduce and nobody can see.
        return Ok(contents.trim().to_owned());
    }

    anyhow::bail!("{what} must be `env:NAME` or `file:/path`, not `{reference}`")
}

/// `[sinks]` — what the outbound sinks need that is not per-channel (design
/// §7.1bis).
///
/// An addition to §11, on the same terms as `[federation]` before it: §11 declares
/// an `email` sink per channel and never says which relay carries it, and a relay
/// is instance-wide by nature — one SMTP server serves every channel that mails.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Sinks {
    pub smtp: Option<Smtp>,
}

/// `[sinks.smtp]` — the relay the `email` sinks hand their mail to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Smtp {
    /// `smtp://host:port` inside a cluster, `smtps://…` across one.
    pub url: String,
    /// The envelope sender.
    pub from: String,
    /// Relay credentials, when it asks for them.
    pub username: Option<String>,
    /// `env:NAME` or `file:/path`, like every other secret here.
    pub password_ref: Option<String>,
    /// Where this instance is reachable, for the link at the bottom of the mail
    /// (design §10.4). No link is written without it, because a guessed hostname
    /// produces mail full of dead links.
    pub permalink_base: Option<String>,
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

/// `[federation]` — signing the CI tokens this instance mints (design §5.3, §11).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Federation {
    /// Where the Ed25519 private key lives.
    ///
    /// A path and not the key, like `[push.webpush] vapid_key_file`, so it is
    /// mounted from whatever the cluster uses for secrets and never passes through
    /// a file somebody commits by accident (§15).
    ///
    /// It cannot be generated at boot: a replica would sign what its neighbours
    /// cannot verify, and the failure would be intermittent and look like a flaky
    /// pipeline (§11 `[ha]`).
    pub signing_key_file: PathBuf,
    /// What the minted tokens name as their audience.
    ///
    /// Defaults to `weebo-notify`. Worth configuring only where two instances share
    /// an ingress and a token for one must not be usable on the other.
    pub audience: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Push {
    pub webpush: Option<WebPush>,
    pub apns: Option<Apns>,
    pub unifiedpush: Option<UnifiedPush>,
}

/// `[push.webpush]` — RFC 8292 identification (design §11, M3.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WebPush {
    /// Where the P-256 VAPID private key lives.
    ///
    /// A path and not the key itself, so it can be mounted from whatever the cluster
    /// uses for secrets (§11) without the key passing through a config file that
    /// gets committed by accident.
    pub vapid_key_file: PathBuf,
    /// `mailto:…` or `https://…` — how a push service reaches this instance's
    /// operator (RFC 8292 `sub`).
    ///
    /// §11 did not list it, and it is not optional: relays refuse a VAPID assertion
    /// without one. Added rather than defaulted, because a wrong contact is worse
    /// than a missing one.
    pub contact: String,
}

/// `[push.apns]` — token-based `.p8` authentication for iOS (design §11, §6.2,
/// §10bis.5, M7).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Apns {
    /// Where the `.p8` provider authentication key lives.
    ///
    /// A path, not the key itself, same reason as `[push.webpush]`'s
    /// `vapid_key_file` (§11): mounted from whatever the cluster keeps secrets in,
    /// never a config file that gets committed by accident.
    pub key_file: PathBuf,
    /// The 10-character Key ID Apple assigned this `.p8` key.
    pub key_id: String,
    /// The 10-character Apple Developer Team ID.
    pub team_id: String,
    /// The app's bundle identifier, sent as `apns-topic` on every request.
    pub topic: String,
    /// Push against `api.sandbox.push.apple.com` instead of production — what a
    /// debug-signed build's device tokens are valid against. `false` by default:
    /// production is the common case, and a deployment that only ever ships
    /// debug builds is asking for something unusual enough to opt into.
    #[serde(default)]
    pub sandbox: bool,
}

/// `[push.unifiedpush]` — enables the self-hosted UnifiedPush transport (design
/// §11, §6.2, M6).
///
/// Empty on purpose, unlike `[push.webpush]`: there is no server-wide credential to
/// hold. The destination is per-device (the distributor endpoint from `POST
/// /devices`), and the payload is sealed to the device's own key rather than signed
/// by this server's — nothing here identifies the server to a relay the way VAPID
/// does. The section's only job is to exist, so an operator can turn the transport
/// on without inventing a value to put under it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UnifiedPush {}

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

    /// The configuration a `SIGHUP` may actually adopt (design §11, T21).
    ///
    /// # What a reload changes, and what it cannot
    ///
    /// §11 asks for reconciliation "au boot et sur SIGHUP", and reconciliation is
    /// what this honours: `[[channels.managed]]` and `[[service_accounts]]`. They
    /// are the sections the reconciler *applies to the database*, so re-reading them
    /// is enough for them to take effect.
    ///
    /// Everything else was wired into a running object at boot — the listening
    /// socket, the database pool, the OIDC verifier, the sink fleet, and the trust
    /// policies the token exchange holds. Re-reading those would change a value
    /// nothing consults.
    ///
    /// **Trust policies are adopted too, and only since D31 was closed.** They were
    /// held by value inside the token exchange, so a reload could change the file
    /// and nothing else; adopting them here would have written "a forge may now
    /// publish" into the audit log (§13, D28) while the process went on refusing
    /// that forge — an entry worse than none, since an incident review would date
    /// the change to the wrong hour. `notify_app::LivePolicies` makes the swap real,
    /// so the file and the running process now agree, and the journal tells the
    /// truth about when rights moved.
    ///
    /// What *did* move and still cannot be adopted is reported by
    /// [`Config::unreloadable`] rather than passed over, so an operator who edited
    /// the wrong section is told.
    pub(crate) fn reloaded_from(&self, file: Self) -> Self {
        Self {
            channels: file.channels,
            service_accounts: file.service_accounts,
            trust_policy: file.trust_policy,
            ..self.clone()
        }
    }

    /// Sections that differ in the file and that a reload cannot adopt (§11).
    ///
    /// The counterpart of [`Config::unhonoured`], and for the same reason: silence
    /// here is how an operator spends an afternoon wondering why their edit did
    /// nothing.
    pub(crate) fn unreloadable(&self, file: &Self) -> Vec<String> {
        let mut moved = Vec::new();

        if self.server.bind != file.server.bind {
            moved.push("[server] bind — the socket is bound at boot".to_owned());
        }
        if self.database.url != file.database.url {
            moved.push("[database] url — the pool is connected at boot".to_owned());
        }
        if self.oidc.issuer != file.oidc.issuer || self.oidc.client_id != file.oidc.client_id {
            moved.push("[oidc] — the verifier is built at boot".to_owned());
        }

        moved
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

        // actix-cors rejects `*` and malformed origins only when a worker starts,
        // which fails every worker. Refuse here, with the key named.
        for origin in &self.server.cors_origins {
            let exact = !origin.ends_with('/')
                && url::Url::parse(origin).is_ok_and(|url| {
                    url.host_str().is_some()
                        && url.username().is_empty()
                        && matches!(url.path(), "" | "/")
                        && url.query().is_none()
                        && url.fragment().is_none()
                });
            anyhow::ensure!(
                exact,
                "[server] cors_origins entry `{origin}` is not an exact origin \
                 (`scheme://host[:port]`, no path, no trailing slash, no `*`)"
            );
        }

        if let Some(attachments) = &self.attachments {
            match attachments.kind {
                AttachmentsBackend::S3 => {}
                AttachmentsBackend::Filesystem => {
                    anyhow::ensure!(
                        !attachments.path.is_empty(),
                        "[attachments] kind = \"filesystem\" requires `path`"
                    );
                    anyhow::ensure!(
                        !attachments.public_base_url.is_empty(),
                        "[attachments] kind = \"filesystem\" requires `public_base_url` \
                         — the address this server presigns its own upload/download \
                         URLs against"
                    );
                    anyhow::ensure!(
                        !attachments.token_secret.is_empty(),
                        "[attachments] kind = \"filesystem\" requires `token_secret` \
                         — it signs the URLs presign hands out, and an empty one would \
                         mean an unsigned, open directory"
                    );
                }
            }
        }

        for channel in &self.channels.managed {
            anyhow::ensure!(
                !channel.id.is_empty(),
                "a [[channels.managed]] entry has no id"
            );
            // Refused rather than read as "unlimited", which is what somebody
            // writing it almost certainly meant: a ceiling of zero refuses every
            // publish on the channel, and it would do so with a `429` that looks
            // like load rather than like configuration. Leaving `limits` out is how
            // §8.2's default is asked for.
            anyhow::ensure!(
                channel
                    .limits
                    .is_none_or(|limits| limits.publish_per_min != Some(0)),
                "channel `{}` sets limits.publish_per_min = 0, which refuses every \
                 publish; omit the key to take the default of design §8.2",
                channel.id
            );
            for sink in &channel.sinks {
                sink.validate(&channel.id)?;
                anyhow::ensure!(
                    sink.kind != "email" || self.sinks.smtp.is_some(),
                    "channel `{}` declares an email sink and there is no [sinks.smtp] \
                     relay to send it through",
                    channel.id
                );
            }
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

        if !self.trust_policy.is_empty() && self.federation.is_none() {
            pending.push(format!(
                "{} [[trust_policy]] entries — no [federation] block, so nothing \
                 can be exchanged against them (§5.3)",
                self.trust_policy.len()
            ));
        }

        let publishers: usize = self
            .channels
            .managed
            .iter()
            .map(|channel| channel.publishers.len())
            .sum();
        if publishers > 0 && self.federation.is_none() {
            pending.push(format!(
                "{publishers} [[channels.managed.publishers]] entries — no [federation] \
                 block, so nothing can be exchanged against them (§5.3)"
            ));
        }

        // `[[channels.managed.sinks]]` is honoured since M4bis and is absent from
        // this list on purpose, on the same terms as `[push.webpush]` below: the
        // composition root reads it, and a sink it cannot build stops the process.

        // `[[channels.managed]].limits` is honoured since M4octodecies — the
        // reconciler puts `publish_per_min` on the channel policy and the publish
        // path measures against it. It was parsed and read by nobody before that,
        // and it was not in this list either, which is the failure mode this
        // function exists to prevent: the one key that was silently dropped was the
        // one the drop-detector did not know about.

        // `[push.webpush]` is honoured since M3.5, `[push.unifiedpush]` since M6,
        // and `[push.apns]` since M7 — all three absent from this list on purpose:
        // the composition root reads them, and a transport it cannot build stops
        // the process rather than warning here.

        pending
    }
}

/// [`Config::load`] under figment's global `Jail` lock, so the T23 test's `WNK_*`
/// variables cannot leak into a load running concurrently on another thread.
#[cfg(test)]
pub(crate) fn load_isolated(path: &Path) -> anyhow::Result<Config> {
    let mut loaded = None;
    #[expect(
        clippy::result_large_err,
        reason = "the signature belongs to figment::Jail"
    )]
    figment::Jail::expect_with(|_| {
        loaded = Some(Config::load(path));
        Ok(())
    });
    loaded.unwrap_or_else(|| anyhow::bail!("the jail did not run"))
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
        let config = load_isolated(Path::new("/nonexistent/config.toml")).unwrap();

        assert_eq!(config.server.bind.port(), 8080);
        assert!(
            config.server.cors_origins.is_empty(),
            "closed by default (§5.1): an unlisted origin cannot call the API"
        );
        assert_eq!(config.retention.default_ttl, Ttl::parse("7d", "t").unwrap());
        assert!(!config.retention.allow_forever);
    }

    #[test]
    fn a_cors_origin_actix_would_refuse_is_refused_at_load() {
        for bad in [
            "*",
            "https://app.example/",
            "https://app.example/path",
            "app.example",
        ] {
            let mut config = Config::default();
            config.server.cors_origins = vec![bad.to_owned()];
            assert!(config.validate().is_err(), "`{bad}` was accepted");
        }
        let mut config = Config::default();
        config.server.cors_origins = vec![
            "https://app.example".to_owned(),
            "http://localhost:5173".to_owned(),
            "tauri://localhost".to_owned(),
        ];
        config.validate().unwrap();
    }

    #[test]
    fn the_committed_dev_configuration_parses() {
        // The one config file this repository ships. If it stops parsing, `task
        // dev:run` stops working, and finding that out at startup is worse than
        // finding it out here.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml");
        let config = load_isolated(&path).expect("config/dev.toml parses");

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

    /// Every scalar leaf of a serialised config, as `(path, value)`.
    ///
    /// The "generated" half of T23: the schema is not written down anywhere, so it
    /// is read off `Config::default()` — which is the same shape figment extracts
    /// into, so a field added tomorrow is walked tomorrow with no test to update.
    ///
    /// Sequences are skipped, and that is not laziness: `[[channels.managed]]` and
    /// the other repeated tables are GitOps material (§11), set in the file that
    /// declares them rather than one variable at a time.
    fn scalar_leaves(
        value: &serde_json::Value,
        prefix: &mut Vec<String>,
        found: &mut Vec<(Vec<String>, serde_json::Value)>,
    ) {
        match value {
            serde_json::Value::Object(fields) => {
                for (name, child) in fields {
                    prefix.push(name.clone());
                    scalar_leaves(child, prefix, found);
                    prefix.pop();
                }
            }
            // `null` is an absent optional section — `[telemetry]`, `[federation]`.
            // There is nothing under it to walk until something sets it, and the
            // test above covers that case for the one that matters.
            serde_json::Value::Null | serde_json::Value::Array(_) => {}
            scalar => found.push((prefix.clone(), scalar.clone())),
        }
    }

    #[test]
    fn critical_t23_every_configuration_key_can_be_set_from_the_environment() {
        // T23 (§3.4). The rule §11 states is "every key without exception", and the
        // only honest way to check a claim about *every* key is to enumerate them
        // rather than to list the ones somebody remembered.
        //
        // Walked from `config/dev.toml` rather than from the defaults, for a reason
        // that took a failing run to see: half the schema is optional sections and
        // empty vectors, so a default config serialises to eleven scalars and would
        // have let this test pass while covering almost nothing. The committed file
        // fills `[attachments]`, `[push.webpush]`, `[sinks.smtp]` and the rest — and
        // it is also the shape an operator actually overrides one variable at a time.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml");
        let mut leaves = Vec::new();
        scalar_leaves(
            &serde_json::to_value(load_isolated(&base).unwrap()).unwrap(),
            &mut Vec::new(),
            &mut leaves,
        );

        // A floor and not an exact count: the point is to fail when the walk stops
        // walking, not to be rewritten every time a key is added. It was 24 the day
        // this was written, with the repeated tables (`[[channels.managed]]`,
        // `[[trust_policy]]`, `[[service_accounts]]`) and the list-valued keys
        // deliberately outside — those are GitOps material, not variables.
        assert!(
            leaves.len() >= 20,
            "the walk found only {} keys, which means the walk is broken rather than \
             the configuration being small: {:?}",
            leaves.len(),
            leaves
                .iter()
                .map(|(path, _)| path.join("."))
                .collect::<Vec<_>>()
        );

        // How many keys were actually *taken* rather than refused by name. Both
        // outcomes are acceptable per key, so without this the test could pass with
        // every single key erroring — which would prove that the variables are read
        // and nothing about them being honoured.
        let mut taken = 0_usize;
        let total = leaves.len();

        for (path, value) in leaves {
            let variable = format!("{ENV_PREFIX}{}", path.join("__").to_uppercase());
            let sentinel = match &value {
                serde_json::Value::Bool(flag) => (!flag).to_string(),
                serde_json::Value::Number(number) => number
                    .as_i64()
                    .map_or_else(|| "1".to_owned(), |n| (n + 1).to_string()),
                _ => "weebo-t23".to_owned(),
            };

            #[expect(
                clippy::result_large_err,
                reason = "the signature belongs to figment::Jail"
            )]
            figment::Jail::expect_with(|jail| {
                jail.set_env(&variable, &sentinel);

                // Two acceptable outcomes, and they are acceptable for the same
                // reason: both prove the variable *reached* the field. Either it was
                // taken, or it was refused **by name** — a `bind` that will not parse
                // as a socket address says so. The failure this test exists for is
                // the third outcome: the variable is ignored in silence.
                match Config::load(&base) {
                    Ok(config) => {
                        let loaded = serde_json::to_value(config).unwrap();
                        let mut at = &loaded;
                        for segment in &path {
                            at = &at[segment];
                        }
                        let rendered = match at {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        assert_eq!(
                            rendered,
                            sentinel,
                            "{variable} loaded without changing {}",
                            path.join(".")
                        );
                        taken += 1;
                    }
                    Err(error) => {
                        let message = error.to_string().to_lowercase();
                        let key = path.last().unwrap();
                        assert!(
                            message.contains(key) || message.contains(&sentinel),
                            "{variable} was refused without naming {key}: {error}"
                        );
                    }
                }

                Ok(())
            });
        }

        // Roughly two thirds of the keys hold a free-form value the sentinel is
        // valid for; the rest are socket addresses, durations and closed enums that
        // refuse it by name. Both prove the variable arrived — but if this ever fell
        // to zero, the test would be checking error messages and nothing else.
        assert!(
            taken * 2 > total,
            "only {taken} of {total} keys were actually taken from the environment; \
             the rest merely produced an error naming themselves"
        );
    }

    #[test]
    fn traces_can_be_switched_on_by_environment_alone() {
        // The path a Kubernetes deployment actually takes: one file for every
        // environment (§11), and the collector named by a variable because it is the
        // one thing that differs. A whole section arriving from the environment is
        // the case `Option<Telemetry>` could plausibly get wrong — figment has to
        // build the `Some` and fill the rest from the defaults.
        //
        // The closure's error type is figment's, and clippy would rather it were
        // boxed — which is not this test's call to make.
        #[expect(
            clippy::result_large_err,
            reason = "the signature belongs to figment::Jail"
        )]
        figment::Jail::expect_with(|jail| {
            jail.set_env("WNK_TELEMETRY__OTLP_ENDPOINT", "http://collector:4318");

            let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();

            let telemetry = config
                .telemetry
                .expect("naming a collector is enough to turn traces on");
            assert_eq!(telemetry.otlp_endpoint, "http://collector:4318");
            assert_eq!(
                telemetry.service_name, "weebo-notify",
                "the rest of the section still comes from its defaults"
            );
            assert!((telemetry.sample_ratio - 1.0).abs() < f64::EPSILON);

            Ok(())
        });
    }

    #[test]
    fn a_kubernetes_lease_is_refused_rather_than_silently_downgraded() {
        let path = with_config("k8s", "[ha]\nleader_election = \"kubernetes\"\n");

        let error = load_isolated(&path).unwrap_err().to_string();

        assert!(
            error.contains("not implemented"),
            "two leaders across a cluster is the failure this prevents: {error}"
        );
    }

    #[test]
    fn a_filesystem_attachment_store_without_its_required_fields_is_refused() {
        let path = with_config("fs-incomplete", "[attachments]\nkind = \"filesystem\"\n");

        let error = load_isolated(&path).unwrap_err().to_string();

        assert!(
            error.contains("requires `path`"),
            "an unset directory would be a filesystem store with nowhere to write: {error}"
        );
    }

    #[test]
    fn a_complete_filesystem_attachment_store_parses_and_validates() {
        let path = with_config(
            "fs-complete",
            "[attachments]\n\
             kind = \"filesystem\"\n\
             path = \"/data/attachments\"\n\
             public_base_url = \"https://notify.weebo.poc\"\n\
             token_secret = \"correct-horse-battery-staple\"\n",
        );

        let config = load_isolated(&path).expect("a complete filesystem config validates");
        let attachments = config.attachments.expect("[attachments] was set");

        assert_eq!(attachments.kind, AttachmentsBackend::Filesystem);
        assert_eq!(attachments.path, "/data/attachments");
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
        assert!(load_isolated(&both).is_err());

        let neither = with_config(
            "neither",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             scopes = [\"subscribe\"]\n",
        );
        assert!(load_isolated(&neither).is_err());
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

        assert!(load_isolated(&path).is_err());
    }

    #[test]
    fn what_this_build_parses_but_ignores_is_reported_rather_than_dropped() {
        let path = with_config(
            "pending",
            "[[trust_policy]]\n\
             issuer = \"https://forgejo.weebo.poc\"\n\
             subject = \"repo:weebo/*\"\n\
             scopes = [\"publish\"]\n",
        );

        let pending = load_isolated(&path).unwrap().unhonoured();

        assert_eq!(
            pending.len(),
            1,
            "a silently ignored key costs an afternoon"
        );
        assert!(pending.iter().any(|line| line.contains("trust_policy")));
    }

    #[test]
    fn a_sink_is_no_longer_reported_as_a_key_this_build_ignores() {
        let path = with_config(
            "sinks-honoured",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n\
             secret_ref = \"env:WNK_SINK_HMAC\"\n",
        );

        // Same rule as Web Push above: warning about a section the composition root
        // reads would train an operator to ignore this list.
        assert!(
            !load_isolated(&path)
                .unwrap()
                .unhonoured()
                .iter()
                .any(|line| line.contains("sinks"))
        );
    }

    #[test]
    fn a_webhook_with_no_secret_is_refused_at_startup() {
        let path = with_config(
            "unsigned-webhook",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n",
        );

        // An unsigned webhook is one a consumer cannot tell from anybody else's POST
        // (§7.1bis), so it is not a degraded mode — it is a refusal.
        assert!(load_isolated(&path).is_err());
    }

    #[test]
    fn a_webhook_url_that_would_leak_or_reach_inward_is_refused() {
        for url in [
            // A password that would land in a queue row, a dead letter listing and
            // a log line at once — the sink id is derived from this url.
            "https://user:hunter2@hooks.weebo.poc/incidents",
            // Not a delivery: a write primitive pointed at our own filesystem.
            "file:///etc/passwd",
        ] {
            let path = with_config(
                "bad-webhook",
                &format!(
                    "[[channels.managed]]\n\
                     id = \"ci-infra\"\n\
                     [[channels.managed.sinks]]\n\
                     type = \"webhook\"\n\
                     url = \"{url}\"\n\
                     secret_ref = \"env:WNK_SINK_HMAC\"\n"
                ),
            );

            assert!(load_isolated(&path).is_err(), "`{url}` must be refused");
        }
    }

    #[test]
    fn an_email_sink_without_a_relay_is_refused_rather_than_queued_forever() {
        let path = with_config(
            "mail-no-relay",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"email\"\n\
             address = \"oncall@weebo.poc\"\n",
        );

        assert!(load_isolated(&path).is_err());
    }

    #[test]
    fn a_channel_publish_limit_is_parsed_where_the_reconciler_reads_it() {
        // Written the way design §11 writes it — an inline table — rather than as a
        // sub-table. Both are TOML and both deserialise; the one worth pinning is
        // the one an operator will copy out of the document.
        let path = with_config(
            "channel-limits",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             limits = { publish_per_min = 120 }\n",
        );

        let config = load_isolated(&path).unwrap();

        assert_eq!(
            config.channels.managed[0]
                .limits
                .and_then(|limits| limits.publish_per_min),
            Some(120)
        );
    }

    #[test]
    fn a_publish_limit_of_zero_is_refused_rather_than_read_as_unlimited() {
        // Zero refuses every publish on the channel, with a `429` that reads like
        // load rather than like configuration. Whoever writes it means "no limit",
        // and the way to ask for that is to leave the key out.
        let path = with_config(
            "channel-limits-zero",
            "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             limits = { publish_per_min = 0 }\n",
        );

        assert!(load_isolated(&path).is_err());
    }

    #[test]
    fn a_web_push_key_is_no_longer_reported_as_a_key_this_build_ignores() {
        let path = with_config(
            "webpush-honoured",
            "[push.webpush]\n\
             vapid_key_file = \"/secrets/vapid.pem\"\n\
             contact = \"mailto:ops@weebo.poc\"\n\
             \n\
             [push.apns]\n\
             key_file = \"/secrets/apns.p8\"\n\
             key_id = \"ABCDE12345\"\n\
             team_id = \"TEAM123456\"\n\
             topic = \"poc.weebo.notify\"\n",
        );

        let pending = load_isolated(&path).unwrap().unhonoured();

        // Warning about a section the composition root reads would train an operator
        // to ignore this list, which is the one thing it cannot afford (M3.5).
        assert!(
            !pending.iter().any(|line| line.contains("push.webpush")),
            "Web Push is wired: {pending:?}"
        );
        assert!(
            !pending.iter().any(|line| line.contains("push.apns")),
            "APNs is wired since M7: {pending:?}"
        );
    }

    #[test]
    fn an_empty_unifiedpush_section_is_no_longer_reported_as_a_key_this_build_ignores() {
        let path = with_config(
            "unifiedpush-honoured",
            "[push.unifiedpush]\n\
             \n\
             [push.apns]\n\
             key_file = \"/secrets/apns.p8\"\n\
             key_id = \"ABCDE12345\"\n\
             team_id = \"TEAM123456\"\n\
             topic = \"poc.weebo.notify\"\n",
        );

        let pending = load_isolated(&path).unwrap().unhonoured();

        assert!(
            !pending.iter().any(|line| line.contains("push.unifiedpush")),
            "UnifiedPush is wired: {pending:?}"
        );
        assert!(
            !pending.iter().any(|line| line.contains("push.apns")),
            "APNs is wired since M7: {pending:?}"
        );
    }

    #[test]
    fn a_cli_section_names_where_the_binary_lives() {
        let path = with_config(
            "cli-binary-path",
            "[cli]\n\
             binary_path = \"/usr/local/bin/notify\"\n",
        );

        let config = load_isolated(&path).unwrap();

        assert_eq!(
            config.cli.unwrap().binary_path,
            std::path::PathBuf::from("/usr/local/bin/notify")
        );
    }

    #[test]
    fn no_cli_section_means_the_download_route_has_nothing_to_serve() {
        let config = load_isolated(Path::new("/nonexistent/config.toml")).unwrap();

        assert!(
            config.cli.is_none(),
            "absent by default, like [federation] and [push.webpush]"
        );
    }

    // --- What a SIGHUP adopts, and what it cannot (design §11, T21) ----------

    #[test]
    fn a_reload_adopts_the_managed_channels_and_accounts() {
        let running = Config::default();
        let file = Config {
            channels: Channels {
                managed: vec![ManagedChannel {
                    id: "ci-infra".to_owned(),
                    ..ManagedChannel::default()
                }],
                ..Channels::default()
            },
            service_accounts: vec![ManagedServiceAccount {
                id: "svc_ci".to_owned(),
                ..ManagedServiceAccount::default()
            }],
            ..Config::default()
        };

        // These are the two sections the reconciler applies *to the database*, so
        // re-reading them is enough for them to take effect — which is exactly what
        // §11 asks a SIGHUP for.
        let reloaded = running.reloaded_from(file);

        assert_eq!(reloaded.channels.managed.len(), 1);
        assert_eq!(reloaded.service_accounts.len(), 1);
    }

    #[test]
    fn a_reload_adopts_the_trust_policies() {
        let running = Config {
            trust_policy: vec![TrustPolicy {
                issuer: "https://forge.example".to_owned(),
                subject: "repo:weebo/infra:ref:refs/heads/main".to_owned(),
                ..TrustPolicy::default()
            }],
            ..Config::default()
        };
        let file = Config {
            trust_policy: vec![TrustPolicy {
                issuer: "https://forge.example".to_owned(),
                subject: "repo:somebody/else:ref:refs/heads/main".to_owned(),
                ..TrustPolicy::default()
            }],
            ..Config::default()
        };

        let reloaded = running.reloaded_from(file.clone());

        // Adopted only since D31 was closed. Before that, `[[trust_policy]]` was held
        // by value inside the token exchange, so taking the file's version here would
        // have made the reconciler journal "a forge may now publish" (§13, D28) while
        // the process went on refusing it — an entry dating the change to the wrong
        // hour, which is worse than no entry. `LivePolicies` makes the swap real, so
        // the file, the running process and the journal now agree.
        assert_eq!(
            reloaded.trust_policy[0].subject, "repo:somebody/else:ref:refs/heads/main",
            "a reload adopts the file's trust policies, and the swap makes that true"
        );
    }

    #[test]
    fn a_reload_says_what_it_could_not_adopt() {
        let running = Config::default();
        let file = Config {
            trust_policy: vec![TrustPolicy {
                issuer: "https://forge.example".to_owned(),
                ..TrustPolicy::default()
            }],
            database: Database {
                url: "postgres://elsewhere/db".to_owned(),
                ..Database::default()
            },
            ..Config::default()
        };

        let moved = running.unreloadable(&file);

        // Silence here is how somebody spends an afternoon wondering why their edit
        // did nothing — the same reason `unhonoured` exists.
        assert!(
            moved.iter().any(|line| line.contains("database")),
            "{moved:?}"
        );
        // And trust policies are *not* on this list any more: since D31 they are
        // genuinely adopted, so warning about them would train an operator to ignore
        // the warnings that still mean something.
        assert!(
            !moved.iter().any(|line| line.contains("trust_policy")),
            "{moved:?}"
        );
    }

    #[test]
    fn an_unchanged_file_has_nothing_to_report() {
        let running = Config {
            trust_policy: vec![TrustPolicy {
                issuer: "https://forge.example".to_owned(),
                ..TrustPolicy::default()
            }],
            ..Config::default()
        };

        // A SIGHUP that changed nothing must be quiet, or the warning stops meaning
        // anything the one time it matters.
        assert!(running.unreloadable(&running.clone()).is_empty());
    }
}
