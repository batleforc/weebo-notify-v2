//! Configuration: defaults < TOML file < `WNK_*` environment (design §11).
//! Every key is overridable as `WNK_` + path joined by `__` (e.g. `WNK_OIDC__ISSUER`).
//! Parsed-but-unhonoured sections are reported at startup by [`Config::unhonoured`].

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
    pub auth: Auth,
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
}

/// `[devices]` — the ceiling on enrolment.
///
/// Every active device is a credential the owner's notifications go to and a push
/// the fan-out pays for, so a leaked session must not be able to enrol them
/// without end. Counts push and stream devices together; a revoked one frees its
/// slot. Past it, `POST /devices` answers `422 device.limit_reached`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Devices {
    /// The most active devices one owner may hold.
    pub max_per_user: u32,
}

impl Default for Devices {
    fn default() -> Self {
        Self {
            max_per_user: notify_app::DEFAULT_MAX_DEVICES_PER_USER,
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
pub(crate) struct Secret(String);

impl Secret {
    /// The value, for the one call that needs it. Named so every use is a grep.
    pub(crate) fn expose(&self) -> &str {
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
pub(crate) struct Reconcile {
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
pub(crate) struct Cli {
    /// Path to the binary, built from the same commit as `notify-server`.
    pub binary_path: PathBuf,
}

/// Where the OTLP exporter sends, and what it calls this process (design §13).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Telemetry {
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
pub(crate) struct Server {
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
pub(crate) struct Database {
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
    /// Five leader jobs (purge, stale closure, DND digest, sink drain, reconcile)
    /// each hold one connection for as long as they lead — the advisory lock lives
    /// on it — and a replica can lead all five at once. Four more is the least that
    /// still serves requests (and lets those jobs do their own queries) while they
    /// do. The `LISTEN` connection is taken from the pool and detached, so it is
    /// not counted against it.
    pub(crate) const MIN_CONNECTIONS: u32 = 5 + 4;

    /// What the persistence adapter is handed.
    pub(crate) const fn pool(&self) -> notify_persistence_pg::PoolSettings {
        notify_persistence_pg::PoolSettings {
            max_connections: self.max_connections,
            acquire_timeout: self.acquire_timeout.as_duration(),
            idle_timeout: self.idle_timeout.as_duration(),
            max_lifetime: self.max_lifetime.as_duration(),
            statement_timeout: self.statement_timeout.as_duration(),
            idle_in_transaction_timeout: self.idle_in_transaction_timeout.as_duration(),
        }
    }
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
pub(crate) struct Oidc {
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
pub(crate) struct Auth {
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
            group_snapshot_ttl: HumanDuration::from_secs_const(
                notify_app::DEFAULT_GROUP_SNAPSHOT_TTL
                    .unsigned_abs()
                    .as_secs(),
            ),
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
            #[allow(
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
pub(crate) enum LeaderBackend {
    /// A Postgres advisory lock. Zero extra infrastructure.
    #[default]
    Postgres,
    /// The Kubernetes Lease API; not implemented, so refused at startup.
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
    /// Redis pub/sub; no adapter exists, so refused at startup with a reason.
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
    /// A local directory served through this server's own routes, for deployments without S3 (ADR-0022).
    Filesystem,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Attachments {
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
pub(crate) enum OnRemoved {
    /// Leave the rows in place and stop reconciling it; the safe default, as deletion takes history.
    #[default]
    Archive,
    /// Delete it, history included.
    Delete,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Channels {
    pub on_removed: OnRemoved,
    /// `sighup` — reload without a restart (§11).
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
    /// Refuse a sink this build cannot serve, at startup. Security checks: `http(s)` only (no `file:`
    /// write primitive), no URL credentials (they would land in queue rows and logs), webhooks need a secret.
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

/// Resolve an `env:NAME` / `file:/path` reference (design §11); inline secrets are refused.
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
        // Trimmed: `echo` into a file adds a newline that would poison the HMAC.
        return Ok(contents.trim().to_owned());
    }

    anyhow::bail!("{what} must be `env:NAME` or `file:/path`, not `{reference}`")
}

/// `[sinks]` — what outbound sinks need beyond their channel (design §7.1bis): the instance-wide relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Sinks {
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
    pub(crate) fn fingerprint_key(&self) -> anyhow::Result<notify_ports::FingerprintKey> {
        fingerprint_key(
            "[sinks] fingerprint_key (WNK_SINKS__FINGERPRINT_KEY)",
            &self.fingerprint_key,
        )
    }

    /// `[sinks] previous_fingerprint_keys`, as keys.
    pub(crate) fn previous_fingerprint_keys(
        &self,
    ) -> anyhow::Result<Vec<notify_ports::FingerprintKey>> {
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
pub(crate) struct Smtp {
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
    /// Path to the Ed25519 private key, mounted from a secret store (§15). Never generated at boot,
    /// or replicas would mint tokens their neighbours cannot verify.
    pub signing_key_file: PathBuf,
    /// The minted tokens' audience; defaults to `weebo-notify`, change it when instances share an ingress.
    pub audience: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Push {
    pub webpush: Option<WebPush>,
    pub apns: Option<Apns>,
    pub unifiedpush: Option<UnifiedPush>,
}

/// `[push.webpush]` — RFC 8292 identification (design §11).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WebPush {
    /// Path to the P-256 VAPID private key, mounted from a secret store (§11).
    pub vapid_key_file: PathBuf,
    /// `mailto:…` or `https://…` operator contact (RFC 8292 `sub`); required by relays.
    pub contact: String,
}

/// `[push.apns]` — token-based `.p8` authentication for iOS (design §11, §6.2, §10bis.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Apns {
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
pub(crate) struct UnifiedPush {}

impl Config {
    /// Read the layered configuration; a missing file is fine (env-only deployments are supported).
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        if path.exists() {
            figment = figment.merge(Toml::file(path));
        } else {
            tracing::warn!(path = %path.display(), "no configuration file; using defaults and WNK_*");
        }

        // `split("__")` maps `WNK_OIDC__ISSUER` to `[oidc] issuer`, for every key (§11).
        let config: Self = figment
            .merge(Env::prefixed(ENV_PREFIX).split("__"))
            .extract()?;

        config.validate()?;
        Ok(config)
    }

    /// The configuration a `SIGHUP` may adopt (design §11, T21): `[[channels.managed]]` (with their
    /// sinks and `[sinks.smtp]`, swapped via `crate::sinks::SinkRoutes::adopt`), `[[service_accounts]]`
    /// and trust policies (via `notify_app::LivePolicies`). Everything else was wired at boot;
    /// changes there are reported by [`Config::unreloadable`].
    pub(crate) fn reloaded_from(&self, file: Self) -> Self {
        Self {
            channels: file.channels,
            service_accounts: file.service_accounts,
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
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.database.max_connections >= Database::MIN_CONNECTIONS,
            "[database] max_connections = {} is below the {} this build needs: five \
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
                    anyhow::ensure!(
                        !attachments.token_secret.expose().is_empty(),
                        "[attachments] kind = \"filesystem\" requires `token_secret` \
                         — it signs the URLs presign hands out, and an empty one would \
                         mean an unsigned, open directory"
                    );
                }
            }
        }

        self.validate_fingerprint_keys()?;

        for channel in &self.channels.managed {
            anyhow::ensure!(
                !channel.id.is_empty(),
                "a [[channels.managed]] entry has no id"
            );
            // Zero would refuse every publish with a misleading `429`; omit `limits` for the default (§8.2).
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

    /// `[sinks] fingerprint_key` and its predecessors.
    ///
    /// Checked whenever they are set, and the current one required once a webhook
    /// needs it: a previous key is only ever read to re-key, so a typo in one would
    /// orphan exactly the rows it was kept for.
    fn validate_fingerprint_keys(&self) -> anyhow::Result<()> {
        self.sinks.previous_fingerprint_keys()?;
        if self
            .channels
            .managed
            .iter()
            .flat_map(|channel| &channel.sinks)
            .any(|sink| sink.kind == "webhook")
        {
            self.sinks.fingerprint_key().map_err(|error| {
                error
                    .context("a webhook sink is configured, so [sinks] fingerprint_key is required")
            })?;
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

        // Honoured sections (sinks, `limits`, `[push.*]`) are deliberately absent: the
        // composition root reads them, and failures stop the process.

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
        // The repository's shipped config must parse.
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

    /// Every scalar leaf of a serialised config, as `(path, value)` (T23); sequences are GitOps material.
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
            // `null` is an absent optional section.
            serde_json::Value::Null | serde_json::Value::Array(_) => {}
            scalar => found.push((prefix.clone(), scalar.clone())),
        }
    }

    #[test]
    fn critical_t23_every_configuration_key_can_be_set_from_the_environment() {
        // T23 (§3.4): enumerate every key, from `config/dev.toml` since defaults leave most sections empty.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml");
        let mut leaves = Vec::new();
        scalar_leaves(
            &serde_json::to_value(load_isolated(&base).unwrap()).unwrap(),
            &mut Vec::new(),
            &mut leaves,
        );

        // A floor, so the test fails when the walk stops walking.
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

        // Count keys taken, or all-refused would pass.
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

                // Taken, or refused by name: both prove the variable arrived; silence is the failure.
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

        // Ensure some keys are actually taken, not only refused by name.
        assert!(
            taken * 2 > total,
            "only {taken} of {total} keys were actually taken from the environment; \
             the rest merely produced an error naming themselves"
        );
    }

    #[test]
    fn traces_can_be_switched_on_by_environment_alone() {
        // A whole optional section from the environment: figment must build the `Some`.
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
    fn the_group_snapshot_ttl_defaults_to_a_week_and_takes_an_override_within_bounds() {
        let default = load_isolated(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(default.auth.group_snapshot_ttl.as_secs(), 7 * 24 * 3_600);

        let path = with_config("auth-ttl", "[auth]\ngroup_snapshot_ttl = \"1d\"\n");
        assert_eq!(
            load_isolated(&path)
                .unwrap()
                .auth
                .group_snapshot_ttl
                .as_secs(),
            24 * 3_600
        );

        for (test, value) in [("auth-ttl-short", "30m"), ("auth-ttl-long", "91d")] {
            let path = with_config(test, &format!("[auth]\ngroup_snapshot_ttl = \"{value}\"\n"));
            let error = load_isolated(&path).unwrap_err().to_string();
            assert!(error.contains("outside 1h..=90d"), "{value}: {error}");
        }
    }

    #[test]
    fn audit_retention_is_off_by_default_and_never_under_ninety_days() {
        let config = load_isolated(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(
            config.retention.audit, None,
            "keep for ever unless asked (ADR-0027)"
        );

        let path = with_config("audit-keep", "[retention]\naudit = \"365d\"\n");
        assert!(load_isolated(&path).unwrap().retention.audit.is_some());

        let path = with_config("audit-short", "[retention]\naudit = \"30d\"\n");
        let error = load_isolated(&path).unwrap_err().to_string();
        assert!(error.contains("90-day floor"), "{error}");
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
            "[sinks]\n\
             fingerprint_key = \"test-only-fingerprint-key-0123456789\"\n\
             [[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n\
             secret_ref = \"env:WNK_SINK_HMAC\"\n",
        );

        // Warning about read sections would train operators to ignore the list.
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
            "[sinks]\n\
             fingerprint_key = \"test-only-fingerprint-key-0123456789\"\n\
             [[channels.managed]]\n\
             id = \"ci-infra\"\n\
             [[channels.managed.sinks]]\n\
             type = \"webhook\"\n\
             url = \"https://hooks.weebo.poc/incidents\"\n",
        );

        // An unsigned webhook is a refusal, not a degraded mode (§7.1bis).
        assert!(load_isolated(&path).is_err());
    }

    #[test]
    fn a_webhook_without_a_fingerprint_key_of_full_length_is_refused_at_startup() {
        for (name, sinks) in [
            ("unkeyed-webhook", ""),
            (
                "short-keyed-webhook",
                "[sinks]\nfingerprint_key = \"too-short\"\n",
            ),
        ] {
            let path = with_config(
                name,
                &format!(
                    "{sinks}[[channels.managed]]\n\
                     id = \"ci-infra\"\n\
                     [[channels.managed.sinks]]\n\
                     type = \"webhook\"\n\
                     url = \"https://hooks.weebo.poc/incidents\"\n\
                     secret_ref = \"env:WNK_SINK_HMAC\"\n"
                ),
            );

            // No fallback to an unkeyed (brute-forceable) id; the message names the key and its env var.
            let error = format!("{:#}", load_isolated(&path).unwrap_err());
            assert!(error.contains("[sinks] fingerprint_key"), "{error}");
            assert!(error.contains("WNK_SINKS__FINGERPRINT_KEY"), "{error}");
        }
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
            let path = with_config(
                "bad-webhook",
                &format!(
                    "[sinks]\n\
                     fingerprint_key = \"test-only-fingerprint-key-0123456789\"\n\
                     [[channels.managed]]\n\
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
        // The inline-table form §11 documents.
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
        // Zero would refuse every publish; omit the key for no override.
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

        // Applied to the database, so re-reading is enough (§11).
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
        // Trust policies are adopted, so they must not warn.
        assert!(
            !moved.iter().any(|line| line.contains("trust_policy")),
            "{moved:?}"
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
        let path = with_config("tiny-pool", "[database]\nmax_connections = 4\n");

        let error = load_isolated(&path).unwrap_err().to_string();

        assert!(error.contains("max_connections"), "{error}");
        assert!(
            load_isolated(&with_config(
                "enough-pool",
                &format!(
                    "[database]\nmax_connections = {}\n",
                    Database::MIN_CONNECTIONS
                )
            ))
            .is_ok()
        );
    }

    #[test]
    fn the_database_defaults_migrate_at_boot_and_bound_every_wait() {
        let database = load_isolated(Path::new("/nonexistent/config.toml"))
            .unwrap()
            .database;

        assert!(database.migrate_on_start);
        let pool = database.pool();
        assert_eq!(pool.acquire_timeout.as_secs(), 5);
        assert_eq!(pool.statement_timeout.as_secs(), 30);
        assert_eq!(pool.idle_in_transaction_timeout.as_secs(), 60);
    }

    #[test]
    fn migrations_can_be_left_to_the_subcommand_from_the_environment() {
        #[expect(
            clippy::result_large_err,
            reason = "the signature belongs to figment::Jail"
        )]
        figment::Jail::expect_with(|jail| {
            jail.set_env("WNK_DATABASE__MIGRATE_ON_START", "false");
            let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
            assert!(!config.database.migrate_on_start);
            Ok(())
        });
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
}
