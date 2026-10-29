//! The server's configuration (design §11): the types, their defaults, the
//! structural validation and the reload split (ADR-0031). No figment and no I/O,
//! so an operator renders and checks `config.toml` with notify's own rules; the
//! server layers `WNK_*` over the file and checks the secret values on top.
//! Parsed-but-unhonoured sections are reported at startup by [`Config::unhonoured`].

use std::net::SocketAddr;
use std::path::PathBuf;

use notify_domain::{CloseAfter, HumanDuration, Priority, Scope, Ttl};
use serde::{Deserialize, Serialize};

/// `notify_app::DEFAULT_MAX_DEVICES_PER_USER`, which this crate cannot depend on;
/// notify-server's tests hold the two equal.
const DEFAULT_MAX_DEVICES_PER_USER: u32 = 20;
/// `notify_app::DEFAULT_GROUP_SNAPSHOT_TTL` (a week), in seconds; held equal the same way.
const DEFAULT_GROUP_SNAPSHOT_TTL_SECS: u64 = 7 * 24 * 3_600;

/// Everything the server is configured with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub database: Database,
    pub oidc: Oidc,
    pub auth: Auth,
    pub admin: Admin,
    pub retention: Retention,
    pub ha: Ha,
    pub eventbus: EventBus,
    pub channels: Channels,
    pub attachments: Option<Attachments>,

    /// Managed service accounts, reconciled like channels (§5.2bis).
    pub service_accounts: Vec<ManagedServiceAccount>,

    /// JSON webhook transforms (RFC 0003), swapped on `SIGHUP`.
    pub transforms: Vec<ManagedTransform>,

    /// `[[transform]]`, a misspelt `[[transforms]]`: kept only so
    /// [`Config::unhonoured`] can say so, since unknown top-level keys load.
    #[serde(skip_serializing)]
    pub transform: Option<toml::Value>,

    /// Federation trust policies (§5.3).
    pub trust_policy: Vec<TrustPolicy>,

    /// `[federation]` — the Ed25519 signing key (§5.3, §11). Absent: no federation (`501`);
    /// present but unreadable: startup failure.
    pub federation: Option<Federation>,

    /// Push transport credentials (§11).
    pub push: Push,

    /// `[sinks]` — what the outbound sinks of §7.1bis need beyond their channel.
    pub sinks: Sinks,

    /// `[telemetry]` — where the traces of §13 go; absent (the default) means no spans are exported.
    pub telemetry: Option<Telemetry>,

    /// `[cli]` — serving the `notify` CLI binary (design §9). Absent: `501`; unreadable path: startup failure.
    pub cli: Option<Cli>,

    /// `[reconcile]` — how much a single pass may take away (§11).
    pub reconcile: Reconcile,

    /// `[devices]` — how many devices one owner may enrol.
    pub devices: Devices,

    /// `[reload]` — whether the server re-reads its file when it changes (ADR-0031).
    pub reload: Reload,

    /// `[[key_issuers]]` — tokens that mint keys for managed service accounts (ADR-0033).
    pub key_issuers: Vec<KeyIssuer>,
}

/// One `[[key_issuers]]` entry (ADR-0033): a token, presented as
/// `wki_<id>_<secret>`, that may mint, list and revoke the keys of managed service
/// accounts and nothing else. Read at boot; rotating it is a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyIssuer {
    /// A slug of letters, digits and `-`, unique among issuers; the audit actor.
    pub id: String,
    /// `env:NAME` or `file:/path` to the secret, at least
    /// [`KeyIssuer::MIN_SECRET_BYTES`] bytes. Never inline: this file is a ConfigMap.
    pub token: String,
}

impl KeyIssuer {
    /// Same floor as the fingerprint keys.
    pub const MIN_SECRET_BYTES: usize = 32;

    fn validate(&self) -> anyhow::Result<()> {
        // `_` ends the id in `wki_<id>_<secret>`, so it cannot be part of one.
        anyhow::ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !self.id.starts_with('-')
                && !self.id.ends_with('-'),
            "[[key_issuers]] id `{}` must be 1 to 64 lowercase letters, digits and `-`, \
             not starting or ending with `-`",
            self.id
        );
        let reference = self
            .token
            .strip_prefix("env:")
            .or_else(|| self.token.strip_prefix("file:"));
        anyhow::ensure!(
            reference.is_some_and(|rest| !rest.is_empty()),
            "[[key_issuers]] `{}` token must be `env:NAME` or `file:/path`; an inline \
             token would put the secret in the file",
            self.id
        );
        Ok(())
    }
}

/// `[reload]` — re-reading the file without a `SIGHUP` (ADR-0031).
///
/// The server reads the file every `interval` and reloads, exactly as on `SIGHUP`,
/// when its content changed. Polling the path rather than watching the inode is what
/// sees a ConfigMap update, which swaps a symlink and never writes the file. Read at
/// boot: changing it takes a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reload {
    /// Poll the file. `false` leaves `SIGHUP` as the only reload.
    pub watch: bool,
    /// How often the file is read and compared.
    pub interval: HumanDuration,
}

impl Reload {
    /// Under a second, the poll is a busy loop on the file for no gain: kubelet's
    /// ConfigMap sync is a minute behind anyway.
    const MIN_INTERVAL: u64 = 1;
}

impl Default for Reload {
    fn default() -> Self {
        Self {
            watch: true,
            interval: HumanDuration::from_secs_const(5),
        }
    }
}

/// `[devices]` — the ceiling on enrolment.
///
/// Every active device is a credential the owner's notifications go to and a push
/// the fan-out pays for, so a leaked session must not be able to enrol them
/// without end. Counts push and stream devices together; a revoked one frees its
/// slot. Past it, `POST /devices` answers `422 device.limit_reached`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Devices {
    /// The most active devices one owner may hold.
    pub max_per_user: u32,
}

impl Default for Devices {
    fn default() -> Self {
        Self {
            max_per_user: DEFAULT_MAX_DEVICES_PER_USER,
        }
    }
}

/// A configuration value that must never be printed.
///
/// `Debug` says `[redacted]`, and there is no `Display`: a `{:?}` of the
/// configuration is exactly the line that ends up in a startup log or a panic
/// message, and a database password in it would outlive the incident it was
/// printed for. Serialised as the plain string (`serde(transparent)`), because
/// figment reads and layers the configuration through serde and the value has to
/// survive that round trip; nothing serialises a whole `Config` anywhere a person
/// reads it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The value, for the one call that needs it. Named so every use is a grep.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Debug for Secret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// `[reconcile]` — the blast-radius guard on what a pass removes (§11).
///
/// A truncated or half-merged file looks exactly like an operator deleting most
/// of their channels and accounts, and the reconciler cannot tell the two apart.
/// So a pass that would archive, delete or disable more than `max_removals` things
/// at once applies everything *else* in the file and refuses the removals, logs an
/// error and counts `reconcile_removals_refused_total` — until the file is fixed, or
/// the operator says this one is meant with `allow_mass_removal = true`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Reconcile {
    /// The most removals one pass may make. Five covers renaming a handful of
    /// channels; a typo that drops the whole `[[channels.managed]]` array does not
    /// fit under it on any deployment large enough for the loss to hurt.
    pub max_removals: u32,
    /// Lift the guard. Meant to be set for the one deploy that removes a lot on
    /// purpose, and unset after.
    pub allow_mass_removal: bool,
}

impl Default for Reconcile {
    fn default() -> Self {
        Self {
            max_removals: 5,
            allow_mass_removal: false,
        }
    }
}

/// `[cli]` — where the built `notify` binary lives (design §9).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cli {
    /// Path to the binary, built from the same commit as `notify-server`.
    pub binary_path: PathBuf,
}

/// Where the OTLP exporter sends, and what it calls this process (design §13).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Telemetry {
    /// The collector's OTLP HTTP base URL, e.g. `http://collector:4318` (`/v1/traces` is appended).
    pub otlp_endpoint: String,

    /// Service name for traces: one per deployment (`service.instance.id` tells replicas apart).
    pub service_name: String,

    /// Fraction of root traces recorded, 0.0 to 1.0; children follow their root.
    pub sample_ratio: f64,

    /// Continue the `traceparent` an incoming request carries.
    ///
    /// Off by default: the header is sent by whoever calls the API, and a caller
    /// that marks every request sampled would otherwise force this process to
    /// record — and export — 100% of its traces, whatever `sample_ratio` says.
    /// Turn it on where the callers are your own CI and services and linking their
    /// trace to the notification is worth it; even then, a remote "sampled" flag
    /// only *asks*, and the ratio still decides (`telemetry::provider`).
    pub trust_inbound_trace_context: bool,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            // No default endpoint: an empty one is refused at startup.
            otlp_endpoint: String::new(),
            service_name: "weebo-notify".to_owned(),
            sample_ratio: 1.0,
            trust_inbound_trace_context: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    pub bind: SocketAddr,
    /// Closed by default (§5.1): an origin that is not listed cannot call the API
    /// from a browser.
    pub cors_origins: Vec<String>,
    /// Whether `/api/docs` serves the Scalar UI and OpenAPI document; on by default, off answers `404`.
    pub docs: bool,
    /// Declares a development instance, allowing the dev-stack defaults (loopback OIDC, `weebo:weebo`).
    /// Off by default, so a production config missing `[oidc]` refuses to start.
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
pub struct Database {
    /// Carries the password, so it is a [`Secret`].
    pub url: Secret,
    /// Pool size; leave headroom, since each leading background job holds a connection (its lock).
    /// Refused below [`Database::MIN_CONNECTIONS`].
    pub max_connections: u32,
    /// Apply the migrations at boot. `false` leaves them to `notify-server migrate`
    /// (a Job or an init container), and the server then refuses to start on a
    /// schema that is behind the binary rather than serving against it.
    pub migrate_on_start: bool,
    /// How long a request waits for a pooled connection before failing.
    pub acquire_timeout: HumanDuration,
    /// How long an unused connection is kept.
    pub idle_timeout: HumanDuration,
    /// How long any connection lives before it is replaced.
    pub max_lifetime: HumanDuration,
    /// Postgres `statement_timeout` on every pooled connection; `0` disables it.
    /// Migrations run with it lifted.
    pub statement_timeout: HumanDuration,
    /// Postgres `idle_in_transaction_session_timeout`; `0` disables it.
    pub idle_in_transaction_timeout: HumanDuration,
}

impl Database {
    /// The smallest pool this build can run on.
    ///
    /// Six leader jobs (purge, stale closure, DND digest, sink drain, feed poll,
    /// reconcile) each hold one connection for as long as they lead — the advisory
    /// lock lives on it — and a replica can lead all six at once. Four more is the least that
    /// still serves requests (and lets those jobs do their own queries) while they
    /// do. The `LISTEN` connection is taken from the pool and detached, so it is
    /// not counted against it.
    pub const MIN_CONNECTIONS: u32 = 6 + 4;
}

impl Default for Database {
    fn default() -> Self {
        Self {
            url: "postgres://weebo:weebo@localhost:5432/weebo_notify".into(),
            max_connections: 16,
            migrate_on_start: true,
            acquire_timeout: HumanDuration::from_secs_const(5),
            idle_timeout: HumanDuration::from_secs_const(10 * 60),
            max_lifetime: HumanDuration::from_secs_const(30 * 60),
            statement_timeout: HumanDuration::from_secs_const(30),
            idle_in_transaction_timeout: HumanDuration::from_secs_const(60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Oidc {
    pub issuer: String,
    pub client_id: String,
    /// Extra audiences a token may carry, for issuers whose tokens name the API; the client id is always accepted.
    pub audiences: Vec<String>,
    /// Which claim carries group membership; there is no standard one (T29).
    pub groups_claim: String,
    /// Trust an `email` the issuer does not mark `email_verified`. Off: an
    /// individual `email` binding (§11) must not go to whoever typed the address
    /// first. Only for an issuer that never sends the claim.
    pub trust_unverified_email: bool,
    /// How long past its one-hour max age the issuer key set keeps verifying
    /// while the issuer cannot be reached. It is also the window in which a key
    /// the issuer *withdrew* still verifies here, if the outage and the
    /// withdrawal coincide. `0` drops the set on the first failed refetch: no
    /// window, but an IdP blip logs everybody out.
    pub jwks_stale_grace: HumanDuration,
}

impl Default for Oidc {
    fn default() -> Self {
        Self {
            issuer: "http://localhost:9110".to_owned(),
            client_id: "weebo-notify".to_owned(),
            audiences: Vec::new(),
            groups_claim: "groups".to_owned(),
            trust_unverified_email: false,
            jwks_stale_grace: HumanDuration::from_secs_const(3_600),
        }
    }
}

/// `[auth]` — how credentials are turned into callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Auth {
    /// How old an owner's remembered groups may be and still widen what their
    /// personal API key reaches. Groups are remembered when the owner syncs or
    /// enrols a device; past this, the key keeps only direct memberships.
    pub group_snapshot_ttl: HumanDuration,
}

impl Auth {
    /// Under an hour, an owner who is actively using the SPA but has not synced
    /// in that time flickers in and out of their group reach.
    const MIN_GROUP_SNAPSHOT_TTL: u64 = 3_600;
    /// Past 90 days, a user removed from a group at the IdP keeps reaching its
    /// channels through a key for a season — revocation by forgetting stops
    /// meaning anything.
    const MAX_GROUP_SNAPSHOT_TTL: u64 = 90 * 24 * 3_600;

    fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (Self::MIN_GROUP_SNAPSHOT_TTL..=Self::MAX_GROUP_SNAPSHOT_TTL)
                .contains(&self.group_snapshot_ttl.as_secs()),
            "[auth] group_snapshot_ttl = {} is outside 1h..=90d: shorter makes an \
             active user's personal key flicker out of its group reach, longer keeps \
             a user removed from a group reaching it through a key for months",
            self.group_snapshot_ttl
        );
        Ok(())
    }
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            group_snapshot_ttl: HumanDuration::from_secs_const(DEFAULT_GROUP_SNAPSHOT_TTL_SECS),
        }
    }
}

/// The platform role (§5.4). GitOps only — no API path can grant it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Admin {
    pub oidc_groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Retention {
    pub default_ttl: Ttl,
    pub max_ttl: Option<HumanDuration>,
    /// Whether a channel may keep notifications forever. Off by default: unbounded
    /// retention is a deliberate operator decision (§7.2bis).
    pub allow_forever: bool,
    /// Rows per statement in the purge pass. A pass loops batches until each queue
    /// is drained or `pass_budget` is spent.
    pub batch_size: u32,
    /// How long one purge pass may keep looping. Keep it under the one-minute job
    /// interval.
    pub pass_budget: HumanDuration,
    /// How long a revoked device or a revoked/expired API key is kept before its
    /// row is deleted. It authenticates nothing either way; this is how long the
    /// owner's list keeps showing it.
    pub revoked_credentials: HumanDuration,
    /// How long audit entries are kept (ADR-0027). Unset — the default — keeps
    /// them for ever; anything under 90 days is refused at boot.
    pub audit: Option<HumanDuration>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            #[expect(
                clippy::expect_used,
                reason = "a literal that fails to parse is a compile-time mistake, \
                          caught by this module's own tests"
            )]
            default_ttl: Ttl::parse("7d", "retention.default_ttl").expect("a valid literal"),
            max_ttl: None,
            allow_forever: false,
            batch_size: 1_000,
            pass_budget: HumanDuration::from_secs_const(20),
            revoked_credentials: HumanDuration::from_secs_const(90 * 24 * 3_600),
            audit: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderBackend {
    /// A Postgres advisory lock. Zero extra infrastructure.
    #[default]
    Postgres,
    /// The Kubernetes Lease API; not implemented, so refused at startup.
    Kubernetes,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ha {
    pub leader_election: LeaderBackend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusBackend {
    /// `LISTEN/NOTIFY` (ADR-0008).
    #[default]
    Postgres,
    /// Redis pub/sub; no adapter exists, so refused at startup with a reason.
    Redis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EventBus {
    pub backend: BusBackend,
}

/// Which object store `[attachments]` describes (ADR-0002, ADR-0022).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentsBackend {
    /// Any S3-compatible bucket — RustFS by default (ADR-0002).
    #[default]
    S3,
    /// A local directory served through this server's own routes, for deployments without S3 (ADR-0022).
    Filesystem,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachments {
    pub kind: AttachmentsBackend,

    // --- `kind = "s3"` ---
    pub endpoint: String,
    pub bucket: String,
    pub access_key: Secret,
    pub secret_key: Secret,
    /// Region; required by the signature, meaningless to most self-hosted stores.
    #[serde(default = "default_region")]
    pub region: String,

    // --- `kind = "filesystem"` (ADR-0022) ---
    /// Where attachments live on disk. Created at startup if missing.
    pub path: String,
    /// This server's externally reachable base URL; the filesystem backend presigns against itself.
    pub public_base_url: String,
    /// Signs the bearer URLs a filesystem-backed presign hands out.
    pub token_secret: Secret,
}

fn default_region() -> String {
    "us-east-1".to_owned()
}

/// What happens to a managed channel that disappears from the config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnRemoved {
    /// Leave the rows in place and stop reconciling it; the safe default, as deletion takes history.
    #[default]
    Archive,
    /// Delete it, history included.
    Delete,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Channels {
    pub on_removed: OnRemoved,
    /// `sighup` — reload without a restart (§11).
    pub reload: Option<String>,
    pub managed: Vec<ManagedChannel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedChannel {
    pub id: String,
    pub name: Option<String>,
    pub default_ttl: Option<Ttl>,
    pub max_ttl: Option<HumanDuration>,
    /// Inactivity before the server closes a lifecycle notification (§7.2bis); absent means one week,
    /// `"0"` means publishers close their own.
    pub close_after: Option<CloseAfter>,
    pub default_priority: Option<Priority>,
    pub receipts: bool,
    pub limits: Option<ChannelLimits>,
    pub access: Vec<Access>,
    /// Channel-scoped federation publishers (§5.3).
    pub publishers: Vec<Publisher>,
    /// Outbound sinks (§7.1bis), swapped on `SIGHUP` like the rest of this block.
    pub sinks: Vec<Sink>,
    /// Feeds published into this channel (RFC 0002). Not serialised when empty, so
    /// the snapshot of a channel without feeds is the one earlier builds stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feeds: Vec<Feed>,
}

/// One `[[channels.managed.feeds]]` entry (RFC 0002 §4.1). Unknown keys are refused,
/// so `interval = "15m"` is an error rather than a silent 30m.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feed {
    pub url: String,
    /// A `[[service_accounts]]` id holding `publish` on this channel.
    pub publisher: String,
    #[serde(default)]
    pub every: Option<HumanDuration>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub max_per_poll: Option<u32>,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default)]
    pub lang: FeedLang,
}

/// One `[[transforms]]` entry (RFC 0003 §4.1): plain keys, and CEL source in the
/// others. Unknown keys are refused, so `raw_body = true` is an error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedTransform {
    pub id: String,
    pub channels: Vec<String>,
    #[serde(default)]
    pub lang: FeedLang,
    #[serde(default)]
    pub split: Option<String>,
    #[serde(default)]
    pub max_items: Option<usize>,
    pub title: String,
    #[serde(default)]
    pub title_fallback: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub group_key: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub ack: Option<String>,
    #[serde(default)]
    pub update: bool,
    #[serde(default)]
    pub raw: RawBody,
}

/// `raw` (RFC 0003 §4.2): off unless the operator writes it down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawBody {
    #[default]
    Off,
    Item,
    Payload,
}

impl ManagedTransform {
    /// What the adapter compiles. Fails on a channel id that does not parse.
    pub fn spec(&self) -> anyhow::Result<notify_transform_cel::TransformSpec> {
        let channels = self
            .channels
            .iter()
            .map(|id| {
                notify_domain::ChannelId::parse(id).map_err(|error| {
                    anyhow::anyhow!("[[transforms]] `{}` channels: `{id}`: {error}", self.id)
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(notify_transform_cel::TransformSpec {
            id: self.id.clone(),
            channels,
            lang: match self.lang {
                FeedLang::En => notify_ports::FeedLang::En,
                FeedLang::Fr => notify_ports::FeedLang::Fr,
            },
            split: self.split.clone(),
            max_items: self.max_items,
            title: self.title.clone(),
            title_fallback: self.title_fallback.clone(),
            body: self.body.clone(),
            link: self.link.clone(),
            external_id: self.external_id.clone(),
            group_key: self.group_key.clone(),
            tags: self.tags.clone(),
            priority: self.priority.clone(),
            status: self.status.clone(),
            state: self.state.clone(),
            ack: self.ack.clone(),
            update: self.update,
            raw: match self.raw {
                RawBody::Off => notify_ports::RawMode::Off,
                RawBody::Item => notify_ports::RawMode::Item,
                RawBody::Payload => notify_ports::RawMode::Payload,
            },
        })
    }

    /// Every program compiles and passes the §4.3 checks.
    pub fn compile(
        &self,
        slots: notify_transform_cel::Slots,
    ) -> anyhow::Result<notify_transform_cel::CelTransform> {
        Ok(notify_transform_cel::CelTransform::compile(
            self.spec()?,
            slots,
        )?)
    }
}

/// The language of the strings the server writes for a feed (RFC 0002 decision 14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedLang {
    #[default]
    En,
    Fr,
}

impl Feed {
    pub const DEFAULT_EVERY: std::time::Duration = std::time::Duration::from_mins(30);
    const MIN_EVERY: std::time::Duration = std::time::Duration::from_mins(5);
    const MAX_EVERY: std::time::Duration = std::time::Duration::from_hours(24);
    pub const DEFAULT_MAX_PER_POLL: u32 = 10;
    const MAX_MAX_PER_POLL: u32 = 50;

    pub fn every(&self) -> std::time::Duration {
        self.every
            .map_or(Self::DEFAULT_EVERY, HumanDuration::as_duration)
    }

    #[must_use]
    pub fn max_per_poll(&self) -> u32 {
        self.max_per_poll.unwrap_or(Self::DEFAULT_MAX_PER_POLL)
    }

    /// The per-feed rows of RFC 0002 §4.3; the checks against the rest of the file
    /// are [`Config::validate`]'s.
    pub fn validate(&self, channel: &str, position: usize) -> anyhow::Result<url::Url> {
        let at = format!("channel `{channel}` feeds[{position}]");
        let url = url::Url::parse(&self.url)
            .map_err(|error| anyhow::anyhow!("{at} has a url that is not one: {error}"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https"),
            "{at} has scheme `{}`; only http and https feeds are fetched",
            url.scheme()
        );
        anyhow::ensure!(
            url.username().is_empty() && url.password().is_none(),
            "{at} has a url carrying credentials; the url is stored and logged, and a \
             password in it would be too"
        );
        // reqwest does not resolve a literal, so the resolver guard never sees one.
        let private = match url.host() {
            Some(url::Host::Ipv4(ip)) => !notify_domain::is_public_address(ip.into()),
            Some(url::Host::Ipv6(ip)) => !notify_domain::is_public_address(ip.into()),
            _ => false,
        };
        anyhow::ensure!(
            !private || self.allow_private_network,
            "{at} points at a non-public address; set `allow_private_network = true` on \
             this feed if that is meant"
        );
        let every = self.every();
        anyhow::ensure!(
            (Self::MIN_EVERY..=Self::MAX_EVERY).contains(&every),
            "{at} has every = {every:?}; it must be between 5m and 24h"
        );
        // The publish path's own check, so a tag it would refuse fails the load
        // instead of every item of the feed.
        notify_domain::Content::try_from(notify_domain::ContentDraft {
            title: "feed".to_owned(),
            tags: self.tags.clone(),
            ..notify_domain::ContentDraft::default()
        })
        .map_err(|error| anyhow::anyhow!("{at} has tags the publish path refuses: {error}"))?;
        anyhow::ensure!(
            (1..=Self::MAX_MAX_PER_POLL).contains(&self.max_per_poll()),
            "{at} has max_per_poll = {}; it must be between 1 and {}",
            self.max_per_poll(),
            Self::MAX_MAX_PER_POLL
        );
        Ok(url)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelLimits {
    pub publish_per_min: Option<u32>,
}

/// One access binding: exactly one subject, plus the scopes it grants.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Access {
    pub oidc_group: Option<String>,
    pub oidc_sub: Option<String>,
    pub email: Option<String>,
    pub service_account: Option<String>,
    pub scopes: Vec<Scope>,
}

impl Access {
    /// How many subjects this binding names. Anything but one is a config mistake.
    #[must_use]
    pub fn subjects(&self) -> usize {
        [
            self.oidc_group.is_some(),
            self.oidc_sub.is_some(),
            self.email.is_some(),
            self.service_account.is_some(),
        ]
        .into_iter()
        .filter(|named| *named)
        .count()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Publisher {
    pub issuer: String,
    pub subject: String,
    pub scopes: Vec<Scope>,
}

/// One `[[channels.managed.sinks]]` entry (design §7.1bis, §11).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sink {
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
    /// Refuse a sink this build cannot serve, at startup. Security checks: `http(s)` only (no `file:`
    /// write primitive), no URL credentials (they would land in queue rows and logs), webhooks need a secret.
    pub fn validate(&self, channel: &str) -> anyhow::Result<()> {
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

/// `[sinks]` — what outbound sinks need beyond their channel (design §7.1bis): the instance-wide relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Sinks {
    pub smtp: Option<Smtp>,
    /// How long a dead letter is kept before the leader's drain forgets it. It
    /// carries the whole event it failed to deliver, so "for ever" would be a
    /// retention policy nobody chose.
    pub dead_letter_retention: HumanDuration,
    /// The key a webhook's sink id is an HMAC under (`WNK_SINKS__FINGERPRINT_KEY`),
    /// at least [`notify_ports::FingerprintKey::MIN_BYTES`] bytes. Required as soon
    /// as one webhook is configured: the id is stored, listed and backed up, and an
    /// unkeyed digest of a url is one a short token in its query can be guessed
    /// back from (see `notify_ports::redact_url`).
    pub fingerprint_key: Secret,
    /// Keys the ids were fingerprinted under before the current one. Rows still
    /// carrying an id under one of them are re-keyed at boot, so a rotation does
    /// not orphan what was queued; drop a key once a boot has run with it.
    pub previous_fingerprint_keys: Vec<Secret>,
}

impl Default for Sinks {
    fn default() -> Self {
        Self {
            smtp: None,
            dead_letter_retention: HumanDuration::from_secs_const(30 * 24 * 3_600),
            fingerprint_key: Secret::default(),
            previous_fingerprint_keys: Vec::new(),
        }
    }
}

impl Sinks {
    /// `[sinks] fingerprint_key`, as the key it is.
    pub fn fingerprint_key(&self) -> anyhow::Result<notify_ports::FingerprintKey> {
        fingerprint_key(
            "[sinks] fingerprint_key (WNK_SINKS__FINGERPRINT_KEY)",
            &self.fingerprint_key,
        )
    }

    /// `[sinks] previous_fingerprint_keys`, as keys.
    pub fn previous_fingerprint_keys(&self) -> anyhow::Result<Vec<notify_ports::FingerprintKey>> {
        self.previous_fingerprint_keys
            .iter()
            .map(|key| fingerprint_key("an entry of [sinks] previous_fingerprint_keys", key))
            .collect()
    }
}

fn fingerprint_key(what: &str, key: &Secret) -> anyhow::Result<notify_ports::FingerprintKey> {
    notify_ports::FingerprintKey::new(key.expose().as_bytes()).ok_or_else(|| {
        anyhow::anyhow!(
            "{what} must be at least {} bytes: webhook sink ids are an HMAC under it; \
             generate one with `openssl rand -base64 48`",
            notify_ports::FingerprintKey::MIN_BYTES
        )
    })
}

/// `[sinks.smtp]` — the relay the `email` sinks hand their mail to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Smtp {
    /// `smtp://host:port` inside a cluster, `smtps://…` across one.
    pub url: String,
    /// The envelope sender.
    pub from: String,
    /// Relay credentials, when it asks for them.
    pub username: Option<String>,
    /// `env:NAME` or `file:/path`, like every other secret here.
    pub password_ref: Option<String>,
    /// Where this instance is reachable, for the mail's link (design §10.4); no link without it.
    pub permalink_base: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedServiceAccount {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustPolicy {
    pub issuer: String,
    pub subject: String,
    pub channels: Vec<String>,
    pub scopes: Vec<Scope>,
}

/// `[federation]` — signing the CI tokens this instance mints (design §5.3, §11).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Federation {
    /// Path to the Ed25519 private key, mounted from a secret store (§15). Never generated at boot,
    /// or replicas would mint tokens their neighbours cannot verify.
    pub signing_key_file: PathBuf,
    /// The minted tokens' audience; defaults to `weebo-notify`, change it when instances share an ingress.
    pub audience: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Push {
    pub webpush: Option<WebPush>,
    pub apns: Option<Apns>,
    pub unifiedpush: Option<UnifiedPush>,
}

/// `[push.webpush]` — RFC 8292 identification (design §11).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebPush {
    /// Path to the P-256 VAPID private key, mounted from a secret store (§11).
    pub vapid_key_file: PathBuf,
    /// `mailto:…` or `https://…` operator contact (RFC 8292 `sub`); required by relays.
    pub contact: String,
}

/// `[push.apns]` — token-based `.p8` authentication for iOS (design §11, §6.2, §10bis.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Apns {
    /// Path to the `.p8` provider authentication key, mounted from a secret store (§11).
    pub key_file: PathBuf,
    /// The 10-character Key ID Apple assigned this `.p8` key.
    pub key_id: String,
    /// The 10-character Apple Developer Team ID.
    pub team_id: String,
    /// The app's bundle identifier, sent as `apns-topic` on every request.
    pub topic: String,
    /// Push to `api.sandbox.push.apple.com` (debug-signed builds) instead of production.
    #[serde(default)]
    pub sandbox: bool,
}

/// `[push.unifiedpush]` — enables the UnifiedPush transport (design §11, §6.2); empty since there
/// is no server-wide credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UnifiedPush {}

impl Config {
    /// Parse one `config.toml` over the defaults and run [`Config::validate`]: no
    /// `WNK_*` layering, no disk read, and no secret's value looked at — a secret
    /// the file leaves out is not an error here (ADR-0031).
    pub fn from_toml_str(contents: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    /// The configuration a `SIGHUP` may adopt (design §11, T21): `[[channels.managed]]` (with their
    /// sinks and `[sinks.smtp]`, swapped via notify-server's `sinks::SinkRoutes::adopt`), `[[service_accounts]]`,
    /// `[[transforms]]` (via `transforms::TransformRoutes::adopt`) and trust policies (via `notify_app::LivePolicies`). Everything else was wired at boot;
    /// changes there are reported by [`Config::unreloadable`].
    #[must_use]
    pub fn reloaded_from(&self, file: Self) -> Self {
        Self {
            channels: file.channels,
            service_accounts: file.service_accounts,
            transforms: file.transforms,
            trust_policy: file.trust_policy,
            // The relay rides with the sinks: a reload that adds an email sink
            // rebuilds the sink fleet from this, and a fleet built from the old
            // `[sinks.smtp]` would mail through a relay the file no longer names.
            // The fingerprint keys stay as booted: every stored sink id is keyed
            // under them, and only the boot-time re-key moves rows to a new one.
            sinks: Sinks {
                dead_letter_retention: self.sinks.dead_letter_retention,
                fingerprint_key: self.sinks.fingerprint_key.clone(),
                previous_fingerprint_keys: self.sinks.previous_fingerprint_keys.clone(),
                ..file.sinks
            },
            reconcile: file.reconcile,
            ..self.clone()
        }
    }

    /// Sections that differ in the file but a reload cannot adopt (§11), so the operator is told.
    /// Every issuer a trust policy or a channel's publishers name, once each: the
    /// federation verifiers are built for these.
    #[must_use]
    pub fn federation_issuers(&self) -> Vec<String> {
        let global = self.trust_policy.iter().map(|policy| &policy.issuer);
        let scoped = self
            .channels
            .managed
            .iter()
            .flat_map(|channel| channel.publishers.iter())
            .map(|publisher| &publisher.issuer);

        let mut issuers: Vec<String> = Vec::new();
        for issuer in global.chain(scoped) {
            if !issuer.is_empty() && !issuers.contains(issuer) {
                issuers.push(issuer.clone());
            }
        }
        issuers
    }

    #[must_use]
    pub fn unreloadable(&self, file: &Self) -> Vec<String> {
        let mut moved = Vec::new();

        let known = self.federation_issuers();
        if file
            .federation_issuers()
            .iter()
            .any(|issuer| !known.contains(issuer))
        {
            moved.push(
                "a new trust-policy issuer — its token verifier is built at boot; restart"
                    .to_owned(),
            );
        }

        if self.server.bind != file.server.bind {
            moved.push("[server] bind — the socket is bound at boot".to_owned());
        }
        if self.database.url != file.database.url {
            moved.push("[database] url — the pool is connected at boot".to_owned());
        }
        if self.oidc.issuer != file.oidc.issuer || self.oidc.client_id != file.oidc.client_id {
            moved.push("[oidc] — the verifier is built at boot".to_owned());
        }
        if self.admin.oidc_groups != file.admin.oidc_groups {
            moved.push(
                "[admin] oidc_groups — platform admins are wired at boot; restart".to_owned(),
            );
        }
        if self.key_issuers != file.key_issuers {
            moved.push(
                "[[key_issuers]] — the tokens are read at boot; rotate by restarting".to_owned(),
            );
        }
        if self.reload != file.reload {
            moved.push("[reload] — the file watch is started at boot; restart".to_owned());
        }
        if self.auth != file.auth {
            moved.push("[auth] — the authenticator is built at boot".to_owned());
        }
        if self.sinks.dead_letter_retention != file.sinks.dead_letter_retention {
            moved.push("[sinks] dead_letter_retention — the drain is built at boot".to_owned());
        }
        if self.sinks.fingerprint_key != file.sinks.fingerprint_key
            || self.sinks.previous_fingerprint_keys != file.sinks.previous_fingerprint_keys
        {
            moved.push("[sinks] fingerprint_key — stored sink ids are re-keyed at boot".to_owned());
        }

        moved
    }

    /// actix-cors rejects `*` and malformed origins only when a worker starts,
    /// which fails every worker. Refuse here, with the key named.
    fn validate_cors_origins(&self) -> anyhow::Result<()> {
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
        Ok(())
    }

    /// Refuse a configuration this build cannot honour, rather than silently downgrade it.
    ///
    /// Structural only: everything here holds whatever the secrets' values are. The
    /// checks that need them (fingerprint key length, `token_secret`, `env:`/`file:`
    /// references) are notify-server's, after the `WNK_*` layer is merged.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.database.max_connections >= Database::MIN_CONNECTIONS,
            "[database] max_connections = {} is below the {} this build needs: six \
             leader jobs each hold a connection while they lead, and requests need the \
             rest — a smaller pool deadlocks the first time they all lead at once",
            self.database.max_connections,
            Database::MIN_CONNECTIONS
        );
        anyhow::ensure!(
            self.devices.max_per_user > 0,
            "[devices] max_per_user = 0 would refuse every enrolment, and a user with \
             no device receives nothing"
        );
        self.auth.validate()?;
        anyhow::ensure!(
            self.reload.interval.as_secs() >= Reload::MIN_INTERVAL,
            "[reload] interval = {} is under a second; the file would be read in a busy \
             loop, and a ConfigMap update is a minute behind anyway",
            self.reload.interval
        );
        anyhow::ensure!(
            !self.database.acquire_timeout.is_zero(),
            "[database] acquire_timeout = 0 would fail every request the moment the \
             pool is busy; give it a few seconds"
        );
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
        // Refused here rather than left to the database, which refuses it too:
        // there it would fail every purge pass, in a log line, for ever.
        anyhow::ensure!(
            self.retention.audit.is_none_or(|keep| {
                keep.as_duration() >= notify_ports::AUDIT_RETENTION_FLOOR.unsigned_abs()
            }),
            "[retention] audit is under the 90-day floor (ADR-0027); an audit log \
             that forgets last month cannot answer an incident review"
        );

        self.validate_cors_origins()?;

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
                }
            }
        }

        self.validate_smtp()?;

        self.validate_managed_ids()?;
        self.validate_key_issuers()?;
        self.validate_transforms()?;
        for channel in &self.channels.managed {
            // Zero would refuse every publish with a misleading `429`; omit `limits` for the default (§8.2).
            anyhow::ensure!(
                channel
                    .limits
                    .is_none_or(|limits| limits.publish_per_min != Some(0)),
                "channel `{}` sets limits.publish_per_min = 0, which refuses every \
                 publish; omit the key to take the default of design §8.2",
                channel.id
            );
            self.validate_feeds(channel)?;
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

    /// Managed channel and service-account ids parse, and name one thing each. Checked
    /// here so a typo fails the load (and a SIGHUP keeps the running file) rather than
    /// every reconciliation pass after it; `CI` and `ci` would otherwise be one channel.
    fn validate_managed_ids(&self) -> anyhow::Result<()> {
        let mut channels = std::collections::HashSet::new();
        for channel in &self.channels.managed {
            let id = notify_domain::ChannelId::parse(&channel.id).map_err(|error| {
                anyhow::anyhow!("[[channels.managed]] id `{}`: {error}", channel.id)
            })?;
            anyhow::ensure!(
                channels.insert(id),
                "[[channels.managed]] id `{}` is declared twice",
                channel.id
            );
        }
        let mut accounts = std::collections::HashSet::new();
        for account in &self.service_accounts {
            let id = notify_domain::ServiceAccountId::parse(&account.id).map_err(|error| {
                anyhow::anyhow!("[[service_accounts]] id `{}`: {error}", account.id)
            })?;
            anyhow::ensure!(
                accounts.insert(id),
                "[[service_accounts]] id `{}` is declared twice",
                account.id
            );
        }
        Ok(())
    }

    /// Each `[[key_issuers]]` entry on its own, and no id declared twice (ADR-0033).
    fn validate_key_issuers(&self) -> anyhow::Result<()> {
        let mut ids = std::collections::HashSet::new();
        for issuer in &self.key_issuers {
            issuer.validate()?;
            anyhow::ensure!(
                ids.insert(issuer.id.as_str()),
                "[[key_issuers]] id `{}` is declared twice",
                issuer.id
            );
        }
        Ok(())
    }

    /// Every transform compiles on its own (RFC 0003 §4.3), and no id is declared twice.
    fn validate_transforms(&self) -> anyhow::Result<()> {
        let mut ids = std::collections::HashSet::new();
        for transform in &self.transforms {
            transform.compile(notify_transform_cel::Slots::new(1))?;
            anyhow::ensure!(
                ids.insert(transform.id.as_str()),
                "[[transforms]] id `{}` is declared twice",
                transform.id
            );
        }
        Ok(())
    }

    /// A channel's feeds (RFC 0002 §4.3): each on its own, then against the file —
    /// the publisher is declared and may publish here, and no url is listed twice.
    fn validate_feeds(&self, channel: &ManagedChannel) -> anyhow::Result<()> {
        let mut urls = std::collections::HashSet::new();
        for (position, feed) in channel.feeds.iter().enumerate() {
            let url = feed.validate(&channel.id, position)?;
            let at = format!("channel `{}` feeds[{position}]", channel.id);
            anyhow::ensure!(
                urls.insert(url),
                "{at} repeats a url already declared on this channel"
            );
            anyhow::ensure!(
                self.service_accounts
                    .iter()
                    .any(|account| account.id == feed.publisher),
                "{at} names publisher `{}`, which is not a [[service_accounts]] id",
                feed.publisher
            );
            anyhow::ensure!(
                channel.access.iter().any(|access| {
                    access.service_account.as_deref() == Some(feed.publisher.as_str())
                        && access.scopes.contains(&Scope::Publish)
                }),
                "{at} names publisher `{}`, which holds no `publish` grant in this \
                 channel's access; the poller must not publish where its identity could \
                 not through the API",
                feed.publisher
            );
        }
        Ok(())
    }

    /// lettre reads `smtp://host` with no `?tls=` as plaintext, without STARTTLS: relay
    /// credentials need `smtps://` or `?tls=required`.
    fn validate_smtp(&self) -> anyhow::Result<()> {
        if let Some(smtp) = &self.sinks.smtp
            && smtp.username.is_some()
        {
            let encrypted = url::Url::parse(&smtp.url).is_ok_and(|url| {
                url.scheme() == "smtps"
                    || url
                        .query_pairs()
                        .any(|(key, value)| key == "tls" && value == "required")
            });
            anyhow::ensure!(
                encrypted,
                "[sinks.smtp] sends `username` and its password in clear over `{}`; use \
             `smtps://` or `smtp://…?tls=required`",
                smtp.url
            );
        }
        Ok(())
    }

    /// Sections this build parses but does not act on, for the startup log.
    #[must_use]
    pub fn unhonoured(&self) -> Vec<String> {
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

        if self.transform.is_some() {
            pending.push(
                "[[transform]] — the table is `[[transforms]]`; no transform was declared \
                 from it"
                    .to_owned(),
            );
        }

        // Honoured sections (sinks, `limits`, `[push.*]`) are deliberately absent: the
        // composition root reads them, and failures stop the process.

        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(contents: &str) -> anyhow::Result<Config> {
        Config::from_toml_str(contents)
    }

    #[test]
    fn the_defaults_are_a_working_configuration() {
        let config = parse("").unwrap();

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
        // The repository's shipped config must parse.
        let config =
            parse(include_str!("../../../config/dev.toml")).expect("config/dev.toml parses");

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
    fn the_group_snapshot_ttl_defaults_to_a_week_and_takes_an_override_within_bounds() {
        let default = parse("").unwrap();
        assert_eq!(default.auth.group_snapshot_ttl.as_secs(), 7 * 24 * 3_600);

        let file = "[auth]\ngroup_snapshot_ttl = \"1d\"\n";
        assert_eq!(
            parse(file).unwrap().auth.group_snapshot_ttl.as_secs(),
            24 * 3_600
        );

        for value in ["30m", "91d"] {
            let file = format!("[auth]\ngroup_snapshot_ttl = \"{value}\"\n");
            let error = parse(&file).unwrap_err().to_string();
            assert!(error.contains("outside 1h..=90d"), "{value}: {error}");
        }
    }

    #[test]
    fn audit_retention_is_off_by_default_and_never_under_ninety_days() {
        let config = parse("").unwrap();
        assert_eq!(
            config.retention.audit, None,
            "keep for ever unless asked (ADR-0027)"
        );

        let file = "[retention]\naudit = \"365d\"\n";
        assert!(parse(file).unwrap().retention.audit.is_some());

        let file = "[retention]\naudit = \"30d\"\n";
        let error = parse(file).unwrap_err().to_string();
        assert!(error.contains("90-day floor"), "{error}");
    }

    #[test]
    fn a_kubernetes_lease_is_refused_rather_than_silently_downgraded() {
        let file = "[ha]\nleader_election = \"kubernetes\"\n";

        let error = parse(file).unwrap_err().to_string();

        assert!(
            error.contains("not implemented"),
            "two leaders across a cluster is the failure this prevents: {error}"
        );
    }

    #[test]
    fn a_filesystem_attachment_store_without_its_required_fields_is_refused() {
        let file = "[attachments]\nkind = \"filesystem\"\n";

        let error = parse(file).unwrap_err().to_string();

        assert!(
            error.contains("requires `path`"),
            "an unset directory would be a filesystem store with nowhere to write: {error}"
        );
    }

    #[test]
    fn a_complete_filesystem_attachment_store_parses_and_validates() {
        let file = "[attachments]\n\
             kind = \"filesystem\"\n\
             path = \"/data/attachments\"\n\
             public_base_url = \"https://notify.weebo.poc\"\n\
             token_secret = \"correct-horse-battery-staple-and-then-some\"\n";

        let config = parse(file).expect("a complete filesystem config validates");
        let attachments = config.attachments.expect("[attachments] was set");

        assert_eq!(attachments.kind, AttachmentsBackend::Filesystem);
        assert_eq!(attachments.path, "/data/attachments");
    }

    #[test]
    fn a_managed_id_that_cannot_be_one_fails_the_load() {
        for (name, file) in [
            ("bad-channel", "[[channels.managed]]\nid = \"-ci\"\n"),
            (
                "twin-channel",
                "[[channels.managed]]\nid = \"ci\"\n\n[[channels.managed]]\nid = \"CI\"\n",
            ),
            ("bad-account", "[[service_accounts]]\nid = \"-bot\"\n"),
        ] {
            assert!(parse(file).is_err(), "{name} loaded");
        }
    }

    #[test]
    fn smtp_credentials_need_an_encrypted_relay() {
        let relay = |url: &str| {
            format!(
                "[sinks.smtp]\nurl = \"{url}\"\nfrom = \"notify@weebo.poc\"\nusername = \"notify\"\n"
            )
        };

        assert!(parse(&relay("smtp://relay:25")).is_err());
        assert!(parse(&relay("smtp://relay:587?tls=required")).is_ok());
        assert!(parse(&relay("smtps://relay:465")).is_ok());
    }

    #[test]
    fn an_access_binding_must_name_exactly_one_subject() {
        let both = "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             oidc_group = \"dev-team\"\n\
             email = \"alice@weebo.local\"\n\
             scopes = [\"subscribe\"]\n";
        assert!(parse(both).is_err());

        let neither = "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             scopes = [\"subscribe\"]\n";
        assert!(parse(neither).is_err());
    }

    #[test]
    fn a_binding_that_grants_nothing_is_a_mistake_worth_reporting() {
        let file = "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.access]]\n\
             oidc_group = \"dev-team\"\n\
             scopes = []\n";

        assert!(parse(file).is_err());
    }

    #[test]
    fn what_this_build_parses_but_ignores_is_reported_rather_than_dropped() {
        let file = "[[trust_policy]]\n\
             issuer = \"https://forgejo.weebo.poc\"\n\
             subject = \"repo:weebo/*\"\n\
             scopes = [\"publish\"]\n";

        let pending = parse(file).unwrap().unhonoured();

        assert_eq!(
            pending.len(),
            1,
            "a silently ignored key costs an afternoon"
        );
        assert!(pending.iter().any(|line| line.contains("trust_policy")));
    }

    #[test]
    fn a_sink_is_no_longer_reported_as_a_key_this_build_ignores() {
        let file = "[sinks]\n\
             fingerprint_key = \"test-only-fingerprint-key-0123456789\"\n\
             [[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n\
             secret_ref = \"env:WNK_SINK_HMAC\"\n";

        // Warning about read sections would train operators to ignore the list.
        assert!(
            !parse(file)
                .unwrap()
                .unhonoured()
                .iter()
                .any(|line| line.contains("sinks"))
        );
    }

    #[test]
    fn a_webhook_with_no_secret_is_refused_at_startup() {
        let file = "[sinks]\n\
             fingerprint_key = \"test-only-fingerprint-key-0123456789\"\n\
             [[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n";

        // An unsigned webhook is a refusal, not a degraded mode (§7.1bis).
        assert!(parse(file).is_err());
    }

    /// A file declaring one feed, with `feed` lines appended to its table.
    fn feed_config(feed: &str) -> anyhow::Result<Config> {
        parse(&format!(
            "[sinks]\n\
                 fingerprint_key = \"feeds-test-fingerprint-key-0123456789\"\n\
                 [[service_accounts]]\n\
                 id = \"bot-feeds\"\n\
                 [[channels.managed]]\n\
                 id = \"veille\"\n\
                 [[channels.managed.access]]\n\
                 service_account = \"bot-feeds\"\n\
                 scopes = [\"publish\"]\n\
                 [[channels.managed.feeds]]\n\
                 {feed}\n"
        ))
    }

    const FEED: &str = "url = \"https://blog.example.org/feed.xml\"\npublisher = \"bot-feeds\"";

    #[test]
    fn a_feed_takes_its_defaults_and_its_snapshot_skips_an_empty_list() {
        let config = feed_config(FEED).unwrap();
        let feed = &config.channels.managed[0].feeds[0];

        assert_eq!(feed.every(), Feed::DEFAULT_EVERY);
        assert_eq!(feed.max_per_poll(), 10);
        assert!(!feed.allow_private_network);
        assert_eq!(feed.lang, FeedLang::En);
        let bare = serde_json::to_value(ManagedChannel::default()).unwrap();
        assert!(bare.get("feeds").is_none(), "{bare}");
    }

    #[test]
    fn every_refused_feed_names_its_channel_and_position() {
        for (test, extra, expected) in [
            (
                "feed-scheme",
                "url = \"file:///etc/passwd\"\npublisher = \"bot-feeds\"",
                "scheme",
            ),
            (
                "feed-userinfo",
                "url = \"https://me:pw@blog.example.org/feed\"\npublisher = \"bot-feeds\"",
                "credentials",
            ),
            (
                "feed-literal",
                "url = \"http://127.0.0.1:8080/feed\"\npublisher = \"bot-feeds\"",
                "non-public",
            ),
            (
                "feed-metadata",
                "url = \"http://169.254.169.254/latest\"\npublisher = \"bot-feeds\"",
                "non-public",
            ),
            (
                "feed-every-low",
                &format!("{FEED}\nevery = \"1m\""),
                "between 5m and 24h",
            ),
            (
                "feed-every-high",
                &format!("{FEED}\nevery = \"25h\""),
                "between 5m and 24h",
            ),
            (
                "feed-max-zero",
                &format!("{FEED}\nmax_per_poll = 0"),
                "max_per_poll",
            ),
            (
                "feed-max-high",
                &format!("{FEED}\nmax_per_poll = 51"),
                "max_per_poll",
            ),
            (
                "feed-tag-long",
                &format!("{FEED}\ntags = [\"{}\"]", "t".repeat(33)),
                "tags the publish path refuses",
            ),
            (
                "feed-tags-many",
                &format!(
                    "{FEED}\ntags = [{}]",
                    (0..17)
                        .map(|n| format!("\"t{n}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "tags the publish path refuses",
            ),
            (
                "feed-unknown-publisher",
                "url = \"https://blog.example.org/feed\"\npublisher = \"nobody\"",
                "not a [[service_accounts]] id",
            ),
            (
                "feed-twice",
                &format!("{FEED}\n[[channels.managed.feeds]]\n{FEED}"),
                "repeats a url",
            ),
        ] {
            let error = format!("{:#}", feed_config(extra).unwrap_err());
            assert!(error.contains(expected), "{test}: {error}");
            if expected != "repeats a url" {
                assert!(error.contains("`veille` feeds[0]"), "{test}: {error}");
            }
        }
    }

    #[test]
    fn a_private_literal_is_allowed_with_the_opt_in() {
        let config = feed_config(
            "url = \"http://10.0.0.5/feed\"\npublisher = \"bot-feeds\"\nallow_private_network = true",
        );
        assert!(config.is_ok(), "{:?}", config.err());
    }

    #[test]
    fn an_unknown_key_or_language_in_a_feed_is_refused() {
        for (test, extra) in [
            ("feed-unknown-key", format!("{FEED}\ninterval = \"15m\"")),
            ("feed-lang", format!("{FEED}\nlang = \"de\"")),
        ] {
            assert!(feed_config(&extra).is_err(), "{test}");
        }
        let french = feed_config(&format!("{FEED}\nlang = \"fr\"")).unwrap();
        assert_eq!(french.channels.managed[0].feeds[0].lang, FeedLang::Fr);
    }

    #[test]
    fn a_publisher_without_a_publish_grant_is_refused() {
        let file = format!(
            "[sinks]\n\
                 fingerprint_key = \"feeds-test-fingerprint-key-0123456789\"\n\
                 [[service_accounts]]\n\
                 id = \"bot-feeds\"\n\
                 [[channels.managed]]\n\
                 id = \"veille\"\n\
                 [[channels.managed.access]]\n\
                 service_account = \"bot-feeds\"\n\
                 scopes = [\"subscribe\"]\n\
                 [[channels.managed.feeds]]\n\
                 {FEED}\n"
        );

        let error = format!("{:#}", parse(&file).unwrap_err());
        assert!(error.contains("no `publish` grant"), "{error}");
    }

    #[test]
    fn a_fingerprint_key_is_never_printed() {
        let sinks = Sinks {
            fingerprint_key: "do-not-print-this-fingerprint-key-ever".into(),
            ..Sinks::default()
        };

        assert!(!format!("{sinks:?}").contains("do-not-print"));
    }

    #[test]
    fn a_webhook_url_that_would_leak_or_reach_inward_is_refused() {
        for url in [
            // A password would land in queue rows and logs.
            "https://user:hunter2@hooks.weebo.poc/incidents",
            // A write primitive on our own filesystem.
            "file:///etc/passwd",
        ] {
            let file = format!(
                "[sinks]\n\
                     fingerprint_key = \"test-only-fingerprint-key-0123456789\"\n\
                     [[channels.managed]]\n\
                     id = \"ci-infra\"\n\
                     [[channels.managed.sinks]]\n\
                     type = \"webhook\"\n\
                     url = \"{url}\"\n\
                     secret_ref = \"env:WNK_SINK_HMAC\"\n"
            );

            assert!(parse(&file).is_err(), "`{url}` must be refused");
        }
    }

    #[test]
    fn an_email_sink_without_a_relay_is_refused_rather_than_queued_forever() {
        let file = "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"email\"\n\
             address = \"oncall@weebo.poc\"\n";

        assert!(parse(file).is_err());
    }

    #[test]
    fn a_channel_publish_limit_is_parsed_where_the_reconciler_reads_it() {
        // The inline-table form §11 documents.
        let file = "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             limits = { publish_per_min = 120 }\n";

        let config = parse(file).unwrap();

        assert_eq!(
            config.channels.managed[0]
                .limits
                .and_then(|limits| limits.publish_per_min),
            Some(120)
        );
    }

    #[test]
    fn a_publish_limit_of_zero_is_refused_rather_than_read_as_unlimited() {
        // Zero would refuse every publish; omit the key for no override.
        let file = "[[channels.managed]]\n\
             id = \"ci-infra\"\n\
             limits = { publish_per_min = 0 }\n";

        assert!(parse(file).is_err());
    }

    #[test]
    fn a_web_push_key_is_no_longer_reported_as_a_key_this_build_ignores() {
        let file = "[push.webpush]\n\
             vapid_key_file = \"/secrets/vapid.pem\"\n\
             contact = \"mailto:ops@weebo.poc\"\n\
             \n\
             [push.apns]\n\
             key_file = \"/secrets/apns.p8\"\n\
             key_id = \"ABCDE12345\"\n\
             team_id = \"TEAM123456\"\n\
             topic = \"poc.weebo.notify\"\n";

        let pending = parse(file).unwrap().unhonoured();

        // Warning about read sections would train operators to ignore the list.
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
        let file = "[push.unifiedpush]\n\
             \n\
             [push.apns]\n\
             key_file = \"/secrets/apns.p8\"\n\
             key_id = \"ABCDE12345\"\n\
             team_id = \"TEAM123456\"\n\
             topic = \"poc.weebo.notify\"\n";

        let pending = parse(file).unwrap().unhonoured();

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
        let file = "[cli]\n\
             binary_path = \"/usr/local/bin/notify\"\n";

        let config = parse(file).unwrap();

        assert_eq!(
            config.cli.unwrap().binary_path,
            std::path::PathBuf::from("/usr/local/bin/notify")
        );
    }

    #[test]
    fn no_cli_section_means_the_download_route_has_nothing_to_serve() {
        let config = parse("").unwrap();

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

        // Applied to the database, so re-reading is enough (§11).
        let reloaded = running.reloaded_from(file);

        assert_eq!(reloaded.channels.managed.len(), 1);
        assert_eq!(reloaded.service_accounts.len(), 1);
    }

    const NEWS: &str = r#"
[[transforms]]
id = "mlab-news"
channels = ["security"]
split = 'body.items'
title = 'item.title'
external_id = 'item.id'
"#;

    #[test]
    fn a_transform_loads_and_a_reload_adopts_it() {
        let config = parse(NEWS).unwrap();
        assert_eq!(config.transforms.len(), 1);
        assert_eq!(config.unhonoured(), [] as [std::string::String; 0]);

        let reloaded = Config::default().reloaded_from(config);
        assert_eq!(reloaded.transforms.len(), 1);
    }

    #[test]
    fn a_transform_that_would_not_work_fails_the_load() {
        for (name, contents, why) in [
            (
                "transform-unknown-key",
                format!("{NEWS}raw_body = true\n"),
                "raw_body",
            ),
            ("transform-twice", format!("{NEWS}{NEWS}"), "declared twice"),
            (
                "transform-bad-channel",
                NEWS.replace(r#"["security"]"#, r#"["Not A Channel"]"#),
                "channels",
            ),
            (
                "transform-unquoted",
                format!("{NEWS}group_key = 'mlab-news'\n"),
                "CEL quotes",
            ),
            (
                "transform-no-title",
                NEWS.replace("title = 'item.title'\n", ""),
                "title",
            ),
            ("transform-raw", format!("{NEWS}raw = \"all\"\n"), "raw"),
        ] {
            let error = parse(&contents).unwrap_err();
            assert!(format!("{error:#}").contains(why), "{name}: {error:#}");
        }
    }

    #[test]
    fn a_misspelt_transforms_table_is_reported() {
        let config = parse(&NEWS.replace("[[transforms]]", "[[transform]]")).unwrap();
        assert!(config.transforms.is_empty());
        assert!(
            config
                .unhonoured()
                .iter()
                .any(|pending| pending.contains("[[transform]]"))
        );
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

        // Trust policies are adopted via `LivePolicies`, so file, process and journal agree.
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
                url: "postgres://elsewhere/db".into(),
                ..Database::default()
            },
            ..Config::default()
        };

        let moved = running.unreloadable(&file);

        assert!(
            moved.iter().any(|line| line.contains("database")),
            "{moved:?}"
        );
        // Trust policies are adopted, so they must not warn — but a new issuer needs a
        // verifier only boot builds, and a token from it would be refused until then.
        assert!(
            !moved.iter().any(|line| line.contains("trust_policy")),
            "{moved:?}"
        );
        assert!(
            moved.iter().any(|line| line.contains("issuer")),
            "{moved:?}"
        );

        // A changed policy on a known issuer is adopted silently.
        let known = Config {
            trust_policy: file.trust_policy.clone(),
            ..Config::default()
        };
        assert!(
            !known
                .unreloadable(&file)
                .iter()
                .any(|line| line.contains("issuer"))
        );
    }

    #[test]
    fn a_reload_adopts_the_sinks_and_their_relay() {
        let running = Config::default();
        let file = Config {
            sinks: Sinks {
                smtp: Some(Smtp {
                    url: "smtp://relay:25".to_owned(),
                    ..Smtp::default()
                }),
                ..Sinks::default()
            },
            ..Config::default()
        };

        let reloaded = running.reloaded_from(file.clone());

        assert!(
            reloaded.sinks.smtp.is_some(),
            "a new relay rides with the sinks"
        );
        assert!(
            running.unreloadable(&file).is_empty(),
            "and nothing is reported as stuck"
        );
    }

    #[test]
    fn a_pool_too_small_for_the_leader_jobs_is_refused_by_name() {
        let file = "[database]\nmax_connections = 4\n";

        let error = parse(file).unwrap_err().to_string();

        assert!(error.contains("max_connections"), "{error}");
        assert!(
            parse(&format!(
                "[database]\nmax_connections = {}\n",
                Database::MIN_CONNECTIONS
            ))
            .is_ok()
        );
    }

    #[test]
    fn secrets_never_print_in_a_debug_of_the_configuration() {
        let config = Config {
            database: Database {
                url: "postgres://notify:hunter2@db/notify".into(),
                ..Database::default()
            },
            attachments: Some(Attachments {
                access_key: "AKIAEXAMPLE".into(),
                secret_key: "s3-hunter2".into(),
                token_secret: "fs-hunter2".into(),
                ..Attachments::default()
            }),
            ..Config::default()
        };

        let printed = format!("{config:?}");

        for secret in ["hunter2", "AKIAEXAMPLE"] {
            assert!(!printed.contains(secret), "{secret} leaked: {printed}");
        }
        assert!(printed.contains("[redacted]"));
        assert_eq!(
            config.database.url.expose(),
            "postgres://notify:hunter2@db/notify"
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

        // A no-op SIGHUP must be quiet.
        assert_eq!(
            running.unreloadable(&running.clone()),
            [] as [std::string::String; 0]
        );
    }

    #[test]
    fn a_secret_the_file_leaves_out_is_not_a_structural_error() {
        // What the operator renders: the webhook, the feed and the filesystem store
        // are complete, and every secret arrives later as `WNK_*` (ADR-0031).
        let config = parse(&format!(
            "[attachments]\n\
             kind = \"filesystem\"\n\
             path = \"/data/attachments\"\n\
             public_base_url = \"https://notify.weebo.poc\"\n\
             [[service_accounts]]\n\
             id = \"bot-feeds\"\n\
             [[channels.managed]]\n\
             id = \"veille\"\n\
             [[channels.managed.access]]\n\
             service_account = \"bot-feeds\"\n\
             scopes = [\"publish\"]\n\
             [[channels.managed.feeds]]\n\
             {FEED}\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n\
             secret_ref = \"env:WNK_SINK_HMAC\"\n"
        ));
        assert!(config.is_ok(), "{:#}", config.unwrap_err());
    }

    #[test]
    fn a_change_to_the_platform_admins_needs_a_restart() {
        let running = Config::default();
        let file = Config {
            admin: Admin {
                oidc_groups: vec!["platform-admins".to_owned()],
            },
            ..Config::default()
        };

        let moved = running.unreloadable(&file);

        assert!(
            moved
                .iter()
                .any(|line| line.contains("[admin] oidc_groups")),
            "{moved:?}"
        );
        assert_eq!(
            running.reloaded_from(file).admin.oidc_groups,
            [] as [String; 0],
            "and the reload keeps the groups it booted with"
        );
    }

    #[test]
    fn the_file_is_watched_every_five_seconds_unless_told_otherwise() {
        let reload = parse("").unwrap().reload;
        assert!(reload.watch);
        assert_eq!(reload.interval.as_secs(), 5);

        let reload = parse("[reload]\nwatch = false\ninterval = \"1m\"\n")
            .unwrap()
            .reload;
        assert!(!reload.watch);
        assert_eq!(reload.interval.as_secs(), 60);
    }

    #[test]
    fn a_watch_under_a_second_is_refused() {
        // Even switched off: the server builds its interval either way, and tokio
        // refuses a zero period.
        for file in [
            "[reload]\ninterval = \"0s\"\n",
            "[reload]\nwatch = false\ninterval = \"0s\"\n",
        ] {
            let error = parse(file).unwrap_err().to_string();
            assert!(error.contains("[reload] interval"), "{error}");
        }
        assert!(parse("[reload]\ninterval = \"1s\"\n").is_ok());
    }

    #[test]
    fn a_change_to_the_watch_needs_a_restart() {
        let running = Config::default();
        let file = Config {
            reload: Reload {
                watch: false,
                ..Reload::default()
            },
            ..Config::default()
        };

        assert!(
            running
                .unreloadable(&file)
                .iter()
                .any(|line| line.contains("[reload]"))
        );
        assert!(running.reloaded_from(file).reload.watch);
    }

    #[test]
    fn a_key_issuer_names_its_token_and_never_holds_it() {
        let config = parse(
            "[[key_issuers]]\nid = \"weebo-operator\"\ntoken = \"env:WNK_KEY_ISSUER\"\n\
             [[key_issuers]]\nid = \"backup\"\ntoken = \"file:/var/run/secrets/issuer\"\n",
        )
        .unwrap();
        assert_eq!(config.key_issuers.len(), 2);

        for (file, why) in [
            (
                "[[key_issuers]]\nid = \"op\"\ntoken = \"wki_op_0123456789\"\n",
                "env:NAME",
            ),
            (
                "[[key_issuers]]\nid = \"op\"\ntoken = \"env:\"\n",
                "env:NAME",
            ),
            (
                "[[key_issuers]]\nid = \"op\"\ntoken = \"env:A\"\n\
                 [[key_issuers]]\nid = \"op\"\ntoken = \"env:B\"\n",
                "declared twice",
            ),
            (
                "[[key_issuers]]\nid = \"my_op\"\ntoken = \"env:A\"\n",
                "lowercase letters",
            ),
            (
                "[[key_issuers]]\nid = \"op\"\ntoken = \"env:A\"\nsecret = \"x\"\n",
                "unknown field",
            ),
        ] {
            let error = format!("{:#}", parse(file).unwrap_err());
            assert!(error.contains(why), "{file}: {error}");
        }
    }

    #[test]
    fn a_change_to_the_key_issuers_needs_a_restart() {
        let running = Config::default();
        let file = Config {
            key_issuers: vec![KeyIssuer {
                id: "op".to_owned(),
                token: "env:A".to_owned(),
            }],
            ..Config::default()
        };

        assert!(
            running
                .unreloadable(&file)
                .iter()
                .any(|line| line.contains("[[key_issuers]]"))
        );
        assert_eq!(running.reloaded_from(file).key_issuers, []);
    }
}
